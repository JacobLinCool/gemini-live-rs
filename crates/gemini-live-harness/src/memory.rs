//! Durable long-running memory records.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Persisted harness memory entry addressed by `scope` and `key`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    pub scope: String,
    pub key: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    pub value: Value,
}

/// Request to upsert a durable memory record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryWrite {
    pub scope: String,
    pub key: String,
    pub value: Value,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}
