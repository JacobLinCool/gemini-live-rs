//! Host-managed runtime that owns session forwarding and tool-call orchestration.
//!
//! [`LiveRuntime`](crate::LiveRuntime) remains the low-level staged-session
//! primitive. This module layers on the long-lived async tasks that hosts would
//! otherwise duplicate:
//!
//! - forwarding session events onto a single runtime event stream
//! - intercepting `toolCall` / `toolCallCancellation`
//! - dispatching host tools and sending `toolResponse`
//! - suppressing stale events across fresh-session apply boundaries

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gemini_live::types::{
    FunctionCallRequest, FunctionResponse, ServerEvent, SessionResumptionConfig, SetupConfig,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::{RuntimeConfig, SetupPatch};
use crate::driver::{RuntimeSession, RuntimeSessionObservation, SessionDriver};
use crate::error::{RuntimeError, ToolExecutionError};
use crate::event::{
    RuntimeEvent, RuntimeLifecycleEvent, RuntimeSendFailure, RuntimeSendOperation, ToolCallOutcome,
};
use crate::runtime::{ApplyReport, LiveRuntime};
use crate::tool::ToolAdapter;

#[derive(Debug)]
struct QueuedRuntimeEvent {
    generation: u64,
    event: RuntimeEvent,
}

/// Receiver for runtime events emitted by a [`ManagedRuntime`].
///
/// The receiver filters out events from superseded fresh-session generations so
/// hosts do not need to manually track switchover epochs.
pub struct RuntimeEventReceiver {
    rx: mpsc::UnboundedReceiver<QueuedRuntimeEvent>,
    generation: Arc<AtomicU64>,
}

impl RuntimeEventReceiver {
    /// Wait for the next non-stale runtime event.
    ///
    /// Lifecycle events are always delivered, even if they were emitted just
    /// before a fresh-session generation switchover.
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        loop {
            let queued = self.rx.recv().await?;
            let current_generation = self.generation.load(Ordering::SeqCst);
            if queued.generation == current_generation
                || matches!(queued.event, RuntimeEvent::Lifecycle(_))
            {
                return Some(queued.event);
            }
        }
    }
}

struct PendingToolTask {
    name: String,
    handle: JoinHandle<()>,
}

struct RuntimeTaskSet {
    forwarder: JoinHandle<()>,
    pending_tools: Arc<Mutex<HashMap<String, PendingToolTask>>>,
}

/// Higher-level runtime that owns the async orchestration around [`LiveRuntime`].
///
/// Hosts still own their own UI, persistence, and device I/O, but no longer
/// need to duplicate a second event/task layer on top of the staged-session
/// runtime core.
pub struct ManagedRuntime<D, A>
where
    D: SessionDriver,
    A: ToolAdapter,
{
    core: LiveRuntime<D>,
    active_tool_adapter: Arc<A>,
    desired_tool_adapter: Arc<A>,
    resume_handle: Arc<Mutex<Option<String>>>,
    generation: Arc<AtomicU64>,
    event_tx: mpsc::UnboundedSender<QueuedRuntimeEvent>,
    tasks: Option<RuntimeTaskSet>,
}

