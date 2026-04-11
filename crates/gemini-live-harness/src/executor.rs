//! Harness-owned tool execution wrapper for host-defined tools.
//!
//! The wrapped host tools remain locally blocking from the model's
//! perspective. This module adds the harness-specific policy layer around
//! them:
//!
//! - enforce an inline latency budget
//! - when eligible tools exceed that budget, persist a background task,
//!   immediately return a normal `FunctionResponse`, and let the original
//!   tool execution continue in the background
//! - publish completion through the harness task + notification system rather
//!   than protocol-level async function calling
//!
//! Startup reconciliation for stale `Running` tasks lives one layer up in
//! `HarnessController`, because it is a host/runtime lifecycle concern rather
//! than a per-call execution concern.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use gemini_live::types::{FunctionCallRequest, FunctionResponse, Tool};
use serde_json::{Value, json};
use tokio::task::{AbortHandle, JoinError, JoinHandle};

use crate::error::HarnessError;
use crate::registry::HarnessToolRegistry;
use crate::store::Harness;
use crate::task::{NewRunningTask, TaskRuntimeInstance};
use crate::{NoopToolSource, ToolDescriptor, ToolExecutionError, ToolExecutor, ToolProvider};

const DEFAULT_INLINE_TIMEOUT: Duration = Duration::from_millis(300);
/// Harness-owned inline budget for host tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessToolBudget {
    pub inline_timeout: Duration,
}

impl Default for HarnessToolBudget {
    fn default() -> Self {
        Self {
            inline_timeout: DEFAULT_INLINE_TIMEOUT,
        }
    }
}

struct InFlightHostToolCall {
    name: String,
    abort_handle: AbortHandle,
    background_task_id: Option<String>,
}

/// Composite harness executor that wraps host-local tools with
/// harness-owned inline-budget policy.
pub struct HarnessToolRuntime<A = NoopToolSource> {
    harness: Harness,
    registry: HarnessToolRegistry<A>,
    host_executor: Arc<A>,
    budget: HarnessToolBudget,
    runtime_owner: TaskRuntimeInstance,
    in_flight_host_tools: Arc<Mutex<HashMap<String, InFlightHostToolCall>>>,
}

impl<A> Clone for HarnessToolRuntime<A> {
    fn clone(&self) -> Self {
        Self {
            harness: self.harness.clone(),
            registry: self.registry.clone(),
            host_executor: Arc::clone(&self.host_executor),
            budget: self.budget,
            runtime_owner: self.runtime_owner.clone(),
            in_flight_host_tools: Arc::clone(&self.in_flight_host_tools),
        }
    }
}

