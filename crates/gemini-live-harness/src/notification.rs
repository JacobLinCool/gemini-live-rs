//! Durable notification queue records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Durable delivery state for a queued harness notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NotificationStatus {
    Queued,
    Delivered,
    Acknowledged,
}

/// Notification categories used by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NotificationKind {
    TaskSucceeded,
    TaskFailed,
    TaskCancelled,
    TaskInterrupted,
    Generic,
}

/// Persisted notification record stored under `notifications/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessNotification {
    pub id: String,
    pub kind: NotificationKind,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub status: NotificationStatus,
    #[serde(default)]
    pub task_id: Option<String>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub delivered_at_ms: Option<u64>,
    #[serde(default)]
    pub acknowledged_at_ms: Option<u64>,
}

/// Request to enqueue a durable notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewNotification {
    pub kind: NotificationKind,
    #[serde(default)]
    pub task_id: Option<String>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub payload: Option<Value>,
}
