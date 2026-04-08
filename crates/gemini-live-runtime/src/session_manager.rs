//! Higher-level session lifecycle skeleton above [`ManagedRuntime`](crate::ManagedRuntime).
//!
//! The managed runtime already owns active-session forwarding and tool
//! orchestration. Hosts that want lower-power behavior still need one more
//! layer that decides when a Live session should be hot, when it can go
//! dormant, and how a fresh session should be rehydrated from process-local
//! memory.
//!
//! This module defines that reusable contract without binding it to any one
//! host product. Discord, a desktop host, or a future bot can all supply their
//! own wake/activity policy while reusing the same lifecycle surface.

use std::time::{Duration, Instant};

use gemini_live::types::{Content, HistoryConfig, SetupConfig};

use crate::driver::SessionDriver;
use crate::error::RuntimeError;
use crate::managed::ManagedRuntime;
use crate::memory::{ConversationMemoryStore, ConversationSnapshot};
use crate::tool::ToolAdapter;

/// High-level lifecycle state for a host-managed Live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycleState {
    Dormant,
    Connecting,
    Rehydrating,
    Hot,
    Closing,
}

/// Host-visible reason that triggered a session wake-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    TextInput,
    VoiceJoin,
    VoiceActivity,
    ExplicitRefresh,
}

/// Activity kinds that refresh the idle timer while a process remains alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    TextInput,
    VoiceInput,
    ModelOutput,
    ToolCall,
}

/// How the next hot session should be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeStrategy {
    AlreadyHot,
    Resume,
    Fresh,
    FreshWithRehydrate,
}

/// Recommended host action when evaluating idle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDecision {
    StayHot,
    EnterDormant,
}

/// Runtime-owned policy knobs for dormant/hot session transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdlePolicy {
    pub idle_timeout: Duration,
    pub resumable_handle_max_age: Duration,
    pub max_recent_turns: usize,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(10 * 60),
            resumable_handle_max_age: Duration::from_secs(2 * 60 * 60),
            max_recent_turns: 16,
        }
    }
}

/// Report describing how the session manager satisfied a wake request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeOutcome {
    pub reason: WakeReason,
    pub strategy: WakeStrategy,
}

/// Reusable session lifecycle coordinator.
pub struct SessionManager<D, A, M>
where
    D: SessionDriver,
    A: ToolAdapter,
    M: ConversationMemoryStore,
{
    runtime: ManagedRuntime<D, A>,
    memory: M,
    idle_policy: IdlePolicy,
    lifecycle_state: SessionLifecycleState,
}

