//! Shared host-fed tool surface definitions owned by the harness layer.
//!
//! The host is the source of truth for which downstream tools exist. The
//! harness consumes host-provided tool metadata and executors, then applies
//! orchestration policy such as inline budgets, background continuation, and
//! durable notification delivery.

use futures_util::future::BoxFuture;
use gemini_live::types::{FunctionCallRequest, FunctionResponse, Tool};

/// Failures while resolving or executing a host-defined tool call.
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

/// Metadata about a host-defined tool exposed to users or debuggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub key: String,
    pub summary: String,
    pub kind: ToolKind,
}

/// High-level tool categories for host-side presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    BuiltIn,
    Local,
    Remote,
}

/// Per-function capability declared by the host and consumed by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolCapability {
    pub can_continue_async_after_timeout: bool,
}

impl ToolCapability {
    pub const INLINE_ONLY: Self = Self {
        can_continue_async_after_timeout: false,
    };

    pub const BACKGROUND_CONTINUABLE: Self = Self {
        can_continue_async_after_timeout: true,
    };
}

/// One model-callable function declared by a host-fed provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpecification {
    pub function_name: String,
    pub capability: ToolCapability,
}

impl ToolSpecification {
    pub fn new(function_name: impl Into<String>, capability: ToolCapability) -> Self {
        Self {
            function_name: function_name.into(),
            capability,
        }
    }
}

/// Host-fed metadata surface describing which tools exist.
pub trait ToolProvider: Send + Sync + 'static {
    /// Full `setup.tools` payload advertised for the next session connect.
    fn advertised_tools(&self) -> Option<Vec<Tool>> {
        None
    }

    /// User-facing catalog metadata for this provider's tools.
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }

    /// Function-level capabilities for locally executable tools.
    fn specifications(&self) -> Vec<ToolSpecification> {
        Vec::new()
    }
}

/// Host-fed blocking execution surface for locally executable tools.
pub trait ToolExecutor: Send + Sync + 'static {
    /// Execute one server-requested function call.
    fn execute<'a>(
        &'a self,
        call: FunctionCallRequest,
    ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>>;

    /// Attempt to cancel an in-flight tool call by server `id`.
    fn cancel(&self, _call_id: &str) -> bool {
        false
    }
}

/// Empty downstream tool source used by hosts that do not expose local tools.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopToolSource;

impl ToolProvider for NoopToolSource {}

impl ToolExecutor for NoopToolSource {
    fn execute<'a>(
        &'a self,
        call: FunctionCallRequest,
    ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>> {
        Box::pin(async move { Err(ToolExecutionError::unsupported_function(call.name)) })
    }
}
