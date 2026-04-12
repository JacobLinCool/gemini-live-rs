//! Host-facing harness controller that fixes the harness/host boundary.
//!
//! Concrete hosts should not manually compose one object for durable tools and
//! another object for passive notification delivery. Both concerns belong to
//! the same harness layer. `HarnessController` keeps that boundary explicit by
//! bundling:
//!
//! - the harness-owned tool execution wrapper
//! - the durable passive notification pump
//! - in-flight host tool execution lifecycle and completion normalization
//! - startup reconciliation for stale background tasks from an older runtime
//!
//! Hosts remain responsible for product-specific policy such as "when is it
//! safe to inject a notification?" and "how should a notification prompt be
//! sent back to the model?". The harness controller owns everything else.

use gemini_live::types::{FunctionCallRequest, FunctionResponse, Tool};
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{AbortHandle, JoinHandle};

use crate::delivery::{PassiveNotificationDelivery, PassiveNotificationPump};
use crate::error::HarnessError;
use crate::executor::{HarnessToolBudget, HarnessToolRuntime};
use crate::store::Harness;
use crate::{NoopToolSource, ToolDescriptor, ToolExecutionError, ToolExecutor, ToolProvider};

struct InFlightToolCallDispatch {
    abort_handle: AbortHandle,
}

/// High-level result class for one host-side tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessToolCompletionDisposition {
    Responded,
    Failed,
    Cancelled,
}

/// Result of one host-side tool call spawned by the harness controller.
pub struct HarnessToolCompletion {
    pub call_id: String,
    pub call_name: String,
    pub result: Result<FunctionResponse, ToolExecutionError>,
}

impl HarnessToolCompletion {
    pub fn disposition(&self) -> HarnessToolCompletionDisposition {
        match &self.result {
            Ok(_) => HarnessToolCompletionDisposition::Responded,
            Err(ToolExecutionError::Cancelled { .. }) => {
                HarnessToolCompletionDisposition::Cancelled
            }
            Err(_) => HarnessToolCompletionDisposition::Failed,
        }
    }

    pub fn into_runtime_response(self) -> Option<FunctionResponse> {
        match self.result {
            Ok(response) => Some(response),
            Err(ToolExecutionError::Cancelled { .. }) => None,
            Err(error) => Some(FunctionResponse {
                id: self.call_id,
                name: self.call_name,
                response: json!({
                    "ok": false,
                    "error": {
                        "message": error.to_string(),
                    },
                }),
            }),
        }
    }
}

/// Host-facing harness boundary above the runtime layer.
pub struct HarnessController<A = NoopToolSource> {
    tool_runtime: HarnessToolRuntime<A>,
    notification_pump: PassiveNotificationPump,
    in_flight_tool_dispatches: Arc<Mutex<HashMap<String, InFlightToolCallDispatch>>>,
}

impl<A> Clone for HarnessController<A> {
    fn clone(&self) -> Self {
        Self {
            tool_runtime: self.tool_runtime.clone(),
            notification_pump: self.notification_pump.clone(),
            in_flight_tool_dispatches: Arc::clone(&self.in_flight_tool_dispatches),
        }
    }
}

impl HarnessController<NoopToolSource> {
    pub fn new(harness: Harness) -> Result<Self, HarnessError> {
        let controller = Self {
            tool_runtime: HarnessToolRuntime::new(harness.clone()),
            notification_pump: PassiveNotificationPump::new(harness),
            in_flight_tool_dispatches: Arc::new(Mutex::new(HashMap::new())),
        };
        controller.reconcile_stale_running_tasks()?;
        Ok(controller)
    }

    pub fn open_default() -> Result<Self, HarnessError> {
        Self::new(Harness::open_default()?)
    }
}

