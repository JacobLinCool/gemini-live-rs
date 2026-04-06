//! Reusable runtime abstractions that sit above [`gemini_live`] and below
//! concrete hosts such as the desktop CLI or a future Discord voice bot.
//!
//! This crate is intentionally narrower than a generic "utils" bucket:
//!
//! - `gemini-live` remains the wire-level client and session library
//! - `gemini-live-runtime` owns reusable session orchestration contracts
//! - host crates own product-specific UI, device I/O, and persistence
//!
//! The initial surface focuses on:
//!
//! - staged vs active `setup` management
//! - a testable session-driver boundary above `gemini_live::Session`
//! - reusable runtime event and tool-execution contracts
//! - a managed runtime loop for session forwarding and tool orchestration

pub mod config;
pub mod driver;
pub mod error;
pub mod event;
pub mod managed;
pub mod runtime;
pub mod tool;

pub use config::{Patch, RuntimeConfig, SetupPatch};
pub use driver::{
    GeminiSessionDriver, GeminiSessionHandle, RuntimeSession, RuntimeSessionObservation,
    SessionDriver,
};
pub use error::{RuntimeError, ToolExecutionError};
pub use event::{
    RuntimeEvent, RuntimeLifecycleEvent, RuntimeSendFailure, RuntimeSendOperation, ToolCallOutcome,
};
pub use managed::{ManagedRuntime, RuntimeEventReceiver};
pub use runtime::{ApplyReport, LiveRuntime};
pub use tool::{NoopToolAdapter, ToolAdapter, ToolDescriptor, ToolKind};
