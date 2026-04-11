//! Passive notification delivery loop for idle model sessions.
//!
//! The harness owns the durable notification queue and the reusable loop that
//! decides when queued notifications should be offered back to the model. Host
//! applications still provide the session-specific gate ("is it safe to
//! interrupt now?") and the concrete send operation.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::error::HarnessError;
use crate::notification::{HarnessNotification, NotificationStatus};
use crate::store::Harness;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// One queued passive notification that should be injected into the model.
#[derive(Debug, Clone, PartialEq)]
pub struct PassiveNotificationDelivery {
    pub notification: HarnessNotification,
    pub prompt: String,
}

/// Reusable passive notification pump backed by durable harness state.
#[derive(Debug, Clone)]
pub struct PassiveNotificationPump {
    harness: Harness,
    interval: Duration,
    in_flight_notification_id: Arc<Mutex<Option<String>>>,
}

impl PassiveNotificationPump {
    pub fn new(harness: Harness) -> Self {
        Self {
            harness,
            interval: DEFAULT_INTERVAL,
            in_flight_notification_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_interval(harness: Harness, interval: Duration) -> Self {
        Self {
            harness,
            interval,
            in_flight_notification_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn harness(&self) -> &Harness {
        &self.harness
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn current_in_flight_notification_id(&self) -> Option<String> {
        self.in_flight_notification_id
            .lock()
            .expect("notification pump in-flight lock")
            .clone()
    }

    /// Requeue any notification left in `delivered` state by an interrupted
    /// earlier process. This favors durable eventual delivery over silently
    /// losing a queued system event.
    pub fn recover_orphaned_deliveries(&self) -> Result<Vec<HarnessNotification>, HarnessError> {
        let delivered = self
            .harness
            .list_notifications(Some(NotificationStatus::Delivered), usize::MAX)?;
        let mut recovered = Vec::new();
        for notification in delivered {
            recovered.push(self.harness.requeue_notification(&notification.id)?);
        }
        Ok(recovered)
    }

    pub fn acknowledge_in_flight(&self) -> Result<Option<HarnessNotification>, HarnessError> {
        let Some(notification_id) = self.take_in_flight_notification_id() else {
            return Ok(None);
        };
        self.harness
            .acknowledge_notification(&notification_id)
            .map(Some)
    }

    pub fn requeue_in_flight(&self) -> Result<Option<HarnessNotification>, HarnessError> {
        let Some(notification_id) = self.take_in_flight_notification_id() else {
            return Ok(None);
        };
        self.harness
            .requeue_notification(&notification_id)
            .map(Some)
    }

    pub fn spawn<C, CFut, D, DFut, E>(&self, can_deliver: C, deliver: D) -> JoinHandle<()>
    where
        C: Fn() -> CFut + Send + Sync + 'static,
        CFut: Future<Output = bool> + Send + 'static,
        D: Fn(PassiveNotificationDelivery) -> DFut + Send + Sync + 'static,
        DFut: Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let pump = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(pump.interval);
            loop {
                interval.tick().await;
                if !can_deliver().await {
                    continue;
                }
                match pump.dispatch_once(&deliver).await {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!("passive harness notification dispatch failed: {error}")
                    }
                }
            }
        })
    }

    pub async fn dispatch_once<D, DFut, E>(&self, deliver: &D) -> Result<(), HarnessError>
    where
        D: Fn(PassiveNotificationDelivery) -> DFut + Send + Sync,
        DFut: Future<Output = Result<(), E>> + Send,
        E: std::fmt::Display + Send,
    {
        if self.current_in_flight_notification_id().is_some() {
            return Ok(());
        }
        let Some(notification) = self
            .harness
            .list_notifications(Some(NotificationStatus::Queued), 1)?
            .into_iter()
            .next()
        else {
            return Ok(());
        };

        let delivery = PassiveNotificationDelivery {
            prompt: format_passive_notification_prompt(&notification),
            notification,
        };

        match deliver(delivery.clone()).await {
            Ok(()) => {
                self.harness
                    .mark_notification_delivered(&delivery.notification.id)?;
                self.set_in_flight_notification_id(Some(delivery.notification.id));
                Ok(())
            }
            Err(error) => Err(HarnessError::InvalidSegment {
                kind: "notification_delivery",
                value: error.to_string(),
            }),
        }
    }

    fn set_in_flight_notification_id(&self, notification_id: Option<String>) {
        *self
            .in_flight_notification_id
            .lock()
            .expect("notification pump in-flight lock") = notification_id;
    }

    fn take_in_flight_notification_id(&self) -> Option<String> {
        self.in_flight_notification_id
            .lock()
            .expect("notification pump in-flight lock")
            .take()
    }
}

/// Render a durable harness notification into a model-facing prompt.
pub fn format_passive_notification_prompt(notification: &HarnessNotification) -> String {
    let task_fragment = notification
        .task_id
        .as_deref()
        .map(|task_id| format!("Task ID: {task_id}\n"))
        .unwrap_or_default();
    format!(
        concat!(
            "[System Notification]\n",
            "A background task or durable event needs a user-facing follow-up.\n",
            "Review the notification and report the important result back to the user.\n",
            "Notification ID: {}\n",
            "{}",
            "Title: {}\n",
            "Body: {}\n",
            "[/System Notification]"
        ),
        notification.id, task_fragment, notification.title, notification.body,
    )
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;
    use crate::notification::{HarnessNotification, NewNotification, NotificationKind};
    use crate::store::Harness;

    fn temp_harness() -> Harness {
        let path = std::env::temp_dir().join(format!(
            "gemini-live-harness-delivery-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ));
        Harness::open(path).expect("open harness")
    }

    #[test]
    fn formats_passive_notification_prompt_with_task_context() {
        let prompt = format_passive_notification_prompt(&HarnessNotification {
            id: "notification_1".into(),
            kind: NotificationKind::Generic,
            created_at_ms: 1,
            updated_at_ms: 1,
            status: NotificationStatus::Queued,
            task_id: Some("task_1".into()),
            title: "Task completed".into(),
            body: "The work finished.".into(),
            payload: Some(json!({ "answer": 42 })),
            delivered_at_ms: None,
            acknowledged_at_ms: None,
        });
        assert!(prompt.contains("Notification ID: notification_1"));
        assert!(prompt.contains("Task ID: task_1"));
        assert!(prompt.contains("Title: Task completed"));
    }

    #[test]
    fn orphaned_deliveries_are_requeued() {
        let harness = temp_harness();
        let notification = harness
            .enqueue_notification(NewNotification {
                kind: NotificationKind::Generic,
                task_id: None,
                title: "Ping".into(),
                body: "Hello".into(),
                payload: None,
            })
            .expect("enqueue");
        harness
            .mark_notification_delivered(&notification.id)
            .expect("deliver");

        let pump = PassiveNotificationPump::new(harness.clone());
        let recovered = pump
            .recover_orphaned_deliveries()
            .expect("recover deliveries");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, NotificationStatus::Queued);
    }
}
