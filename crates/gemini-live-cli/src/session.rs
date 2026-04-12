//! CLI runtime bootstrap and dormant/hot session helpers.
//!
//! The desktop CLI should not keep a Live session hot while the user is idle.
//! This module is the natural home for the CLI's session-manager bootstrap and
//! the small helper operations that wake the runtime on demand.

use std::time::{Duration, Instant};

use gemini_live::types::{SetupConfig, Tool};
use gemini_live_runtime::{
    GeminiSessionDriver, GeminiSessionHandle, IdlePolicy, InMemoryConversationMemory,
    ManagedRuntime, RuntimeError, RuntimeEventReceiver, SessionManager, WakeReason,
};

use crate::startup::{StartupConfig, build_runtime_config_with_tools};

pub(crate) const CLI_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

pub(crate) type CliConversationMemory = InMemoryConversationMemory;
pub(crate) type CliSessionManager = SessionManager<GeminiSessionDriver, CliConversationMemory>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplySessionConfigDisposition {
    Reconnected,
    ArmedForNextWake,
}

pub(crate) fn new_session_manager(
    startup: &StartupConfig,
    tools: Option<Vec<Tool>>,
    system_instruction: Option<&str>,
) -> (CliSessionManager, RuntimeEventReceiver) {
    let (runtime, runtime_events) = ManagedRuntime::new(
        build_runtime_config_with_tools(startup, tools, system_instruction),
        GeminiSessionDriver,
    );
    (
        SessionManager::new(
            runtime,
            InMemoryConversationMemory::new(),
            IdlePolicy {
                idle_timeout: CLI_IDLE_TIMEOUT,
                ..Default::default()
            },
        ),
        runtime_events,
    )
}

pub(crate) async fn ensure_hot_session(
    session_manager: &mut CliSessionManager,
    reason: WakeReason,
) -> Result<GeminiSessionHandle, RuntimeError> {
    session_manager.ensure_hot(reason, Instant::now()).await?;
    session_manager
        .active_session()
        .ok_or(RuntimeError::NotConnected)
}

pub(crate) async fn apply_staged_setup(
    session_manager: &mut CliSessionManager,
    setup: SetupConfig,
) -> Result<ApplySessionConfigDisposition, RuntimeError> {
    session_manager.runtime_mut().replace_desired_setup(setup);
    if session_manager.active_session().is_some() {
        session_manager.runtime_mut().apply().await?;
        Ok(ApplySessionConfigDisposition::Reconnected)
    } else {
        Ok(ApplySessionConfigDisposition::ArmedForNextWake)
    }
}

pub(crate) async fn audio_stream_end_if_hot(
    session_manager: &CliSessionManager,
) -> Result<(), RuntimeError> {
    if session_manager.active_session().is_some() {
        session_manager.runtime().audio_stream_end().await?;
    }
    Ok(())
}
