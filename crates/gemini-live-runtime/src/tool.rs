//! Reusable tool-execution contracts for host applications.
//!
//! Host crates remain responsible for their own concrete tool sets. This
//! module only defines the shared boundary between a Gemini Live host and the
//! local executor that handles `toolCall` requests.

use futures_util::future::BoxFuture;
use gemini_live::types::{FunctionCallRequest, FunctionResponse, Tool};

use crate::error::ToolExecutionError;

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

/// Host-provided tool execution surface.
pub trait ToolAdapter: Send + Sync + 'static {
    /// Full `setup.tools` payload advertised for the next session connect.
    fn advertised_tools(&self) -> Option<Vec<Tool>> {
        None
    }

    /// User-facing catalog metadata for this adapter's tools.
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }

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

/// Empty adapter used by hosts that do not expose local tool execution.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopToolAdapter;

impl ToolAdapter for NoopToolAdapter {
    fn execute<'a>(
        &'a self,
        call: FunctionCallRequest,
    ) -> BoxFuture<'a, Result<FunctionResponse, ToolExecutionError>> {
        Box::pin(async move { Err(ToolExecutionError::unsupported_function(call.name)) })
    }
}