impl<D, A> ManagedRuntime<D, A>
where
    D: SessionDriver,
    A: ToolAdapter,
{
    pub fn new(config: RuntimeConfig, driver: D, tool_adapter: A) -> (Self, RuntimeEventReceiver) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let generation = Arc::new(AtomicU64::new(0));
        let tool_adapter = Arc::new(tool_adapter);
        let resume_handle = Arc::new(Mutex::new(None));
        (
            Self {
                core: LiveRuntime::new(config, driver),
                active_tool_adapter: Arc::clone(&tool_adapter),
                desired_tool_adapter: tool_adapter,
                resume_handle: Arc::clone(&resume_handle),
                generation: Arc::clone(&generation),
                event_tx,
                tasks: None,
            },
            RuntimeEventReceiver {
                rx: event_rx,
                generation,
            },
        )
    }

    pub fn active_setup(&self) -> &SetupConfig {
        self.core.active_setup()
    }

    pub fn desired_setup(&self) -> &SetupConfig {
        self.core.desired_setup()
    }

    pub fn stage_patch(&mut self, patch: &SetupPatch) {
        self.core.stage_patch(patch);
    }

    pub fn replace_desired_setup(&mut self, setup: SetupConfig) {
        self.core.replace_desired_setup(setup);
    }

    pub fn discard_staged_setup(&mut self) {
        self.core.discard_staged_setup();
    }

    pub fn active_session(&self) -> Option<D::Session> {
        self.core.session().cloned()
    }

    pub fn latest_resume_handle(&self) -> Option<String> {
        self.resume_handle
            .lock()
            .expect("resume handle lock")
            .clone()
    }

    pub fn replace_desired_tool_adapter(&mut self, tool_adapter: A) {
        self.desired_tool_adapter = Arc::new(tool_adapter);
    }

    pub fn discard_staged_tool_adapter(&mut self) {
        self.desired_tool_adapter = Arc::clone(&self.active_tool_adapter);
    }

    pub async fn connect(&mut self) -> Result<(), RuntimeError> {
        self.cancel_task_set();
        if self.core.session().is_some() {
            self.core.close().await?;
        }
        self.set_resume_handle(None);

        let generation = self.current_generation();
        self.emit_lifecycle(generation, RuntimeLifecycleEvent::Connecting);
        self.core.connect().await?;
        self.active_tool_adapter = Arc::clone(&self.desired_tool_adapter);
        self.tasks = Some(
            self.spawn_task_set(
                self.active_session()
                    .expect("connected runtime must expose an active session"),
                Arc::clone(&self.active_tool_adapter),
                Arc::clone(&self.resume_handle),
                generation,
            ),
        );
        self.emit_lifecycle(generation, RuntimeLifecycleEvent::Connected);
        Ok(())
    }

    /// Apply the desired setup using the latest server-issued resumption handle
    /// so conversation state can carry over across the reconnect.
    pub async fn apply(&mut self) -> Result<ApplyReport, RuntimeError> {
        let Some(resume_handle) = self.latest_resume_handle() else {
            return Err(RuntimeError::MissingResumeHandle);
        };
        let current_generation = self.current_generation();
        self.emit_lifecycle(current_generation, RuntimeLifecycleEvent::Reconnecting);

        let next_session = self
            .core
            .connect_with_setup(resumed_setup(
                self.core.desired_setup().clone(),
                resume_handle,
            ))
            .await?;
        let next_generation = current_generation + 1;

        self.cancel_task_set();
        let (report, old_session) = self.core.install_connected_session(next_session);
        self.generation.store(next_generation, Ordering::SeqCst);
        self.active_tool_adapter = Arc::clone(&self.desired_tool_adapter);
        self.tasks = Some(
            self.spawn_task_set(
                self.active_session()
                    .expect("applied runtime must expose an active session"),
                Arc::clone(&self.active_tool_adapter),
                Arc::clone(&self.resume_handle),
                next_generation,
            ),
        );

        if let Some(old_session) = old_session
            && let Err(error) = old_session.close().await
        {
            self.emit_send_failure(
                next_generation,
                RuntimeSendOperation::SessionClose,
                error.to_string(),
            );
        }

        self.emit_lifecycle(
            next_generation,
            RuntimeLifecycleEvent::AppliedResumedSession,
        );
        Ok(report)
    }

    /// Apply the desired setup onto a completely fresh session without
    /// attempting session resumption.
    pub async fn apply_fresh(&mut self) -> Result<ApplyReport, RuntimeError> {
        let current_generation = self.current_generation();
        self.emit_lifecycle(current_generation, RuntimeLifecycleEvent::Reconnecting);

        let next_session = self.core.connect_desired_session().await?;
        let next_generation = current_generation + 1;

        self.cancel_task_set();
        let (report, old_session) = self.core.install_connected_session(next_session);
        self.generation.store(next_generation, Ordering::SeqCst);
        self.active_tool_adapter = Arc::clone(&self.desired_tool_adapter);
        self.set_resume_handle(None);
        self.tasks = Some(
            self.spawn_task_set(
                self.active_session()
                    .expect("applied runtime must expose an active session"),
                Arc::clone(&self.active_tool_adapter),
                Arc::clone(&self.resume_handle),
                next_generation,
            ),
        );

        if let Some(old_session) = old_session
            && let Err(error) = old_session.close().await
        {
            self.emit_send_failure(
                next_generation,
                RuntimeSendOperation::SessionClose,
                error.to_string(),
            );
        }

        self.emit_lifecycle(next_generation, RuntimeLifecycleEvent::AppliedFreshSession);
        Ok(report)
    }

    pub async fn send_raw(
        &self,
        message: gemini_live::types::ClientMessage,
    ) -> Result<(), RuntimeError> {
        let Some(session) = self.active_session() else {
            return Err(RuntimeError::NotConnected);
        };
        match session.send_raw(message).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.emit_send_failure(
                    self.current_generation(),
                    RuntimeSendOperation::Raw,
                    error.to_string(),
                );
                Err(error.into())
            }
        }
    }

    pub async fn send_text(&self, text: &str) -> Result<(), RuntimeError> {
        let Some(session) = self.active_session() else {
            return Err(RuntimeError::NotConnected);
        };
        match session.send_text(text).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.emit_send_failure(
                    self.current_generation(),
                    RuntimeSendOperation::Text,
                    error.to_string(),
                );
                Err(error.into())
            }
        }
    }

    pub async fn send_audio_at_rate(
        &self,
        pcm_i16_le: &[u8],
        sample_rate: u32,
    ) -> Result<(), RuntimeError> {
        let Some(session) = self.active_session() else {
            return Err(RuntimeError::NotConnected);
        };
        match session.send_audio_at_rate(pcm_i16_le, sample_rate).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.emit_send_failure(
                    self.current_generation(),
                    RuntimeSendOperation::Audio,
                    error.to_string(),
                );
                Err(error.into())
            }
        }
    }

    pub async fn send_video(&self, data: &[u8], mime: &str) -> Result<(), RuntimeError> {
        let Some(session) = self.active_session() else {
            return Err(RuntimeError::NotConnected);
        };
        match session.send_video(data, mime).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.emit_send_failure(
                    self.current_generation(),
                    RuntimeSendOperation::Video,
                    error.to_string(),
                );
                Err(error.into())
            }
        }
    }

    pub async fn audio_stream_end(&self) -> Result<(), RuntimeError> {
        let Some(session) = self.active_session() else {
            return Err(RuntimeError::NotConnected);
        };
        match session.audio_stream_end().await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.emit_send_failure(
                    self.current_generation(),
                    RuntimeSendOperation::Audio,
                    error.to_string(),
                );
                Err(error.into())
            }
        }
    }

    pub async fn close(&mut self) -> Result<(), RuntimeError> {
        self.cancel_task_set();
        self.core.close().await?;
        self.set_resume_handle(None);
        self.emit_lifecycle(
            self.current_generation(),
            RuntimeLifecycleEvent::Closed {
                reason: String::new(),
            },
        );
        Ok(())
    }

    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn emit_lifecycle(&self, generation: u64, lifecycle: RuntimeLifecycleEvent) {
        self.emit(generation, RuntimeEvent::Lifecycle(lifecycle));
    }

    fn emit_send_failure(&self, generation: u64, operation: RuntimeSendOperation, reason: String) {
        self.emit(
            generation,
            RuntimeEvent::SendFailed(RuntimeSendFailure { operation, reason }),
        );
    }

    fn emit(&self, generation: u64, event: RuntimeEvent) {
        let _ = self.event_tx.send(QueuedRuntimeEvent { generation, event });
    }

    fn set_resume_handle(&self, handle: Option<String>) {
        *self.resume_handle.lock().expect("resume handle lock") = handle;
    }

    fn spawn_task_set(
        &self,
        session: D::Session,
        tool_adapter: Arc<A>,
        resume_handle: Arc<Mutex<Option<String>>>,
        generation: u64,
    ) -> RuntimeTaskSet {
        let pending_tools = Arc::new(Mutex::new(HashMap::new()));
        let forwarder = spawn_session_forwarder(
            session,
            tool_adapter,
            resume_handle,
            self.event_tx.clone(),
            generation,
            Arc::clone(&pending_tools),
        );
        RuntimeTaskSet {
            forwarder,
            pending_tools,
        }
    }

    fn cancel_task_set(&mut self) {
        if let Some(tasks) = self.tasks.take() {
            abort_pending_tools(&tasks.pending_tools);
            tasks.forwarder.abort();
        }
    }
}