impl<A> HarnessController<A>
where
    A: ToolProvider + ToolExecutor,
{
    pub fn with_host_tools(harness: Harness, host_tools: A) -> Result<Self, HarnessError> {
        let controller = Self {
            tool_runtime: HarnessToolRuntime::with_host_tools(harness.clone(), host_tools)?,
            notification_pump: PassiveNotificationPump::new(harness),
            in_flight_tool_dispatches: Arc::new(Mutex::new(HashMap::new())),
        };
        controller.reconcile_stale_running_tasks()?;
        Ok(controller)
    }

    pub fn with_budget(mut self, budget: HarnessToolBudget) -> Self {
        self.tool_runtime = self.tool_runtime.with_budget(budget);
        self
    }

    pub fn harness(&self) -> &Harness {
        self.notification_pump.harness()
    }

    pub fn runtime_owner(&self) -> &crate::TaskRuntimeInstance {
        self.tool_runtime.runtime_owner()
    }

    pub fn advertised_tools(&self) -> Option<Vec<Tool>> {
        self.tool_runtime.advertised_tools()
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tool_runtime.descriptors()
    }

    pub async fn execute(
        &self,
        call: FunctionCallRequest,
    ) -> Result<FunctionResponse, ToolExecutionError> {
        self.tool_runtime.execute(call).await
    }

    pub fn spawn_tool_call(
        &self,
        call: FunctionCallRequest,
        completions: UnboundedSender<HarnessToolCompletion>,
    ) {
        let call_id = call.id.clone();
        let controller = self.clone();
        let abort_handle = tokio::spawn(async move {
            let call_name = call.name.clone();
            let result = controller.execute(call.clone()).await;
            controller.remove_in_flight_tool_dispatch(&call.id);
            let _ = completions.send(HarnessToolCompletion {
                call_id: call.id,
                call_name,
                result,
            });
        })
        .abort_handle();
        self.insert_in_flight_tool_dispatch(call_id, abort_handle);
    }

    pub fn cancel_tool_call(&self, call_id: &str) -> bool {
        let cancelled = self.tool_runtime.cancel(call_id);
        if let Some(dispatch) = self.remove_in_flight_tool_dispatch(call_id) {
            dispatch.abort_handle.abort();
            return true;
        }
        cancelled
    }

    pub fn abort_all_tool_calls(&self) {
        let dispatches = self
            .in_flight_tool_dispatches
            .lock()
            .expect("controller tool dispatch lock")
            .drain()
            .map(|(_, dispatch)| dispatch)
            .collect::<Vec<_>>();
        for dispatch in dispatches {
            dispatch.abort_handle.abort();
        }
    }

    pub fn current_in_flight_notification_id(&self) -> Option<String> {
        self.notification_pump.current_in_flight_notification_id()
    }

    pub fn passive_notification_queue_version(&self) -> u64 {
        self.notification_pump.queue_version()
    }

    pub async fn wait_for_passive_notification_signal(&self, observed_queue_version: u64) {
        self.notification_pump
            .wait_for_signal_since(observed_queue_version)
            .await;
    }

    pub fn notify_passive_notification_gate_changed(&self) {
        self.notification_pump.notify_delivery_gate_changed();
    }

    pub fn recover_orphaned_deliveries(&self) -> Result<usize, HarnessError> {
        Ok(self.notification_pump.recover_orphaned_deliveries()?.len())
    }

    pub fn acknowledge_in_flight_notification(&self) -> Result<bool, HarnessError> {
        Ok(self.notification_pump.acknowledge_in_flight()?.is_some())
    }

    pub fn requeue_in_flight_notification(&self) -> Result<bool, HarnessError> {
        Ok(self.notification_pump.requeue_in_flight()?.is_some())
    }

    pub async fn dispatch_passive_notification_once<D, DFut, E>(
        &self,
        deliver: D,
    ) -> Result<(), HarnessError>
    where
        D: FnOnce(PassiveNotificationDelivery) -> DFut + Send,
        DFut: Future<Output = Result<(), E>> + Send,
        E: std::fmt::Display + Send,
    {
        self.notification_pump.dispatch_once(deliver).await
    }

    pub fn spawn_passive_notification_loop<C, CFut, D, DFut, E>(
        &self,
        can_deliver: C,
        deliver: D,
    ) -> JoinHandle<()>
    where
        C: Fn() -> CFut + Send + Sync + 'static,
        CFut: Future<Output = bool> + Send + 'static,
        D: Fn(PassiveNotificationDelivery) -> DFut + Send + Sync + 'static,
        DFut: Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        self.notification_pump.spawn(can_deliver, deliver)
    }

    fn insert_in_flight_tool_dispatch(&self, call_id: String, abort_handle: AbortHandle) {
        let previous = self
            .in_flight_tool_dispatches
            .lock()
            .expect("controller tool dispatch lock")
            .insert(call_id, InFlightToolCallDispatch { abort_handle });
        if let Some(previous) = previous {
            previous.abort_handle.abort();
        }
    }

    fn remove_in_flight_tool_dispatch(&self, call_id: &str) -> Option<InFlightToolCallDispatch> {
        self.in_flight_tool_dispatches
            .lock()
            .expect("controller tool dispatch lock")
            .remove(call_id)
    }

    fn reconcile_stale_running_tasks(&self) -> Result<(), HarnessError> {
        let _ = self
            .harness()
            .interrupt_stale_running_tasks(&self.runtime_owner().instance_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use gemini_live::types::{FunctionCallRequest, FunctionDeclaration, FunctionResponse, Tool};
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        NewRunningTask, TaskRuntimeInstance, TaskStatus, ToolCapability, ToolKind,
        ToolSpecification,
    };

    #[derive(Clone, Copy)]
    struct SleepToolSource {
        delay: Duration,
    }

    impl ToolProvider for SleepToolSource {
        fn advertised_tools(&self) -> Option<Vec<Tool>> {
            Some(vec![Tool::FunctionDeclarations(vec![
                FunctionDeclaration {
                    name: "sleep_tool".into(),
                    description: "sleep".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {}
                    }),
                    scheduling: None,
                    behavior: None,
                },
            ])])
        }

        fn descriptors(&self) -> Vec<ToolDescriptor> {
            vec![ToolDescriptor {
                key: "sleep-tool".into(),
                summary: "sleep tool".into(),
                kind: ToolKind::Local,
            }]
        }

        fn specifications(&self) -> Vec<ToolSpecification> {
            vec![ToolSpecification::new(
                "sleep_tool",
                ToolCapability::BACKGROUND_CONTINUABLE,
            )]
        }
    }

    impl ToolExecutor for SleepToolSource {
        fn execute<'a>(
            &'a self,
            call: FunctionCallRequest,
        ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>> {
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(FunctionResponse {
                    id: call.id,
                    name: call.name,
                    response: json!({ "ok": true }),
                })
            })
        }
    }

    fn temp_harness() -> Harness {
        let path = std::env::temp_dir().join(format!(
            "gemini-live-harness-controller-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ));
        Harness::open(path).expect("open harness")
    }

    #[tokio::test]
    async fn spawned_tool_call_emits_completion() {
        let controller = HarnessController::with_host_tools(
            temp_harness(),
            SleepToolSource {
                delay: Duration::from_millis(5),
            },
        )
        .expect("controller");
        let (tx, mut rx) = mpsc::unbounded_channel();
        controller.spawn_tool_call(
            FunctionCallRequest {
                id: "call_1".into(),
                name: "sleep_tool".into(),
                args: json!({}),
            },
            tx,
        );

        let completion = rx.recv().await.expect("tool completion");
        assert_eq!(completion.call_id, "call_1");
        assert_eq!(completion.call_name, "sleep_tool");
        assert!(completion.result.is_ok());
    }

    #[tokio::test]
    async fn cancelling_spawned_tool_call_suppresses_completion() {
        let controller = HarnessController::with_host_tools(
            temp_harness(),
            SleepToolSource {
                delay: Duration::from_secs(30),
            },
        )
        .expect("controller");
        let (tx, mut rx) = mpsc::unbounded_channel();
        controller.spawn_tool_call(
            FunctionCallRequest {
                id: "call_2".into(),
                name: "sleep_tool".into(),
                args: json!({}),
            },
            tx,
        );

        assert!(controller.cancel_tool_call("call_2"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn controller_interrupts_stale_running_tasks_on_startup() {
        let harness = temp_harness();
        let stale_runtime = TaskRuntimeInstance::current();
        let task = harness
            .start_task(NewRunningTask {
                title: "stale".into(),
                instructions: "continue".into(),
                runtime: stale_runtime,
                requested_by: Some("harness-tool-runtime".into()),
                tags: vec![],
                metadata: None,
                origin_call_id: Some("call_stale".into()),
            })
            .expect("start task");

        let controller = HarnessController::with_host_tools(
            harness.clone(),
            SleepToolSource {
                delay: Duration::from_millis(5),
            },
        )
        .expect("controller");
        assert_ne!(
            controller.runtime_owner().instance_id,
            task.runtime.as_ref().expect("runtime owner").instance_id
        );

        let interrupted = harness.read_task(&task.id).expect("read task");
        assert_eq!(interrupted.status, TaskStatus::Interrupted);
    }

    #[test]
    fn failed_completion_builds_runtime_error_response() {
        let completion = HarnessToolCompletion {
            call_id: "call_3".into(),
            call_name: "sleep_tool".into(),
            result: Err(ToolExecutionError::failed("boom")),
        };

        assert_eq!(
            completion.disposition(),
            HarnessToolCompletionDisposition::Failed
        );
        let response = completion
            .into_runtime_response()
            .expect("runtime response for failed tool call");
        assert_eq!(response.id, "call_3");
        assert_eq!(response.name, "sleep_tool");
        assert_eq!(
            response.response,
            json!({
                "ok": false,
                "error": {
                    "message": "tool execution failed: boom",
                },
            })
        );
    }
}