impl<D, A, M> SessionManager<D, A, M>
where
    D: SessionDriver,
    A: ToolAdapter,
    M: ConversationMemoryStore,
{
    pub fn new(runtime: ManagedRuntime<D, A>, memory: M, idle_policy: IdlePolicy) -> Self {
        Self {
            runtime,
            memory,
            idle_policy,
            lifecycle_state: SessionLifecycleState::Dormant,
        }
    }

    pub fn runtime(&self) -> &ManagedRuntime<D, A> {
        &self.runtime
    }

    pub fn runtime_mut(&mut self) -> &mut ManagedRuntime<D, A> {
        &mut self.runtime
    }

    pub fn memory(&self) -> &M {
        &self.memory
    }

    pub fn lifecycle_state(&self) -> SessionLifecycleState {
        self.lifecycle_state
    }

    pub fn idle_policy(&self) -> &IdlePolicy {
        &self.idle_policy
    }

    pub fn snapshot(&self) -> ConversationSnapshot {
        self.memory.load_snapshot()
    }

    pub fn active_session(&self) -> Option<D::Session> {
        self.runtime.active_session()
    }

    /// Copy the latest runtime-issued resumable handle into process memory.
    pub fn sync_resume_handle_from_runtime(&self, issued_at: Instant) {
        let Some(handle) = self.runtime.latest_resume_handle() else {
            return;
        };

        let mut snapshot = self.memory.load_snapshot();
        snapshot.install_resumable_handle(handle, issued_at);
        self.memory.store_snapshot(snapshot);
    }

    /// Refresh the process-local activity timestamp.
    pub fn record_activity(&self, _kind: ActivityKind, at: Instant) {
        let mut snapshot = self.memory.load_snapshot();
        snapshot.note_activity(at);
        self.memory.store_snapshot(snapshot);
    }

    /// Append one completed turn into process-local memory.
    pub fn record_recent_turn(&self, turn: Content) {
        let mut snapshot = self.memory.load_snapshot();
        snapshot.push_recent_turn(turn, self.idle_policy.max_recent_turns);
        self.memory.store_snapshot(snapshot);
    }

    /// Replace the rolling summary used to cold-rehydrate future fresh sessions.
    pub fn set_rolling_summary(&self, summary: Option<Content>) {
        let mut snapshot = self.memory.load_snapshot();
        snapshot.set_rolling_summary(summary);
        self.memory.store_snapshot(snapshot);
    }

    /// Drop the stored resumable handle from process-local memory.
    pub fn clear_resumable_handle(&self) {
        let mut snapshot = self.memory.load_snapshot();
        snapshot.clear_resumable_handle();
        self.memory.store_snapshot(snapshot);
    }

    /// Decide whether the host should keep the current hot session open.
    pub fn idle_decision(&self, now: Instant) -> IdleDecision {
        let snapshot = self.memory.load_snapshot();
        match snapshot.last_activity_at {
            Some(last_activity)
                if now.duration_since(last_activity) < self.idle_policy.idle_timeout =>
            {
                IdleDecision::StayHot
            }
            Some(_) | None => IdleDecision::EnterDormant,
        }
    }

    pub async fn ensure_hot(
        &mut self,
        reason: WakeReason,
        now: Instant,
    ) -> Result<WakeOutcome, RuntimeError> {
        if self.runtime.active_session().is_some() {
            self.lifecycle_state = SessionLifecycleState::Hot;
            self.record_activity(map_wake_reason_to_activity(reason), now);
            return Ok(WakeOutcome {
                reason,
                strategy: WakeStrategy::AlreadyHot,
            });
        }

        let resume_handle = self
            .memory
            .load_snapshot()
            .resumable_handle(now, self.idle_policy.resumable_handle_max_age)
            .map(ToOwned::to_owned);
        let should_rehydrate = self
            .memory
            .load_snapshot()
            .build_rehydrate_content()
            .is_some();

        self.lifecycle_state = SessionLifecycleState::Connecting;
        let strategy = if let Some(handle) = resume_handle {
            match self.runtime.connect_resumed(handle).await {
                Ok(()) => WakeStrategy::Resume,
                Err(_) => {
                    self.clear_resumable_handle();
                    if should_rehydrate {
                        self.runtime
                            .connect_with_setup_override(initial_history_setup(
                                self.runtime.desired_setup().clone(),
                            ))
                            .await?;
                        self.lifecycle_state = SessionLifecycleState::Rehydrating;
                        self.rehydrate_from_memory().await?;
                        WakeStrategy::FreshWithRehydrate
                    } else {
                        self.runtime.connect().await?;
                        WakeStrategy::Fresh
                    }
                }
            }
        } else {
            if should_rehydrate {
                self.runtime
                    .connect_with_setup_override(initial_history_setup(
                        self.runtime.desired_setup().clone(),
                    ))
                    .await?;
                self.lifecycle_state = SessionLifecycleState::Rehydrating;
                self.rehydrate_from_memory().await?;
                WakeStrategy::FreshWithRehydrate
            } else {
                self.runtime.connect().await?;
                WakeStrategy::Fresh
            }
        };
        self.lifecycle_state = SessionLifecycleState::Hot;
        self.record_activity(map_wake_reason_to_activity(reason), now);
        Ok(WakeOutcome { reason, strategy })
    }

    pub async fn enter_dormant(&mut self) -> Result<(), RuntimeError> {
        if self.runtime.active_session().is_none() {
            self.lifecycle_state = SessionLifecycleState::Dormant;
            return Ok(());
        }

        self.lifecycle_state = SessionLifecycleState::Closing;
        self.sync_resume_handle_from_runtime(Instant::now());
        self.runtime.close().await?;
        self.lifecycle_state = SessionLifecycleState::Dormant;
        Ok(())
    }

    pub async fn rehydrate_from_memory(&mut self) -> Result<bool, RuntimeError> {
        let Some(history) = self.memory.load_snapshot().build_rehydrate_content() else {
            return Ok(false);
        };
        self.runtime.send_client_content(history).await?;
        Ok(true)
    }
}

fn map_wake_reason_to_activity(reason: WakeReason) -> ActivityKind {
    match reason {
        WakeReason::TextInput => ActivityKind::TextInput,
        WakeReason::VoiceJoin | WakeReason::VoiceActivity => ActivityKind::VoiceInput,
        WakeReason::ExplicitRefresh => ActivityKind::ToolCall,
    }
}