fn spawn_session_forwarder<S, A>(
    session: S,
    tool_adapter: Arc<A>,
    resume_handle: Arc<Mutex<Option<String>>>,
    tx: mpsc::UnboundedSender<QueuedRuntimeEvent>,
    generation: u64,
    pending_tools: Arc<Mutex<HashMap<String, PendingToolTask>>>,
) -> JoinHandle<()>
where
    S: RuntimeSession,
    A: ToolAdapter,
{
    tokio::spawn(async move {
        let mut recv = session;
        while let Some(observation) = recv.next_observed_event().await {
            match observation {
                RuntimeSessionObservation::Lagged { count } => {
                    let _ = tx.send(QueuedRuntimeEvent {
                        generation,
                        event: RuntimeEvent::Lagged { count },
                    });
                }
                RuntimeSessionObservation::Event(event) => match event {
                    ServerEvent::SessionResumption {
                        new_handle,
                        resumable,
                    } => {
                        let next_handle = if resumable { new_handle.clone() } else { None };
                        *resume_handle.lock().expect("resume handle lock") = next_handle;
                        let _ = tx.send(QueuedRuntimeEvent {
                            generation,
                            event: RuntimeEvent::Server(ServerEvent::SessionResumption {
                                new_handle,
                                resumable,
                            }),
                        });
                    }
                    ServerEvent::ToolCall(calls) => {
                        for call in calls {
                            let call_id = call.id.clone();
                            let call_name = call.name.clone();
                            let _ = tx.send(QueuedRuntimeEvent {
                                generation,
                                event: RuntimeEvent::ToolCallStarted {
                                    id: call_id.clone(),
                                    name: call_name.clone(),
                                },
                            });

                            let handle = spawn_tool_call(
                                Arc::clone(&tool_adapter),
                                recv.clone(),
                                tx.clone(),
                                generation,
                                call,
                                Arc::clone(&pending_tools),
                            );
                            pending_tools.lock().expect("pending tools lock").insert(
                                call_id,
                                PendingToolTask {
                                    name: call_name,
                                    handle,
                                },
                            );
                        }
                    }
                    ServerEvent::ToolCallCancellation(ids) => {
                        for call_id in ids {
                            tool_adapter.cancel(&call_id);
                            if let Some(pending) = pending_tools
                                .lock()
                                .expect("pending tools lock")
                                .remove(&call_id)
                            {
                                pending.handle.abort();
                                let _ = tx.send(QueuedRuntimeEvent {
                                    generation,
                                    event: RuntimeEvent::ToolCallFinished {
                                        id: call_id,
                                        name: pending.name,
                                        outcome: ToolCallOutcome::Cancelled,
                                    },
                                });
                            }
                        }
                    }
                    ServerEvent::Closed { reason } => {
                        let _ = tx.send(QueuedRuntimeEvent {
                            generation,
                            event: RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Closed {
                                reason,
                            }),
                        });
                    }
                    other => {
                        let _ = tx.send(QueuedRuntimeEvent {
                            generation,
                            event: RuntimeEvent::Server(other),
                        });
                    }
                },
            }
        }

        fail_pending_tools(
            &pending_tools,
            &tx,
            generation,
            "session event stream ended".to_string(),
        );
    })
}

