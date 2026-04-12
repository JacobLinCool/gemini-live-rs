//! File-backed harness stores and the top-level orchestration surface.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::sync::Notify;

use crate::error::HarnessError;
use crate::fs::{
    append_json_line, ensure_dir, list_child_dirs, list_child_files, next_id, now_ms, read_json,
    read_json_lines, remove_file_if_exists, validate_segment, write_json_atomic,
};
use crate::memory::{MemoryRecord, MemoryWrite};
use crate::notification::{
    HarnessNotification, NewNotification, NotificationKind, NotificationStatus,
};
use crate::task::{
    HarnessTask, NewRunningTask, TaskDetail, TaskEvent, TaskEventKind, TaskResult,
    TaskRuntimeInstance, TaskStatus,
};

const TASK_FILE_NAME: &str = "task.json";
const TASK_EVENTS_FILE_NAME: &str = "events.jsonl";
const TASK_RESULT_FILE_NAME: &str = "result.json";
const DEFAULT_PROFILE_FILE_NAME: &str = "default-profile.json";
const PROFILES_DIR_NAME: &str = "profiles";
const DEFAULT_PROFILE_NAME: &str = "default";

#[derive(Debug, Clone, Default)]
pub(crate) struct NotificationSignal {
    queue_version: Arc<AtomicU64>,
    queue_notify: Arc<Notify>,
}

impl NotificationSignal {
    pub(crate) fn current_version(&self) -> u64 {
        self.queue_version.load(Ordering::SeqCst)
    }

    pub(crate) fn notify_queue_changed(&self) {
        self.queue_version.fetch_add(1, Ordering::SeqCst);
        self.queue_notify.notify_waiters();
    }

    pub(crate) async fn wait_for_queue_change_since(&self, observed_version: u64) {
        loop {
            if self.current_version() != observed_version {
                return;
            }
            let notified = self.queue_notify.notified();
            if self.current_version() != observed_version {
                return;
            }
            notified.await;
        }
    }
}

/// File layout configuration for the durable harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessPaths {
    root: PathBuf,
}

