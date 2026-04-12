//! Reusable runtime abstractions that sit above [`gemini_live`] and below
//! concrete hosts such as the desktop CLI or a future Discord voice bot.
//!
//! This crate is intentionally narrower than a generic "utils" bucket:
//!
//! - `gemini-live` remains the wire-level client and session library
//! - `gemini-live-runtime` owns reusable session orchestration contracts
//! - `gemini-live-harness` owns durable state and shared tool execution policy
//! - host crates own product-specific UI and device I/O
//!
//! The initial surface focuses on:
//!
//! - staged vs active `setup` management
//! - a testable session-driver boundary above `gemini_live::Session`
//! - reusable runtime events for session forwarding and tool-call request fanout
//! - a managed runtime loop for session forwarding above the Live session layer
//! - process-local conversation memory and hot/dormant lifecycle abstractions

pub mod config;
pub mod driver;
pub mod error;
pub mod event;
pub mod managed;
pub mod memory;
pub mod runtime;
pub mod session_manager;

pub use config::{Patch, RuntimeConfig, SetupPatch};
pub use driver::{
    GeminiSessionDriver, GeminiSessionHandle, RuntimeSession, RuntimeSessionObservation,
    SessionDriver,
};
pub use error::RuntimeError;
pub use event::{RuntimeEvent, RuntimeLifecycleEvent, RuntimeSendFailure, RuntimeSendOperation};
pub use managed::{ManagedRuntime, RuntimeEventReceiver};
pub use memory::{
    ConversationMemoryStore, ConversationSnapshot, InMemoryConversationMemory,
    ResumableSessionHandle,
};
pub use runtime::{ApplyReport, LiveRuntime};
pub use session_manager::{
    ActivityKind, IdleDecision, IdlePolicy, SessionLifecycleState, SessionManager, WakeOutcome,
    WakeReason, WakeStrategy,
};