fn spawn_tool_call<S, A>(
    tool_adapter: Arc<A>,
    session: S,
    tx: mpsc::UnboundedSender<QueuedRuntimeEvent>,
    generation: u64,
    call: FunctionCallRequest,
    pending_tools: Arc<Mutex<HashMap<String, PendingToolTask>>>,
) -> JoinHandle<()>
where
    S: RuntimeSession,
    A: ToolAdapter,
{
    tokio::spawn(async move {
        let call_id = call.id.clone();
        let call_name = call.name.clone();
        let response = match tool_adapter.execute(call).await {
            Ok(response) => response,
            Err(ToolExecutionError::Cancelled { .. }) => {
                pending_tools
                    .lock()
                    .expect("pending tools lock")
                    .remove(&call_id);
                let _ = tx.send(QueuedRuntimeEvent {
                    generation,
                    event: RuntimeEvent::ToolCallFinished {
                        id: call_id,
                        name: call_name,
                        outcome: ToolCallOutcome::Cancelled,
                    },
                });
                return;
            }
            Err(error) => tool_error_response(&call_id, &call_name, error),
        };

        let outcome = match session.send_tool_response(vec![response]).await {
            Ok(()) => ToolCallOutcome::Responded,
            Err(error) => {
                let reason = error.to_string();
                let _ = tx.send(QueuedRuntimeEvent {
                    generation,
                    event: RuntimeEvent::SendFailed(RuntimeSendFailure {
                        operation: RuntimeSendOperation::ToolResponse,
                        reason: reason.clone(),
                    }),
                });
                ToolCallOutcome::Failed { reason }
            }
        };

        pending_tools
            .lock()
            .expect("pending tools lock")
            .remove(&call_id);
        let _ = tx.send(QueuedRuntimeEvent {
            generation,
            event: RuntimeEvent::ToolCallFinished {
                id: call_id,
                name: call_name,
                outcome,
            },
        });
    })
}