impl HarnessPaths {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn base_root() -> Result<PathBuf, HarnessError> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let path = std::env::var_os("HOMEPATH")?;
                let mut combined = PathBuf::from(drive);
                combined.push(path);
                Some(combined.into_os_string())
            })
            .ok_or(HarnessError::HomeDirectoryUnavailable)?;
        Ok(PathBuf::from(home).join(".gemini-live").join("harness"))
    }

    pub fn default_root() -> Result<PathBuf, HarnessError> {
        let base_root = Self::base_root()?;
        let default_profile = read_default_profile_name(&base_root)?
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_string());
        Ok(Self::profile_root_for_base(&base_root, &default_profile))
    }

    pub fn profile_root(profile: &str) -> Result<PathBuf, HarnessError> {
        validate_segment("profile", profile)?;
        Ok(Self::profile_root_for_base(&Self::base_root()?, profile))
    }

    pub fn profile_root_for_base(base_root: &Path, profile: &str) -> PathBuf {
        base_root.join(PROFILES_DIR_NAME).join(profile)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    pub fn notifications_dir(&self) -> PathBuf {
        self.root.join("notifications")
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    pub fn ensure_layout(&self) -> Result<(), HarnessError> {
        ensure_dir(&self.tasks_dir())?;
        ensure_dir(&self.notifications_dir())?;
        ensure_dir(&self.memory_dir())?;
        Ok(())
    }

    pub fn task_dir(&self, task_id: &str) -> PathBuf {
        self.tasks_dir().join(task_id)
    }

    pub fn task_file(&self, task_id: &str) -> PathBuf {
        self.task_dir(task_id).join(TASK_FILE_NAME)
    }

    pub fn task_events_file(&self, task_id: &str) -> PathBuf {
        self.task_dir(task_id).join(TASK_EVENTS_FILE_NAME)
    }

    pub fn task_result_file(&self, task_id: &str) -> PathBuf {
        self.task_dir(task_id).join(TASK_RESULT_FILE_NAME)
    }

    pub fn notification_file(&self, notification_id: &str) -> PathBuf {
        self.notifications_dir()
            .join(format!("{notification_id}.json"))
    }

    pub fn memory_scope_dir(&self, scope: &str) -> PathBuf {
        self.memory_dir().join(scope)
    }

    pub fn memory_file(&self, scope: &str, key: &str) -> PathBuf {
        self.memory_scope_dir(scope).join(format!("{key}.json"))
    }
}

/// Top-level file-backed durable harness.
#[derive(Debug, Clone)]
pub struct Harness {
    paths: HarnessPaths,
    notification_signal: NotificationSignal,
}

impl Harness {
    pub fn new(paths: HarnessPaths) -> Result<Self, HarnessError> {
        paths.ensure_layout()?;
        Ok(Self {
            paths,
            notification_signal: NotificationSignal::default(),
        })
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Self, HarnessError> {
        Self::new(HarnessPaths::new(root.into()))
    }

    pub fn open_default() -> Result<Self, HarnessError> {
        Self::new(HarnessPaths::new(HarnessPaths::default_root()?))
    }

    pub fn open_profile(profile: &str) -> Result<Self, HarnessError> {
        Self::new(HarnessPaths::new(HarnessPaths::profile_root(profile)?))
    }

    pub fn paths(&self) -> &HarnessPaths {
        &self.paths
    }

    pub(crate) fn notification_signal(&self) -> NotificationSignal {
        self.notification_signal.clone()
    }

    pub fn start_task(&self, request: NewRunningTask) -> Result<HarnessTask, HarnessError> {
        let task_id = next_id("task");
        let task_dir = self.paths.task_dir(&task_id);
        ensure_dir(&task_dir)?;

        let now = now_ms();
        let task = HarnessTask {
            id: task_id.clone(),
            title: request.title,
            instructions: request.instructions,
            status: TaskStatus::Running,
            created_at_ms: now,
            updated_at_ms: now,
            requested_by: request.requested_by,
            tags: request.tags,
            metadata: request.metadata,
            runtime: Some(request.runtime.clone()),
            origin_call_id: request.origin_call_id,
            summary: None,
            last_error: None,
        };
        self.write_task(&task)?;
        self.append_task_event(&task.id, TaskEventKind::Created, now)?;
        self.append_task_event(
            &task.id,
            TaskEventKind::Started {
                runtime_instance_id: request.runtime.instance_id,
                pid: request.runtime.pid,
            },
            now,
        )?;
        Ok(task)
    }

    pub fn list_tasks(
        &self,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> Result<Vec<HarnessTask>, HarnessError> {
        let mut tasks = Vec::new();
        for task_dir in list_child_dirs(&self.paths.tasks_dir())? {
            let task_path = task_dir.join(TASK_FILE_NAME);
            if !task_path.exists() {
                continue;
            }
            let task: HarnessTask = read_json(&task_path)?;
            if status.is_none_or(|expected| task.status == expected) {
                tasks.push(task);
            }
        }
        tasks.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        tasks.truncate(limit);
        Ok(tasks)
    }

    pub fn read_task(&self, task_id: &str) -> Result<HarnessTask, HarnessError> {
        validate_segment("task_id", task_id)?;
        let path = self.paths.task_file(task_id);
        if !path.exists() {
            return Err(HarnessError::not_found("task", task_id));
        }
        read_json(&path)
    }

    pub fn task_detail(
        &self,
        task_id: &str,
        event_limit: usize,
    ) -> Result<TaskDetail, HarnessError> {
        let task = self.read_task(task_id)?;
        let mut events = self.read_task_events(task_id)?;
        if events.len() > event_limit {
            events = events.split_off(events.len() - event_limit);
        }
        let result = self.read_task_result(task_id)?;
        Ok(TaskDetail {
            task,
            result,
            events,
        })
    }

    pub fn record_task_progress(
        &self,
        task_id: &str,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Result<HarnessTask, HarnessError> {
        let mut task = self.read_task(task_id)?;
        if task.status != TaskStatus::Running {
            return Err(HarnessError::TaskNotRunning {
                id: task.id.clone(),
            });
        }

        let now = now_ms();
        task.updated_at_ms = now;
        self.write_task(&task)?;
        self.append_task_event(
            &task.id,
            TaskEventKind::Progress {
                message: message.into(),
                details,
            },
            now,
        )?;
        Ok(task)
    }

    pub fn interrupt_stale_running_tasks(
        &self,
        current_runtime_instance_id: &str,
    ) -> Result<Vec<HarnessTask>, HarnessError> {
        let mut interrupted = Vec::new();
        for task in self.list_tasks(Some(TaskStatus::Running), usize::MAX)? {
            let Some(runtime) = task.runtime.as_ref() else {
                interrupted.push(self.interrupt_task(
                    &task.id,
                    None,
                    "Running task was left behind by an older harness runtime and had no runtime ownership metadata.".into(),
                )?);
                continue;
            };
            if runtime.instance_id == current_runtime_instance_id {
                continue;
            }
            interrupted.push(self.interrupt_task(
                &task.id,
                Some(runtime),
                format!(
                    "Task was still running when harness runtime instance `{}` (pid {}) stopped.",
                    runtime.instance_id, runtime.pid
                ),
            )?);
        }
        Ok(interrupted)
    }

    pub fn complete_task(
        &self,
        task_id: &str,
        summary: Option<String>,
        output: Value,
    ) -> Result<HarnessTask, HarnessError> {
        let mut task = self.read_task(task_id)?;
        if task.status.is_terminal() {
            return Err(HarnessError::TaskAlreadyTerminal {
                id: task.id,
                status: format!("{:?}", task.status),
            });
        }

        let now = now_ms();
        task.status = TaskStatus::Succeeded;
        task.updated_at_ms = now;
        task.summary = summary.clone();
        task.last_error = None;
        self.write_task(&task)?;
        self.write_task_result(&TaskResult {
            task_id: task.id.clone(),
            completed_at_ms: now,
            summary: summary.clone(),
            output,
        })?;
        self.append_task_event(
            &task.id,
            TaskEventKind::Succeeded {
                summary: summary.clone(),
            },
            now,
        )?;
        self.enqueue_notification(NewNotification {
            kind: NotificationKind::TaskSucceeded,
            task_id: Some(task.id.clone()),
            title: format!("Task completed: {}", task.title),
            body: summary.unwrap_or_else(|| format!("Task `{}` completed successfully.", task.id)),
            payload: Some(serde_json::json!({
                "taskId": task.id,
                "status": "succeeded",
            })),
        })?;
        Ok(task)
    }

    pub fn fail_task(
        &self,
        task_id: &str,
        message: impl Into<String>,
    ) -> Result<HarnessTask, HarnessError> {
        let mut task = self.read_task(task_id)?;
        if task.status.is_terminal() {
            return Err(HarnessError::TaskAlreadyTerminal {
                id: task.id,
                status: format!("{:?}", task.status),
            });
        }

        let message = message.into();
        let now = now_ms();
        task.status = TaskStatus::Failed;
        task.updated_at_ms = now;
        task.last_error = Some(message.clone());
        self.write_task(&task)?;
        remove_file_if_exists(&self.paths.task_result_file(&task.id))?;
        self.append_task_event(
            &task.id,
            TaskEventKind::Failed {
                message: message.clone(),
            },
            now,
        )?;
        self.enqueue_notification(NewNotification {
            kind: NotificationKind::TaskFailed,
            task_id: Some(task.id.clone()),
            title: format!("Task failed: {}", task.title),
            body: message.clone(),
            payload: Some(serde_json::json!({
                "taskId": task.id,
                "status": "failed",
                "message": message,
            })),
        })?;
        Ok(task)
    }

    pub fn cancel_task(
        &self,
        task_id: &str,
        reason: Option<String>,
    ) -> Result<HarnessTask, HarnessError> {
        let mut task = self.read_task(task_id)?;
        if task.status.is_terminal() {
            return Err(HarnessError::TaskAlreadyTerminal {
                id: task.id,
                status: format!("{:?}", task.status),
            });
        }

        let now = now_ms();
        task.status = TaskStatus::Cancelled;
        task.updated_at_ms = now;
        task.last_error = reason.clone();
        self.write_task(&task)?;
        remove_file_if_exists(&self.paths.task_result_file(&task.id))?;
        self.append_task_event(
            &task.id,
            TaskEventKind::Cancelled {
                reason: reason.clone(),
            },
            now,
        )?;
        self.enqueue_notification(NewNotification {
            kind: NotificationKind::TaskCancelled,
            task_id: Some(task.id.clone()),
            title: format!("Task cancelled: {}", task.title),
            body: reason
                .clone()
                .unwrap_or_else(|| format!("Task `{}` was cancelled.", task.id)),
            payload: Some(serde_json::json!({
                "taskId": task.id,
                "status": "cancelled",
            })),
        })?;
        Ok(task)
    }

    pub fn enqueue_notification(
        &self,
        request: NewNotification,
    ) -> Result<HarnessNotification, HarnessError> {
        let notification_id = next_id("notification");
        let now = now_ms();
        let notification = HarnessNotification {
            id: notification_id.clone(),
            kind: request.kind,
            created_at_ms: now,
            updated_at_ms: now,
            status: NotificationStatus::Queued,
            task_id: request.task_id,
            title: request.title,
            body: request.body,
            payload: request.payload,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
        };
        self.write_notification(&notification)?;
        self.notification_signal.notify_queue_changed();
        Ok(notification)
    }

    pub fn list_notifications(
        &self,
        status: Option<NotificationStatus>,
        limit: usize,
    ) -> Result<Vec<HarnessNotification>, HarnessError> {
        let mut notifications = Vec::new();
        for path in list_child_files(&self.paths.notifications_dir())? {
            if path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            let notification: HarnessNotification = read_json(&path)?;
            if status.is_none_or(|expected| notification.status == expected) {
                notifications.push(notification);
            }
        }
        notifications.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        notifications.truncate(limit);
        Ok(notifications)
    }

    pub fn mark_notification_delivered(
        &self,
        notification_id: &str,
    ) -> Result<HarnessNotification, HarnessError> {
        let mut notification = self.read_notification(notification_id)?;
        match notification.status {
            NotificationStatus::Queued => {
                let now = now_ms();
                notification.status = NotificationStatus::Delivered;
                notification.updated_at_ms = now;
                notification.delivered_at_ms = Some(now);
                self.write_notification(&notification)?;
                self.notification_signal.notify_queue_changed();
                Ok(notification)
            }
            NotificationStatus::Delivered => Ok(notification),
            NotificationStatus::Acknowledged => Err(HarnessError::NotificationStatusConflict {
                id: notification.id,
                from: "acknowledged".into(),
                to: "delivered",
            }),
        }
    }

    pub fn acknowledge_notification(
        &self,
        notification_id: &str,
    ) -> Result<HarnessNotification, HarnessError> {
        let mut notification = self.read_notification(notification_id)?;
        match notification.status {
            NotificationStatus::Queued | NotificationStatus::Delivered => {
                let now = now_ms();
                notification.status = NotificationStatus::Acknowledged;
                notification.updated_at_ms = now;
                if notification.delivered_at_ms.is_none() {
                    notification.delivered_at_ms = Some(now);
                }
                notification.acknowledged_at_ms = Some(now);
                self.write_notification(&notification)?;
                self.notification_signal.notify_queue_changed();
                Ok(notification)
            }
            NotificationStatus::Acknowledged => Ok(notification),
        }
    }

    pub fn requeue_notification(
        &self,
        notification_id: &str,
    ) -> Result<HarnessNotification, HarnessError> {
        let mut notification = self.read_notification(notification_id)?;
        match notification.status {
            NotificationStatus::Queued => Ok(notification),
            NotificationStatus::Delivered => {
                let now = now_ms();
                notification.status = NotificationStatus::Queued;
                notification.updated_at_ms = now;
                notification.delivered_at_ms = None;
                self.write_notification(&notification)?;
                self.notification_signal.notify_queue_changed();
                Ok(notification)
            }
            NotificationStatus::Acknowledged => Err(HarnessError::NotificationStatusConflict {
                id: notification.id,
                from: "acknowledged".into(),
                to: "queued",
            }),
        }
    }

    pub fn read_memory(&self, scope: &str, key: &str) -> Result<MemoryRecord, HarnessError> {
        validate_segment("scope", scope)?;
        validate_segment("key", key)?;
        let path = self.paths.memory_file(scope, key);
        if !path.exists() {
            return Err(HarnessError::not_found("memory", format!("{scope}/{key}")));
        }
        read_json(&path)
    }

    pub fn write_memory(&self, request: MemoryWrite) -> Result<MemoryRecord, HarnessError> {
        validate_segment("scope", &request.scope)?;
        validate_segment("key", &request.key)?;
        ensure_dir(&self.paths.memory_scope_dir(&request.scope))?;

        let existing = self.read_memory(&request.scope, &request.key).ok();
        let now = now_ms();
        let record = MemoryRecord {
            scope: request.scope,
            key: request.key,
            created_at_ms: existing.as_ref().map_or(now, |record| record.created_at_ms),
            updated_at_ms: now,
            summary: request.summary,
            metadata: request.metadata,
            value: request.value,
        };
        self.write_memory_record(&record)?;
        Ok(record)
    }

    pub fn list_memory(
        &self,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, HarnessError> {
        let scope_dirs = if let Some(scope) = scope {
            validate_segment("scope", scope)?;
            vec![self.paths.memory_scope_dir(scope)]
        } else {
            list_child_dirs(&self.paths.memory_dir())?
        };

        let mut records: Vec<MemoryRecord> = Vec::new();
        for scope_dir in scope_dirs {
            for path in list_child_files(&scope_dir)? {
                if path.extension() != Some(OsStr::new("json")) {
                    continue;
                }
                records.push(read_json(&path)?);
            }
        }
        records.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.scope.cmp(&right.scope))
                .then_with(|| left.key.cmp(&right.key))
        });
        records.truncate(limit);
        Ok(records)
    }

    pub fn delete_memory(&self, scope: &str, key: &str) -> Result<(), HarnessError> {
        validate_segment("scope", scope)?;
        validate_segment("key", key)?;
        remove_file_if_exists(&self.paths.memory_file(scope, key))
    }

    fn read_notification(
        &self,
        notification_id: &str,
    ) -> Result<HarnessNotification, HarnessError> {
        validate_segment("notification_id", notification_id)?;
        let path = self.paths.notification_file(notification_id);
        if !path.exists() {
            return Err(HarnessError::not_found("notification", notification_id));
        }
        read_json(&path)
    }

    fn write_task(&self, task: &HarnessTask) -> Result<(), HarnessError> {
        ensure_dir(&self.paths.task_dir(&task.id))?;
        write_json_atomic(&self.paths.task_file(&task.id), task)
    }

    fn read_task_events(&self, task_id: &str) -> Result<Vec<TaskEvent>, HarnessError> {
        read_json_lines(&self.paths.task_events_file(task_id))
    }

    fn append_task_event(
        &self,
        task_id: &str,
        kind: TaskEventKind,
        recorded_at_ms: u64,
    ) -> Result<(), HarnessError> {
        let sequence = self.read_task_events(task_id)?.len() as u64 + 1;
        append_json_line(
            &self.paths.task_events_file(task_id),
            &TaskEvent {
                sequence,
                recorded_at_ms,
                kind,
            },
        )
    }

    fn write_task_result(&self, result: &TaskResult) -> Result<(), HarnessError> {
        write_json_atomic(&self.paths.task_result_file(&result.task_id), result)
    }

    fn read_task_result(&self, task_id: &str) -> Result<Option<TaskResult>, HarnessError> {
        let path = self.paths.task_result_file(task_id);
        if !path.exists() {
            return Ok(None);
        }
        read_json(&path).map(Some)
    }

    fn write_notification(&self, notification: &HarnessNotification) -> Result<(), HarnessError> {
        write_json_atomic(
            &self.paths.notification_file(&notification.id),
            notification,
        )
    }

    fn write_memory_record(&self, record: &MemoryRecord) -> Result<(), HarnessError> {
        write_json_atomic(&self.paths.memory_file(&record.scope, &record.key), record)
    }

    fn interrupt_task(
        &self,
        task_id: &str,
        runtime: Option<&TaskRuntimeInstance>,
        reason: String,
    ) -> Result<HarnessTask, HarnessError> {
        let mut task = self.read_task(task_id)?;
        if task.status != TaskStatus::Running {
            return Ok(task);
        }

        let now = now_ms();
        task.status = TaskStatus::Interrupted;
        task.updated_at_ms = now;
        task.last_error = Some(reason.clone());
        self.write_task(&task)?;
        remove_file_if_exists(&self.paths.task_result_file(&task.id))?;
        self.append_task_event(
            &task.id,
            TaskEventKind::Interrupted {
                runtime_instance_id: runtime.map(|value| value.instance_id.clone()),
                pid: runtime.map(|value| value.pid),
                reason: reason.clone(),
            },
            now,
        )?;
        self.enqueue_notification(NewNotification {
            kind: NotificationKind::TaskInterrupted,
            task_id: Some(task.id.clone()),
            title: format!("Task interrupted: {}", task.title),
            body: reason.clone(),
            payload: Some(serde_json::json!({
                "taskId": task.id,
                "status": "interrupted",
                "message": reason,
                "runtimeInstanceId": runtime.map(|value| value.instance_id.clone()),
                "pid": runtime.map(|value| value.pid),
            })),
        })?;
        Ok(task)
    }
}

fn read_default_profile_name(base_root: &Path) -> Result<Option<String>, HarnessError> {
    let path = base_root.join(DEFAULT_PROFILE_FILE_NAME);
    if !path.exists() {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DefaultProfileRecord {
        default_profile: String,
    }
    let record: DefaultProfileRecord = read_json(&path)?;
    validate_segment("profile", &record.default_profile)?;
    Ok(Some(record.default_profile))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;
    use crate::task::TaskStatus;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_root() -> PathBuf {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gemini-live-harness-test-{}-{unique}-{counter}",
            std::process::id(),
        ));
        if path.exists() {
            std::fs::remove_dir_all(&path).expect("remove stale test dir");
        }
        path
    }

    #[test]
    fn task_lifecycle_persists_result_and_notification() {
        let root = temp_root();
        let harness = Harness::open(&root).expect("open harness");

        let runtime = TaskRuntimeInstance::current();
        let running = harness
            .start_task(NewRunningTask {
                title: "Research".into(),
                instructions: "Look up the latest docs".into(),
                runtime: runtime.clone(),
                requested_by: Some("model".into()),
                tags: vec!["research".into()],
                metadata: Some(json!({ "scope": "docs" })),
                origin_call_id: Some("call_1".into()),
            })
            .expect("start task");
        assert_eq!(running.status, TaskStatus::Running);
        assert_eq!(running.runtime.as_ref(), Some(&runtime));
        assert_eq!(running.origin_call_id.as_deref(), Some("call_1"));

        harness
            .record_task_progress(&running.id, "halfway", Some(json!({ "done": 1 })))
            .expect("record progress");

        harness
            .complete_task(
                &running.id,
                Some("Found the answer".into()),
                json!({ "answer": 42 }),
            )
            .expect("complete task");

        let detail = harness.task_detail(&running.id, 10).expect("task detail");
        assert_eq!(detail.task.status, TaskStatus::Succeeded);
        assert_eq!(
            detail.result.expect("task result").output,
            json!({ "answer": 42 })
        );
        assert_eq!(detail.events.len(), 4);

        let notifications = harness
            .list_notifications(Some(NotificationStatus::Queued), 10)
            .expect("list notifications");
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0].task_id.as_deref(),
            Some(running.id.as_str())
        );
    }

    #[test]
    fn stale_running_tasks_are_marked_interrupted_on_reconciliation() {
        let root = temp_root();
        let harness = Harness::open(&root).expect("open harness");

        let stale_runtime = TaskRuntimeInstance::current();
        let task = harness
            .start_task(NewRunningTask {
                title: "Background tool".into(),
                instructions: "Continue running".into(),
                runtime: stale_runtime.clone(),
                requested_by: Some("harness-tool-runtime".into()),
                tags: vec!["tool-call".into()],
                metadata: None,
                origin_call_id: Some("call_stale".into()),
            })
            .expect("start task");

        let current_runtime = TaskRuntimeInstance::current();
        let interrupted = harness
            .interrupt_stale_running_tasks(&current_runtime.instance_id)
            .expect("interrupt stale tasks");
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].status, TaskStatus::Interrupted);

        let detail = harness.task_detail(&task.id, 10).expect("task detail");
        assert_eq!(detail.task.status, TaskStatus::Interrupted);
        assert!(
            detail
                .events
                .iter()
                .any(|event| matches!(event.kind, TaskEventKind::Interrupted { .. }))
        );

        let notifications = harness
            .list_notifications(Some(NotificationStatus::Queued), 10)
            .expect("list notifications");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].kind, NotificationKind::TaskInterrupted);
        assert_eq!(notifications[0].task_id.as_deref(), Some(task.id.as_str()));
    }

    #[test]
    fn memory_round_trip_and_listing() {
        let root = temp_root();
        let harness = Harness::open(&root).expect("open harness");

        harness
            .write_memory(MemoryWrite {
                scope: "project".into(),
                key: "summary".into(),
                value: json!({ "text": "Keep the response terse." }),
                summary: Some("Main project summary".into()),
                metadata: None,
            })
            .expect("write memory");

        let record = harness
            .read_memory("project", "summary")
            .expect("read memory");
        assert_eq!(record.summary.as_deref(), Some("Main project summary"));

        let records = harness
            .list_memory(Some("project"), 10)
            .expect("list memory");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, "summary");
    }

    #[test]
    fn notification_status_transitions_are_persisted() {
        let root = temp_root();
        let harness = Harness::open(&root).expect("open harness");

        let notification = harness
            .enqueue_notification(NewNotification {
                kind: NotificationKind::Generic,
                task_id: None,
                title: "Ping".into(),
                body: "Hello".into(),
                payload: None,
            })
            .expect("enqueue");

        let delivered = harness
            .mark_notification_delivered(&notification.id)
            .expect("deliver");
        assert_eq!(delivered.status, NotificationStatus::Delivered);

        let acknowledged = harness
            .acknowledge_notification(&notification.id)
            .expect("ack");
        assert_eq!(acknowledged.status, NotificationStatus::Acknowledged);
        assert!(acknowledged.acknowledged_at_ms.is_some());
    }

    #[test]
    fn delivered_notification_can_be_requeued() {
        let root = temp_root();
        let harness = Harness::open(&root).expect("open harness");

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
        let requeued = harness
            .requeue_notification(&notification.id)
            .expect("requeue");
        assert_eq!(requeued.status, NotificationStatus::Queued);
        assert!(requeued.delivered_at_ms.is_none());
    }
}
