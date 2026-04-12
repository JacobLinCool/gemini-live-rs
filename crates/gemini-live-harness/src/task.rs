//! Durable task metadata and lifecycle records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fs::{next_id, now_ms};

/// High-level durable status for one persisted harness task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

/// Process/runtime instance metadata for a task that is already executing.
///
/// The harness does not treat tasks as a durable work queue with worker
/// claiming. Instead, background tool tasks are created after a blocking tool
/// call has already begun running inside the current host process. These
/// fields identify which runtime instance owns that execution so a later
/// process restart can mark stale `Running` tasks as `Interrupted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRuntimeInstance {
    pub instance_id: String,
    pub pid: u32,
    pub started_at_ms: u64,
}

impl TaskRuntimeInstance {
    pub fn current() -> Self {
        Self {
            instance_id: next_id("runtime"),
            pid: std::process::id(),
            started_at_ms: now_ms(),
        }
    }
}

/// Primary persisted task record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessTask {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub status: TaskStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub requested_by: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub runtime: Option<TaskRuntimeInstance>,
    #[serde(default)]
    pub origin_call_id: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Request to persist a task that is already running inside the current
/// runtime instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewRunningTask {
    pub title: String,
    pub instructions: String,
    pub runtime: TaskRuntimeInstance,
    #[serde(default)]
    pub requested_by: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub origin_call_id: Option<String>,
}

/// Persisted terminal result for a successful task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    pub task_id: String,
    pub completed_at_ms: u64,
    #[serde(default)]
    pub summary: Option<String>,
    pub output: Value,
}

/// Append-only event stream for task lifecycle changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEvent {
    pub sequence: u64,
    pub recorded_at_ms: u64,
    #[serde(flatten)]
    pub kind: TaskEventKind,
}

/// Event payload variants stored in `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TaskEventKind {
    Created,
    Started {
        runtime_instance_id: String,
        pid: u32,
    },
    Progress {
        message: String,
        #[serde(default)]
        details: Option<Value>,
    },
    Succeeded {
        #[serde(default)]
        summary: Option<String>,
    },
    Failed {
        message: String,
    },
    Cancelled {
        #[serde(default)]
        reason: Option<String>,
    },
    Interrupted {
        #[serde(default)]
        runtime_instance_id: Option<String>,
        #[serde(default)]
        pid: Option<u32>,
        reason: String,
    },
}

/// Rich task projection returned to hosts and tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetail {
    pub task: HarnessTask,
    #[serde(default)]
    pub result: Option<TaskResult>,
    #[serde(default)]
    pub events: Vec<TaskEvent>,
}
