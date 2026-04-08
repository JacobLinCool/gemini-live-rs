//! Runtime-owned conversation memory abstractions.
//!
//! Live session resumption is only a short-lived continuity mechanism. Hosts
//! that want longer-lived conversational state need a second layer that can
//! survive session churn while the process stays alive.
//!
//! This module intentionally stays host-agnostic:
//!
//! - it stores protocol-native `Content` turns rather than host-specific text
//! - it tracks the latest resumable handle plus its issuance time
//! - it exposes a simple in-memory store that matches the current process-only
//!   requirement while leaving room for other stores later

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gemini_live::types::{ClientContent, Content};

/// The latest resumable session token known to the host runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumableSessionHandle {
    pub handle: String,
    pub issued_at: Instant,
}

/// Process-local conversational state used to rehydrate a fresh Live session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversationSnapshot {
    pub resumable_session: Option<ResumableSessionHandle>,
    pub rolling_summary: Option<Content>,
    pub recent_turns: Vec<Content>,
    pub last_activity_at: Option<Instant>,
}

impl ConversationSnapshot {
    /// Record user or model activity against the process-local snapshot.
    pub fn note_activity(&mut self, at: Instant) {
        self.last_activity_at = Some(at);
    }

    /// Replace the latest resumable handle with a newly issued token.
    pub fn install_resumable_handle(&mut self, handle: impl Into<String>, issued_at: Instant) {
        self.resumable_session = Some(ResumableSessionHandle {
            handle: handle.into(),
            issued_at,
        });
    }

    /// Drop any resumable handle currently associated with this snapshot.
    pub fn clear_resumable_handle(&mut self) {
        self.resumable_session = None;
    }

    /// Return the resumable handle when it is still inside the allowed age window.
    pub fn resumable_handle(&self, now: Instant, max_age: Duration) -> Option<&str> {
        let resumable = self.resumable_session.as_ref()?;
        if now.duration_since(resumable.issued_at) <= max_age {
            Some(resumable.handle.as_str())
        } else {
            None
        }
    }

    /// Cap the number of recent turns retained for later rehydration.
    pub fn trim_recent_turns(&mut self, max_turns: usize) {
        if self.recent_turns.len() <= max_turns {
            return;
        }

        let retained = self
            .recent_turns
            .split_off(self.recent_turns.len() - max_turns);
        self.recent_turns = retained;
    }

    /// Append one additional recent turn and enforce the configured cap.
    pub fn push_recent_turn(&mut self, turn: Content, max_turns: usize) {
        self.recent_turns.push(turn);
        self.trim_recent_turns(max_turns);
    }

    /// Replace the rolling summary used for fresh-session rehydration.
    pub fn set_rolling_summary(&mut self, summary: Option<Content>) {
        self.rolling_summary = summary;
    }

    /// Build initial history suitable for a fresh-session rehydrate step.
    pub fn build_rehydrate_content(&self) -> Option<ClientContent> {
        let mut turns = Vec::new();
        if let Some(summary) = &self.rolling_summary {
            turns.push(summary.clone());
        }
        turns.extend(self.recent_turns.clone());

        if turns.is_empty() {
            None
        } else {
            Some(ClientContent {
                turns: Some(turns),
                turn_complete: Some(true),
            })
        }
    }
}

/// Storage abstraction for process-local conversation memory.
pub trait ConversationMemoryStore: Send + Sync + 'static {
    fn load_snapshot(&self) -> ConversationSnapshot;
    fn store_snapshot(&self, snapshot: ConversationSnapshot);
}

/// Default in-process conversation memory store.
#[derive(Debug, Clone, Default)]
pub struct InMemoryConversationMemory {
    snapshot: Arc<Mutex<ConversationSnapshot>>,
}

impl InMemoryConversationMemory {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ConversationMemoryStore for InMemoryConversationMemory {
    fn load_snapshot(&self) -> ConversationSnapshot {
        self.snapshot
            .lock()
            .expect("conversation snapshot lock")
            .clone()
    }

    fn store_snapshot(&self, snapshot: ConversationSnapshot) {
        *self.snapshot.lock().expect("conversation snapshot lock") = snapshot;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn resumable_handle_expires_after_max_age() {
        let issued_at = Instant::now();
        let mut snapshot = ConversationSnapshot::default();
        snapshot.install_resumable_handle("resume-1", issued_at);

        assert_eq!(
            snapshot.resumable_handle(issued_at + Duration::from_secs(30), Duration::from_secs(60)),
            Some("resume-1")
        );
        assert_eq!(
            snapshot.resumable_handle(issued_at + Duration::from_secs(61), Duration::from_secs(60)),
            None
        );
    }

    #[test]
    fn rehydrate_content_prepends_summary_to_recent_turns() {
        let summary = Content {
            role: None,
            parts: Vec::new(),
        };
        let recent = Content {
            role: None,
            parts: Vec::new(),
        };
        let snapshot = ConversationSnapshot {
            resumable_session: None,
            rolling_summary: Some(summary.clone()),
            recent_turns: vec![recent.clone()],
            last_activity_at: None,
        };

        let content = snapshot
            .build_rehydrate_content()
            .expect("rehydrate content");

        assert_eq!(content.turns, Some(vec![summary, recent]));
        assert_eq!(content.turn_complete, Some(true));
    }
}