fn initial_history_setup(mut setup: SetupConfig) -> SetupConfig {
    setup.history_config = Some(HistoryConfig {
        initial_history_in_client_content: Some(true),
    });
    setup
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use gemini_live::transport::TransportConfig;
    use gemini_live::types::{ClientMessage, Content, Part, SessionResumptionConfig, SetupConfig};
    use gemini_live::{ReconnectPolicy, SessionConfig, SessionError, SessionStatus};

    use super::*;
    use crate::config::RuntimeConfig;
    use crate::driver::{RuntimeSession, SessionDriver};
    use crate::memory::{ConversationMemoryStore, InMemoryConversationMemory};
    use crate::tool::NoopToolAdapter;

    #[derive(Clone, Default)]
    struct FakeDriver {
        connects: Arc<Mutex<Vec<SessionConfig>>>,
        sessions: Arc<Mutex<VecDeque<FakeSession>>>,
    }

    #[derive(Clone, Default)]
    struct FakeSession {
        sent: Arc<Mutex<Vec<ClientMessage>>>,
        close_count: Arc<Mutex<usize>>,
    }

    impl SessionDriver for FakeDriver {
        type Session = FakeSession;

        fn connect<'a>(
            &'a self,
            config: SessionConfig,
        ) -> BoxFuture<'a, Result<Self::Session, SessionError>> {
            let connects = Arc::clone(&self.connects);
            let sessions = Arc::clone(&self.sessions);
            Box::pin(async move {
                connects.lock().expect("connects lock").push(config);
                Ok(sessions
                    .lock()
                    .expect("sessions lock")
                    .pop_front()
                    .unwrap_or_default())
            })
        }
    }

    impl RuntimeSession for FakeSession {
        fn status(&self) -> SessionStatus {
            SessionStatus::Connected
        }

        fn send_raw<'a>(
            &'a self,
            message: gemini_live::types::ClientMessage,
        ) -> BoxFuture<'a, Result<(), SessionError>> {
            let sent = Arc::clone(&self.sent);
            Box::pin(async move {
                sent.lock().expect("sent lock").push(message);
                Ok(())
            })
        }

        fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Option<gemini_live::types::ServerEvent>> {
            Box::pin(async { None })
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

    fn setup() -> SetupConfig {
        SetupConfig {
            model: "models/test".into(),
            session_resumption: Some(SessionResumptionConfig::default()),
            ..Default::default()
        }
    }

    fn runtime_config() -> RuntimeConfig {
        RuntimeConfig {
            session: SessionConfig {
                transport: TransportConfig::default(),
                setup: setup(),
                reconnect: ReconnectPolicy::default(),
            },
        }
    }

    #[tokio::test]
    async fn fresh_wake_rehydrates_recent_turns() {
        let first_session = FakeSession::default();
        let first_sent = Arc::clone(&first_session.sent);
        let driver = FakeDriver {
            connects: Arc::default(),
            sessions: Arc::new(Mutex::new(VecDeque::from([first_session]))),
        };
        let (runtime, _events) = ManagedRuntime::new(runtime_config(), driver, NoopToolAdapter);
        let memory = InMemoryConversationMemory::new();
        let mut snapshot = memory.load_snapshot();
        snapshot.push_recent_turn(
            Content {
                role: Some("user".into()),
                parts: vec![Part {
                    text: Some("hello".into()),
                    inline_data: None,
                }],
            },
            16,
        );
        memory.store_snapshot(snapshot);
        let mut manager = SessionManager::new(runtime, memory, IdlePolicy::default());

        let outcome = manager
            .ensure_hot(WakeReason::TextInput, Instant::now())
            .await
            .expect("fresh wake");

        assert_eq!(outcome.strategy, WakeStrategy::FreshWithRehydrate);
        assert_eq!(manager.lifecycle_state(), SessionLifecycleState::Hot);
        assert!(matches!(
            first_sent.lock().expect("sent lock").first(),
            Some(ClientMessage::ClientContent(_))
        ));
    }

    #[tokio::test]
    async fn resume_wake_injects_resume_handle_without_rehydrate() {
        let driver = FakeDriver {
            connects: Arc::default(),
            sessions: Arc::new(Mutex::new(VecDeque::from([FakeSession::default()]))),
        };
        let connects = Arc::clone(&driver.connects);
        let (runtime, _events) = ManagedRuntime::new(runtime_config(), driver, NoopToolAdapter);
        let memory = InMemoryConversationMemory::new();
        let issued_at = Instant::now();
        let mut snapshot = memory.load_snapshot();
        snapshot.install_resumable_handle("resume-1", issued_at);
        memory.store_snapshot(snapshot);
        let mut manager = SessionManager::new(runtime, memory, IdlePolicy::default());

        let outcome = manager
            .ensure_hot(WakeReason::VoiceJoin, issued_at + Duration::from_secs(30))
            .await
            .expect("resume wake");

        assert_eq!(outcome.strategy, WakeStrategy::Resume);
        let setup = connects
            .lock()
            .expect("connects lock")
            .first()
            .expect("connect setup")
            .setup
            .clone();
        assert_eq!(
            setup
                .session_resumption
                .as_ref()
                .and_then(|config| config.handle.as_deref()),
            Some("resume-1")
        );
    }

    #[tokio::test]
    async fn fresh_wake_without_memory_does_not_enable_initial_history_mode() {
        let driver = FakeDriver {
            connects: Arc::default(),
            sessions: Arc::new(Mutex::new(VecDeque::from([FakeSession::default()]))),
        };
        let connects = Arc::clone(&driver.connects);
        let (runtime, _events) = ManagedRuntime::new(runtime_config(), driver, NoopToolAdapter);
        let memory = InMemoryConversationMemory::new();
        let mut manager = SessionManager::new(runtime, memory, IdlePolicy::default());

        let outcome = manager
            .ensure_hot(WakeReason::TextInput, Instant::now())
            .await
            .expect("fresh wake");

        assert_eq!(outcome.strategy, WakeStrategy::Fresh);
        let setup = connects
            .lock()
            .expect("connects lock")
            .first()
            .expect("connect setup")
            .setup
            .clone();
        assert!(setup.history_config.is_none());
    }
}