impl HarnessToolRuntime<NoopToolSource> {
    pub fn new(harness: Harness) -> Self {
        Self {
            harness,
            registry: HarnessToolRegistry::new(),
            host_executor: Arc::new(NoopToolSource),
            budget: HarnessToolBudget::default(),
            runtime_owner: TaskRuntimeInstance::current(),
            in_flight_host_tools: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn open_default() -> Result<Self, HarnessError> {
        Ok(Self::new(Harness::open_default()?))
    }
}

impl<A> HarnessToolRuntime<A>
where
    A: ToolProvider + ToolExecutor,
{
    pub fn with_host_tools(harness: Harness, host_tools: A) -> Result<Self, HarnessError> {
        let host_tools = Arc::new(host_tools);
        Ok(Self {
            harness,
            registry: HarnessToolRegistry::with_host_tools(Arc::clone(&host_tools))?,
            host_executor: host_tools,
            budget: HarnessToolBudget::default(),
            runtime_owner: TaskRuntimeInstance::current(),
            in_flight_host_tools: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn with_budget(mut self, budget: HarnessToolBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn harness(&self) -> &Harness {
        &self.harness
    }

    pub fn runtime_owner(&self) -> &TaskRuntimeInstance {
        &self.runtime_owner
    }

    async fn execute_routed_call(
        &self,
        call: FunctionCallRequest,
    ) -> Result<FunctionResponse, ToolExecutionError> {
        let Some(registration) = self.registry.route(&call.name).cloned() else {
            return Err(ToolExecutionError::unsupported_function(call.name));
        };

        if registration.capability.can_continue_async_after_timeout {
            self.execute_budgeted_host_call(call).await
        } else {
            self.host_executor.execute(call).await
        }
    }

    async fn execute_budgeted_host_call(
        &self,
        call: FunctionCallRequest,
    ) -> Result<FunctionResponse, ToolExecutionError> {
        let call_id = call.id.clone();
        let call_name = call.name.clone();
        let call_args = call.args.clone();
        let host_executor = Arc::clone(&self.host_executor);
        let mut execution = tokio::spawn(async move { host_executor.execute(call).await });
        self.insert_in_flight(
            call_id.clone(),
            InFlightHostToolCall {
                name: call_name.clone(),
                abort_handle: execution.abort_handle(),
                background_task_id: None,
            },
        );

        let sleep = tokio::time::sleep(self.budget.inline_timeout);
        tokio::pin!(sleep);

        tokio::select! {
            join_result = &mut execution => {
                self.remove_in_flight(&call_id);
                flatten_tool_join(&call_id, join_result)
            }
            _ = &mut sleep => {
                let task_id = match self.create_background_task_record(&call_id, &call_name, &call_args) {
                    Ok(task_id) => task_id,
                    Err(error) => {
                        self.remove_in_flight(&call_id);
                        execution.abort();
                        return Err(ToolExecutionError::failed(format!(
                            "failed to persist harness background task: {error}"
                        )));
                    }
                };
                self.set_background_task_id(&call_id, &task_id);
                let runtime = self.clone();
                let background_call_id = call_id.clone();
                let background_call_name = call_name.clone();
                let background_call_args = call_args.clone();
                let background_task_id = task_id.clone();
                tokio::spawn(async move {
                    runtime
                        .finish_background_tool_call(
                            background_call_id,
                            background_call_name,
                            background_call_args,
                            background_task_id,
                            execution,
                        )
                        .await;
                });
                Ok(background_task_response(
                    call_id,
                    call_name,
                    self.budget.inline_timeout,
                ))
            }
        }
    }

    async fn finish_background_tool_call(
        &self,
        call_id: String,
        call_name: String,
        call_args: Value,
        task_id: String,
        execution: JoinHandle<Result<FunctionResponse, ToolExecutionError>>,
    ) {
        let result = flatten_tool_join(&call_id, execution.await);
        match result {
            Ok(response) => {
                if response_indicates_failure(&response) {
                    let message =
                        background_tool_failure_summary(&call_name, &call_args, &response)
                            .unwrap_or_else(|| {
                                format!(
                                    "Tool call:\n{}\nError:\nunknown failure",
                                    format_tool_call(&call_name, &call_args)
                                )
                            });
                    if let Err(error) = self.harness().fail_task(&task_id, message) {
                        log_terminal_task_persist_error(&task_id, error);
                    }
                } else if let Err(error) = self.harness().complete_task(
                    &task_id,
                    Some(background_tool_success_summary(
                        &call_name, &call_args, &response,
                    )),
                    json!({
                        "toolCallId": response.id,
                        "toolName": response.name,
                        "toolArgs": call_args,
                        "response": response.response,
                    }),
                ) {
                    log_terminal_task_persist_error(&task_id, error);
                }
            }
            Err(ToolExecutionError::Cancelled { .. }) => {
                if let Err(error) = self.harness().cancel_task(
                    &task_id,
                    Some(format!(
                        "Tool call:\n{}\nError:\ntool execution was cancelled before completion",
                        format_tool_call(&call_name, &call_args)
                    )),
                ) {
                    log_terminal_task_persist_error(&task_id, error);
                }
            }
            Err(error) => {
                if let Err(persist_error) = self.harness().fail_task(
                    &task_id,
                    format!(
                        "Tool call:\n{}\nError:\n{error}",
                        format_tool_call(&call_name, &call_args)
                    ),
                ) {
                    log_terminal_task_persist_error(&task_id, persist_error);
                }
            }
        }
        self.remove_in_flight(&call_id);
    }

    fn create_background_task_record(
        &self,
        call_id: &str,
        call_name: &str,
        call_args: &Value,
    ) -> Result<String, HarnessError> {
        let task = self.harness().start_task(NewRunningTask {
            title: format!("Background tool: {call_name}"),
            instructions: format!(
                concat!(
                    "This task was created automatically by the harness because tool call ",
                    "`{}` (`{}`) exceeded the inline budget of {} ms. ",
                    "The original tool execution is already running and should continue in ",
                    "the background until it reaches a terminal result."
                ),
                call_name,
                call_id,
                self.budget.inline_timeout.as_millis()
            ),
            requested_by: Some("harness-tool-runtime".into()),
            tags: vec!["tool-call".into(), call_name.to_string()],
            metadata: Some(json!({
                "toolCallId": call_id,
                "toolName": call_name,
                "toolArgs": call_args,
                "inlineBudgetMs": self.budget.inline_timeout.as_millis(),
            })),
            runtime: self.runtime_owner.clone(),
            origin_call_id: Some(call_id.to_string()),
        })?;
        self.harness().record_task_progress(
            &task.id,
            format!(
                "Tool `{call_name}` exceeded the inline budget of {} ms and is continuing in the background.",
                self.budget.inline_timeout.as_millis()
            ),
            Some(json!({
                "toolCallId": call_id,
                "toolName": call_name,
            })),
        )?;
        Ok(task.id)
    }

    fn insert_in_flight(&self, call_id: String, call: InFlightHostToolCall) {
        self.in_flight_host_tools
            .lock()
            .expect("in-flight host tool lock")
            .insert(call_id, call);
    }

    fn set_background_task_id(&self, call_id: &str, task_id: &str) {
        if let Some(call) = self
            .in_flight_host_tools
            .lock()
            .expect("in-flight host tool lock")
            .get_mut(call_id)
        {
            call.background_task_id = Some(task_id.to_string());
        }
    }

    fn remove_in_flight(&self, call_id: &str) -> Option<InFlightHostToolCall> {
        self.in_flight_host_tools
            .lock()
            .expect("in-flight host tool lock")
            .remove(call_id)
    }
}

impl<A> fmt::Debug for HarnessToolRuntime<A>
where
    A: ToolProvider + ToolExecutor,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HarnessToolRuntime")
            .field("harness_root", &self.harness().paths().root())
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl<A> ToolProvider for HarnessToolRuntime<A>
where
    A: ToolProvider + ToolExecutor,
{
    fn advertised_tools(&self) -> Option<Vec<Tool>> {
        self.registry.advertised_tools()
    }

    fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.registry.descriptors()
    }
}

impl<A> ToolExecutor for HarnessToolRuntime<A>
where
    A: ToolProvider + ToolExecutor,
{
    fn execute<'a>(
        &'a self,
        call: FunctionCallRequest,
    ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>> {
        Box::pin(async move { self.execute_routed_call(call).await })
    }

    fn cancel(&self, call_id: &str) -> bool {
        let mut cancelled = self.host_executor.cancel(call_id);
        if let Some(in_flight) = self.remove_in_flight(call_id) {
            in_flight.abort_handle.abort();
            cancelled = true;
            if let Some(task_id) = in_flight.background_task_id
                && let Err(error) = self.harness().cancel_task(
                    &task_id,
                    Some(format!(
                        "Tool `{}` ({call_id}) was cancelled by the runtime.",
                        in_flight.name
                    )),
                )
            {
                log_terminal_task_persist_error(&task_id, error);
            }
        }
        cancelled
    }
}

fn flatten_tool_join(
    call_id: &str,
    join_result: Result<Result<FunctionResponse, ToolExecutionError>, JoinError>,
) -> Result<FunctionResponse, ToolExecutionError> {
    match join_result {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Err(ToolExecutionError::cancelled(call_id)),
        Err(error) => Err(ToolExecutionError::failed(format!(
            "tool execution task panicked or failed to join: {error}"
        ))),
    }
}

fn background_task_response(
    call_id: String,
    call_name: String,
    inline_timeout: Duration,
) -> FunctionResponse {
    FunctionResponse {
        id: call_id,
        name: call_name,
        response: json!({
            "message": format!(
                "This tool is taking longer than {} ms, so it is continuing in the background. Continue the conversation; a later notification will report the result.",
                inline_timeout.as_millis()
            )
        }),
    }
}

fn response_indicates_failure(response: &FunctionResponse) -> bool {
    response
        .response
        .get("ok")
        .and_then(Value::as_bool)
        .is_some_and(|ok| !ok)
}

fn tool_response_error_value(response: &FunctionResponse) -> Option<&Value> {
    response.response.get("error")
}

fn tool_response_success_value(response: &FunctionResponse) -> Option<&Value> {
    response.response.get("result")
}

fn background_tool_success_summary(
    call_name: &str,
    call_args: &Value,
    response: &FunctionResponse,
) -> String {
    let result = tool_response_success_value(response).unwrap_or(&response.response);
    format!(
        "Tool call:\n{}\nReturn value:\n{}",
        format_tool_call(call_name, call_args),
        format_notification_value(result)
    )
}

fn background_tool_failure_summary(
    call_name: &str,
    call_args: &Value,
    response: &FunctionResponse,
) -> Option<String> {
    tool_response_error_value(response).map(|error| {
        format!(
            "Tool call:\n{}\nError:\n{}",
            format_tool_call(call_name, call_args),
            format_notification_value(error)
        )
    })
}

fn format_tool_call(call_name: &str, call_args: &Value) -> String {
    format!("{}({})", call_name, format_notification_value(call_args))
}

fn format_notification_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn log_terminal_task_persist_error(task_id: &str, error: HarnessError) {
    match error {
        HarnessError::TaskAlreadyTerminal { .. } => {}
        other => tracing::warn!(
            "failed to persist terminal state for harness background task `{task_id}`: {other}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use futures_util::future::BoxFuture;
    use gemini_live::types::{FunctionDeclaration, FunctionResponse};
    use serde_json::json;

    use super::*;
    use crate::notification::NotificationStatus;
    use crate::task::TaskStatus;
    use crate::{
        ToolCapability, ToolDescriptor, ToolExecutionError, ToolExecutor, ToolKind, ToolProvider,
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
                    response: json!({
                        "ok": true,
                        "result": {
                            "done": true,
                            "message": "Sleep finished.",
                        },
                    }),
                })
            })
        }
    }

    fn temp_harness() -> Harness {
        let path = std::env::temp_dir().join(format!(
            "gemini-live-harness-runtime-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ));
        Harness::open(path).expect("open harness")
    }

    #[tokio::test]
    async fn eligible_tool_times_out_into_background_task() {
        let harness = temp_harness();
        let runtime = HarnessToolRuntime::with_host_tools(
            harness.clone(),
            SleepToolSource {
                delay: Duration::from_millis(50),
            },
        )
        .expect("runtime")
        .with_budget(HarnessToolBudget {
            inline_timeout: Duration::from_millis(5),
        });

        let response = runtime
            .execute(FunctionCallRequest {
                id: "call_1".into(),
                name: "sleep_tool".into(),
                args: json!({}),
            })
            .await
            .expect("execute tool");

        let message = response
            .response
            .get("message")
            .and_then(serde_json::Value::as_str)
            .expect("background continuation message");
        assert!(message.contains("taking longer than 5 ms"));
        assert!(message.contains("continuing in the background"));
        assert!(message.contains("later notification will report the result"));

        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let tasks = harness.list_tasks(None, 10).expect("list tasks");
                let task = tasks.first().expect("background task");
                if task.status == TaskStatus::Succeeded {
                    break task.id.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("wait for background completion");

        let task = harness.read_task(&status).expect("read task");
        assert_eq!(task.status, TaskStatus::Succeeded);
        let notifications = harness
            .list_notifications(Some(NotificationStatus::Queued), 10)
            .expect("list notifications");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].task_id.as_deref(), Some(status.as_str()));
        assert_eq!(
            notifications[0].title,
            "Task completed: Background tool: sleep_tool"
        );
        assert!(notifications[0].body.contains("Tool call:"));
        assert!(notifications[0].body.contains("sleep_tool({})"));
        assert!(notifications[0].body.contains("Return value:"));
        assert!(notifications[0].body.contains("\"done\": true"));
        assert!(
            notifications[0]
                .body
                .contains("\"message\": \"Sleep finished.\"")
        );

        let detail = harness.task_detail(&status, 10).expect("task detail");
        assert_eq!(
            detail.result.expect("task result").output["toolArgs"],
            json!({})
        );
    }

    #[tokio::test]
    async fn fast_tool_stays_inline_even_if_background_continuable() {
        let harness = temp_harness();
        let runtime = HarnessToolRuntime::with_host_tools(
            harness.clone(),
            SleepToolSource {
                delay: Duration::from_millis(5),
            },
        )
        .expect("runtime")
        .with_budget(HarnessToolBudget {
            inline_timeout: Duration::from_millis(50),
        });

        let response = runtime
            .execute(FunctionCallRequest {
                id: "call_2".into(),
                name: "sleep_tool".into(),
                args: json!({}),
            })
            .await
            .expect("execute tool");

        assert_eq!(response.response["result"]["done"], true);
        let tasks = harness.list_tasks(None, 10).expect("list tasks");
        assert!(tasks.is_empty());
    }
}
