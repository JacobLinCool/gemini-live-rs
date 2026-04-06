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

/// Failures while resolving or executing a server-requested tool call.
#[derive(Debug, thiserror::Error)]
pub enum ToolExecutionError {
    #[error("unsupported function `{name}`")]
    UnsupportedFunction { name: String },
    #[error("tool execution failed: {message}")]
    Failed { message: String },
    #[error("tool call `{call_id}` was cancelled")]
    Cancelled { call_id: String },
}

impl ToolExecutionError {
    pub fn unsupported_function(name: impl Into<String>) -> Self {
        Self::UnsupportedFunction { name: name.into() }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }

    pub fn cancelled(call_id: impl Into<String>) -> Self {
        Self::Cancelled {
            call_id: call_id.into(),
        }
    }
}
