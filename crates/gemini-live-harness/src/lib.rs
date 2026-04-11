//! Durable harness state that sits above `gemini-live-runtime` and below a
//! concrete host application.
//!
//! `gemini-live-runtime` already owns hot-session orchestration, tool-call
//! request fanout, and short-lived in-process conversation continuity. This
//! crate is the next layer up:
//!
//! - tasks are durable filesystem records under one harness root
//! - notifications are durable queue items, not ephemeral callbacks
//! - memory is stored on disk and may be inspected directly by other agents
//! - shared host-tool execution contracts live next to harness policy
//! - harness-owned wrappers may keep blocking host tools within an inline
//!   latency budget and spill eligible calls into durable background tasks
//! - `HarnessController` is the preferred host-facing boundary for combining
//!   tool execution and passive notification delivery
//!
//! The default root is `~/.gemini-live/harness`, but callers may supply any
//! alternative path for testing or custom deployments.

mod adapter;
mod bridge;
mod controller;
mod delivery;
mod error;
mod executor;
mod fs;
mod memory;
mod notification;
mod profile;
mod registry;
mod store;
mod task;

pub use adapter::{
    NoopToolSource, ToolCapability, ToolDescriptor, ToolExecutionError, ToolExecutor, ToolKind,
    ToolProvider, ToolSpecification,
};
pub use bridge::{HarnessRuntimeBridge, HarnessToolForwardFailure, HarnessToolForwardOutcome};
pub use controller::{HarnessController, HarnessToolCompletion, HarnessToolCompletionDisposition};
pub use delivery::{
    PassiveNotificationDelivery, PassiveNotificationPump, format_passive_notification_prompt,
};
pub use error::HarnessError;
pub use executor::{HarnessToolBudget, HarnessToolRuntime};
pub use memory::{MemoryRecord, MemoryWrite};
pub use notification::{
    HarnessNotification, NewNotification, NotificationKind, NotificationStatus,
};
pub use profile::HarnessProfileStore;
pub use registry::{HarnessToolRegistry, RegisteredTool};
pub use store::{Harness, HarnessPaths};
pub use task::{
    HarnessTask, NewRunningTask, TaskDetail, TaskEvent, TaskEventKind, TaskResult,
    TaskRuntimeInstance, TaskStatus,
};
