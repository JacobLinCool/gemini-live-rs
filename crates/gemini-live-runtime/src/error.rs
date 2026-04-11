//! Runtime-level errors above the base `gemini-live` session layer.

use gemini_live::SessionError;

/// Failures surfaced by the shared runtime orchestration layer.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("runtime is not connected")]
    NotConnected,
    #[error("session resumption handle is not available yet")]
    MissingResumeHandle,
    #[error(transparent)]
    Session(#[from] SessionError),
}