fn tool_error_response(
    call_id: &str,
    call_name: &str,
    error: ToolExecutionError,
) -> FunctionResponse {
    FunctionResponse {
        id: call_id.to_string(),
        name: call_name.to_string(),
        response: serde_json::json!({
            "ok": false,
            "error": {
                "message": error.to_string(),
            },
        }),
    }
}

fn abort_pending_tools(pending_tools: &Arc<Mutex<HashMap<String, PendingToolTask>>>) {
    let mut pending_tools = pending_tools.lock().expect("pending tools lock");
    for (_, pending) in pending_tools.drain() {
        pending.handle.abort();
    }
}

fn fail_pending_tools(
    pending_tools: &Arc<Mutex<HashMap<String, PendingToolTask>>>,
    tx: &mpsc::UnboundedSender<QueuedRuntimeEvent>,
    generation: u64,
    reason: String,
) {
    let mut pending_tools = pending_tools.lock().expect("pending tools lock");
    for (id, pending) in pending_tools.drain() {
        pending.handle.abort();
        let _ = tx.send(QueuedRuntimeEvent {
            generation,
            event: RuntimeEvent::ToolCallFinished {
                id,
                name: pending.name,
                outcome: ToolCallOutcome::Failed {
                    reason: reason.clone(),
                },
            },
        });
    }
}

