//! Harness-owned bridge between runtime tool-call events and harness tool
//! orchestration.
//!
//! Hosts still consume runtime events for product-specific UI and lifecycle
//! behavior, but should not need to manage separate channels for tool
//! completions or hand-write `ToolCallRequested -> spawn -> send_tool_response`
//! glue. `HarnessRuntimeBridge` keeps that orchestration inside the harness
//! layer while remaining agnostic about how the host renders logs or UI.

use std::fmt;
use std::future::Future;

use gemini_live::types::FunctionResponse;
use gemini_live_runtime::RuntimeEvent;
use tokio::sync::mpsc;

use crate::{
    HarnessController, HarnessToolCompletion, HarnessToolCompletionDisposition, NoopToolSource,
    ToolExecutor, ToolProvider,
};

/// Summary of one tool completion after it has been forwarded back into the
/// runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessToolForwardOutcome {
    pub call_id: String,
    pub call_name: String,
    pub disposition: HarnessToolCompletionDisposition,
}

/// Failure while forwarding one tool completion back into the runtime.
#[derive(Debug)]
pub struct HarnessToolForwardFailure<E> {
    pub call_id: String,
    pub call_name: String,
    pub disposition: HarnessToolCompletionDisposition,
    pub source: E,
}

impl<E> fmt::Display for HarnessToolForwardFailure<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.call_name, self.call_id)?;
        write!(f, ": {}", self.source)
    }
}

impl<E> std::error::Error for HarnessToolForwardFailure<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl HarnessToolCompletion {
    /// Forward this tool completion back into the runtime using the provided
    /// sender closure, then return a compact summary for host logging.
    pub async fn forward_with<F, Fut, E>(
        self,
        send: F,
    ) -> Result<HarnessToolForwardOutcome, HarnessToolForwardFailure<E>>
    where
        F: FnOnce(Vec<FunctionResponse>) -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let disposition = self.disposition();
        let call_id = self.call_id.clone();
        let call_name = self.call_name.clone();
        if let Some(response) = self.into_runtime_response() {
            send(vec![response])
                .await
                .map_err(|source| HarnessToolForwardFailure {
                    call_id: call_id.clone(),
                    call_name: call_name.clone(),
                    disposition,
                    source,
                })?;
        }
        Ok(HarnessToolForwardOutcome {
            call_id,
            call_name,
            disposition,
        })
    }
}

/// Non-cloneable runtime bridge that owns tool-completion plumbing for one
/// host runtime event loop.
pub struct HarnessRuntimeBridge<A = NoopToolSource> {
    controller: HarnessController<A>,
    completions_tx: mpsc::UnboundedSender<HarnessToolCompletion>,
    completions_rx: mpsc::UnboundedReceiver<HarnessToolCompletion>,
}

impl<A> HarnessRuntimeBridge<A>
where
    A: ToolProvider + ToolExecutor,
{
    pub fn new(controller: HarnessController<A>) -> Self {
        let (completions_tx, completions_rx) = mpsc::unbounded_channel();
        Self {
            controller,
            completions_tx,
            completions_rx,
        }
    }

    pub fn controller(&self) -> &HarnessController<A> {
        &self.controller
    }

    /// Consume tool-call-related runtime events inside the harness layer.
    ///
    /// Returns `true` when the event was a tool event and has been handled.
    pub fn handle_runtime_event(&self, event: &RuntimeEvent) -> bool {
        match event {
            RuntimeEvent::ToolCallRequested { call } => {
                self.controller
                    .spawn_tool_call(call.clone(), self.completions_tx.clone());
                true
            }
            RuntimeEvent::ToolCallCancellationRequested { ids } => {
                for call_id in ids {
                    self.controller.cancel_tool_call(call_id);
                }
                true
            }
            _ => false,
        }
    }

    /// Wait for the next tool completion, forward it with the provided sender,
    /// and return a host-loggable summary.
    pub async fn recv_and_forward_tool_completion<F, Fut, E>(
        &mut self,
        send: F,
    ) -> Option<Result<HarnessToolForwardOutcome, HarnessToolForwardFailure<E>>>
    where
        F: FnOnce(Vec<FunctionResponse>) -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let completion = self.completions_rx.recv().await?;
        Some(completion.forward_with(send).await)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use gemini_live::types::{FunctionCallRequest, FunctionDeclaration, FunctionResponse, Tool};
    use gemini_live_runtime::RuntimeEvent;
    use serde_json::json;

    use super::*;
    use crate::{
        Harness, HarnessToolBudget, ToolCapability, ToolDescriptor, ToolExecutionError, ToolKind,
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
                ToolCapability::INLINE_ONLY,
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
            "gemini-live-harness-bridge-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ));
        Harness::open(path).expect("open harness")
    }

    #[tokio::test]
    async fn runtime_bridge_spawns_and_forwards_tool_completion() {
        let controller = HarnessController::with_host_tools(
            temp_harness(),
            SleepToolSource {
                delay: Duration::from_millis(5),
            },
        )
        .expect("controller")
        .with_budget(HarnessToolBudget::default());
        let mut bridge = HarnessRuntimeBridge::new(controller);

        assert!(
            bridge.handle_runtime_event(&RuntimeEvent::ToolCallRequested {
                call: FunctionCallRequest {
                    id: "call_1".into(),
                    name: "sleep_tool".into(),
                    args: json!({}),
                },
            })
        );

        let forwarded = bridge
            .recv_and_forward_tool_completion(|responses| async move {
                assert_eq!(responses.len(), 1);
                assert_eq!(responses[0].id, "call_1");
                Ok::<(), std::convert::Infallible>(())
            })
            .await
            .expect("tool completion")
            .expect("forwarded completion");

        assert_eq!(forwarded.call_id, "call_1");
        assert_eq!(forwarded.call_name, "sleep_tool");
        assert_eq!(
            forwarded.disposition,
            HarnessToolCompletionDisposition::Responded
        );
    }
}
