//! Shared runtime events for hosts that want a richer lifecycle surface than
//! raw `ServerEvent` alone.

use gemini_live::ServerEvent;

/// Observable runtime events emitted by a host-managed runtime loop.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    Lifecycle(RuntimeLifecycleEvent),
    Server(ServerEvent),
    Lagged {
        count: u64,
    },
    ToolCallStarted {
        id: String,
        name: String,
    },
    ToolCallFinished {
        id: String,
        name: String,
        outcome: ToolCallOutcome,
    },
    SendFailed(RuntimeSendFailure),
}

/// High-level runtime lifecycle states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeLifecycleEvent {
    Connecting,
    Connected,
    Reconnecting,
    AppliedResumedSession,
    AppliedFreshSession,
    Closed { reason: String },
}

/// The result of a tool-call attempt from the runtime's perspective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallOutcome {
    Responded,
    Cancelled,
    Failed { reason: String },
}

/// A send failure that hosts can render or log with product-specific wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSendFailure {
    pub operation: RuntimeSendOperation,
    pub reason: String,
}

/// Categories of outbound runtime operations that may fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSendOperation {
    Raw,
    Text,
    Audio,
    Video,
    ToolResponse,
    SessionClose,
}