fn resumed_setup(mut setup: SetupConfig, resume_handle: String) -> SetupConfig {
    let session_resumption = setup
        .session_resumption
        .get_or_insert_with(SessionResumptionConfig::default);
    session_resumption.handle = Some(resume_handle);
    setup
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use gemini_live::transport::TransportConfig;
    use gemini_live::types::{ClientMessage, GoogleSearchTool, ServerEvent, SetupConfig, Tool};
    use gemini_live::{ReconnectPolicy, SessionError, SessionStatus};

    use super::*;
    use crate::driver::SessionDriver;
    use crate::{RuntimeConfig, ToolDescriptor, ToolKind};

    #[derive(Clone, Default)]
    struct FakeDriver {
        connects: Arc<Mutex<Vec<gemini_live::SessionConfig>>>,
        sessions: Arc<Mutex<VecDeque<FakeSession>>>,
    }

    #[derive(Clone, Default)]
    struct FakeSession {
        sent: Arc<Mutex<Vec<ClientMessage>>>,
        events: Arc<Mutex<VecDeque<ServerEvent>>>,
        observations: Arc<Mutex<VecDeque<RuntimeSessionObservation>>>,
        close_count: Arc<Mutex<usize>>,
        hold_open: bool,
    }

    impl SessionDriver for FakeDriver {
        type Session = FakeSession;

        fn connect<'a>(
            &'a self,
            config: gemini_live::SessionConfig,
        ) -> BoxFuture<'a, Result<Self::Session, SessionError>> {
            let connects = Arc::clone(&self.connects);
            let sessions = Arc::clone(&self.sessions);
            Box::pin(async move {
                connects.lock().expect("connects lock").push(config);
                Ok(sessions
                    .lock()
                    .expect("sessions lock")
                    .pop_front()
                    .expect("fake session"))
            })
        }
    }

    impl RuntimeSession for FakeSession {
        fn status(&self) -> SessionStatus {
            SessionStatus::Connected
        }

        fn send_raw<'a>(
            &'a self,
            message: ClientMessage,
        ) -> BoxFuture<'a, Result<(), SessionError>> {
            let sent = Arc::clone(&self.sent);
            Box::pin(async move {
                sent.lock().expect("sent lock").push(message);
                Ok(())
            })
        }

        fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Option<ServerEvent>> {
            let events = Arc::clone(&self.events);
            let hold_open = self.hold_open;
            Box::pin(async move {
                let next = events.lock().expect("events lock").pop_front();
                if next.is_none() && hold_open {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                next
            })
        }

        fn next_observed_event<'a>(
            &'a mut self,
        ) -> BoxFuture<'a, Option<RuntimeSessionObservation>> {
            let observations = Arc::clone(&self.observations);
            let events = Arc::clone(&self.events);
            let hold_open = self.hold_open;
            Box::pin(async move {
                if let Some(observation) =
                    observations.lock().expect("observations lock").pop_front()
                {
                    return Some(observation);
                }
                let next = events.lock().expect("events lock").pop_front();
                if next.is_none() && hold_open {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                next.map(RuntimeSessionObservation::Event)
            })
        }

        fn close(self) -> BoxFuture<'static, Result<(), SessionError>>
        where
            Self: Sized,
        {
            let close_count = Arc::clone(&self.close_count);
            Box::pin(async move {
                *close_count.lock().expect("close count lock") += 1;
                Ok(())
            })
        }
    }

    #[derive(Clone, Copy)]
    struct FakeToolAdapter;

    impl ToolAdapter for FakeToolAdapter {
        fn descriptors(&self) -> Vec<ToolDescriptor> {
            vec![ToolDescriptor {
                key: "fake".into(),
                summary: "fake".into(),
                kind: ToolKind::Local,
            }]
        }

        fn execute<'a>(
            &'a self,
            call: FunctionCallRequest,
        ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>> {
            Box::pin(async move {
                Ok(FunctionResponse {
                    id: call.id,
                    name: call.name,
                    response: serde_json::json!({ "ok": true }),
                })
            })
        }
    }

    #[tokio::test]
    async fn connect_emits_lifecycle_and_forwards_server_events() {
        let session = FakeSession {
            events: Arc::new(Mutex::new(VecDeque::from(vec![ServerEvent::ModelText(
                "hello".into(),
            )]))),
            ..FakeSession::default()
        };
        let driver = FakeDriver {
            connects: Arc::new(Mutex::new(Vec::new())),
            sessions: Arc::new(Mutex::new(VecDeque::from(vec![session]))),
        };
        let (mut runtime, mut rx) = ManagedRuntime::new(test_config(), driver, FakeToolAdapter);

        runtime.connect().await.expect("connect runtime");

        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Connecting))
        ));
        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Connected))
        ));
        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Server(ServerEvent::ModelText(text))) if text == "hello"
        ));
    }

    #[tokio::test]
    async fn tool_calls_are_executed_and_reported() {
        let session = FakeSession {
            events: Arc::new(Mutex::new(VecDeque::from(vec![ServerEvent::ToolCall(
                vec![FunctionCallRequest {
                    id: "call-1".into(),
                    name: "fake".into(),
                    args: serde_json::json!({}),
                }],
            )]))),
            hold_open: true,
            ..FakeSession::default()
        };
        let sent = Arc::clone(&session.sent);
        let driver = FakeDriver {
            connects: Arc::new(Mutex::new(Vec::new())),
            sessions: Arc::new(Mutex::new(VecDeque::from(vec![session]))),
        };
        let (mut runtime, mut rx) = ManagedRuntime::new(test_config(), driver, FakeToolAdapter);

        runtime.connect().await.expect("connect runtime");
        let _ = rx.recv().await;
        let _ = rx.recv().await;

        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::ToolCallStarted { id, name }) if id == "call-1" && name == "fake"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::ToolCallFinished { id, name, outcome: ToolCallOutcome::Responded })
                if id == "call-1" && name == "fake"
        ));
        let sent = sent.lock().expect("sent lock");
        assert!(matches!(sent.as_slice(), [ClientMessage::ToolResponse(_)]));
    }

    #[tokio::test]
    async fn lagged_observations_are_forwarded() {
        let session = FakeSession {
            observations: Arc::new(Mutex::new(VecDeque::from(vec![
                RuntimeSessionObservation::Lagged { count: 4 },
            ]))),
            ..FakeSession::default()
        };
        let driver = FakeDriver {
            connects: Arc::new(Mutex::new(Vec::new())),
            sessions: Arc::new(Mutex::new(VecDeque::from(vec![session]))),
        };
        let (mut runtime, mut rx) = ManagedRuntime::new(test_config(), driver, FakeToolAdapter);

        runtime.connect().await.expect("connect runtime");
        let _ = rx.recv().await;
        let _ = rx.recv().await;

        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Lagged { count: 4 })
        ));
    }

    #[tokio::test]
    async fn apply_uses_latest_resume_handle_for_carryover() {
        let initial_session = FakeSession {
            events: Arc::new(Mutex::new(VecDeque::from(vec![
                ServerEvent::SessionResumption {
                    new_handle: Some("resume-1".into()),
                    resumable: true,
                },
            ]))),
            ..FakeSession::default()
        };
        let resumed_session = FakeSession::default();
        let close_count = Arc::clone(&initial_session.close_count);
        let driver = FakeDriver {
            connects: Arc::new(Mutex::new(Vec::new())),
            sessions: Arc::new(Mutex::new(VecDeque::from(vec![
                initial_session,
                resumed_session,
            ]))),
        };
        let connects = Arc::clone(&driver.connects);
        let (mut runtime, mut rx) = ManagedRuntime::new(test_config(), driver, FakeToolAdapter);

        runtime.connect().await.expect("connect runtime");
        let _ = rx.recv().await;
        let _ = rx.recv().await;
        let _ = rx.recv().await;

        runtime.apply().await.expect("apply with carryover");

        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Reconnecting))
        ));
        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Lifecycle(
                RuntimeLifecycleEvent::AppliedResumedSession
            ))
        ));

        let connects = connects.lock().expect("connects lock");
        assert_eq!(connects.len(), 2);
        assert_eq!(
            connects[1]
                .setup
                .session_resumption
                .as_ref()
                .and_then(|config| config.handle.as_deref()),
            Some("resume-1")
        );
        assert_eq!(*close_count.lock().expect("close count lock"), 1);
    }

    #[tokio::test]
    async fn apply_requires_resume_handle() {
        let session = FakeSession::default();
        let driver = FakeDriver {
            connects: Arc::new(Mutex::new(Vec::new())),
            sessions: Arc::new(Mutex::new(VecDeque::from(vec![session]))),
        };
        let (mut runtime, _rx) = ManagedRuntime::new(test_config(), driver, FakeToolAdapter);

        runtime.connect().await.expect("connect runtime");

        let error = runtime
            .apply()
            .await
            .expect_err("apply should require handle");
        assert!(matches!(error, RuntimeError::MissingResumeHandle));
    }

    fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            session: gemini_live::SessionConfig {
                transport: TransportConfig::default(),
                setup: SetupConfig {
                    model: "models/test".into(),
                    tools: Some(vec![Tool::GoogleSearch(GoogleSearchTool {})]),
                    ..Default::default()
                },
                reconnect: ReconnectPolicy::default(),
            },
        }
    }
}
