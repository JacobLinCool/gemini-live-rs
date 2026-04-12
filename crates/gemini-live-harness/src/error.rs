//! Errors surfaced by the durable harness layer.

use std::path::PathBuf;

/// Failures while reading, writing, or interpreting on-disk harness state.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("home directory is unavailable")]
    HomeDirectoryUnavailable,
    #[error("invalid {kind} `{value}`")]
    InvalidSegment { kind: &'static str, value: String },
    #[error("duplicate tool function `{name}`")]
    DuplicateToolFunction { name: String },
    #[error("{kind} `{id}` was not found")]
    NotFound { kind: &'static str, id: String },
    #[error("task `{id}` is already in terminal state `{status}`")]
    TaskAlreadyTerminal { id: String, status: String },
    #[error("task `{id}` is not running")]
    TaskNotRunning { id: String },
    #[error("notification `{id}` cannot transition from `{from}` to `{to}`")]
    NotificationStatusConflict {
        id: String,
        from: String,
        to: &'static str,
    },
    #[error("failed to read harness state at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse harness JSON at `{path}`: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl HarnessError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn json(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        Self::Json {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn not_found(kind: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            kind,
            id: id.into(),
        }
    }
}
