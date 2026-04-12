use std::hint::black_box;
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use gemini_live_harness::{
    Harness, HarnessController, NewNotification, NotificationKind, NotificationStatus,
    format_passive_notification_prompt,
};

fn temp_harness() -> Harness {
    let path = std::env::temp_dir().join(format!(
        "gemini-live-harness-bench-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time before unix epoch")
            .as_nanos()
    ));
    Harness::open(path).expect("open bench harness")
}

fn bench_passive_notification(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark runtime");
    let mut group = c.benchmark_group("passive_notification");

    group.bench_function("format_prompt", |b| {
        let harness = temp_harness();
        let notification = harness
            .enqueue_notification(NewNotification {
                kind: NotificationKind::TaskSucceeded,
                task_id: Some("task-bench".into()),
                title: "Task completed".into(),
                body: "The background work finished.".into(),
                payload: Some(serde_json::json!({ "ok": true })),
            })
            .expect("enqueue notification");
        b.iter(|| black_box(format_passive_notification_prompt(black_box(&notification))));
    });

    group.bench_function("dispatch_once_fresh_harness", |b| {
        b.iter_batched(
            || {
                let harness = temp_harness();
                let controller = HarnessController::new(harness.clone()).expect("controller");
                harness
                    .enqueue_notification(NewNotification {
                        kind: NotificationKind::TaskSucceeded,
                        task_id: Some("task-bench".into()),
                        title: "Task completed".into(),
                        body: "The background work finished.".into(),
                        payload: Some(serde_json::json!({ "ok": true })),
                    })
                    .expect("enqueue notification");
                controller
            },
            |controller| {
                runtime.block_on(async move {
                    controller
                        .dispatch_passive_notification_once(|delivery| async move {
                            black_box(delivery.prompt.len());
                            Ok::<(), String>(())
                        })
                        .await
                        .expect("dispatch passive notification");
                    assert!(
                        controller
                            .harness()
                            .list_notifications(Some(NotificationStatus::Delivered), 1)
                            .expect("list delivered")
                            .len()
                            == 1
                    );
                });
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("queue_signal_wakeup", |b| {
        b.iter_batched(
            || {
                let harness = temp_harness();
                let controller = HarnessController::new(harness.clone()).expect("controller");
                let version = controller.passive_notification_queue_version();
                (harness, controller, version)
            },
            |(harness, controller, version)| {
                runtime.block_on(async move {
                    let wait_task = tokio::spawn({
                        let controller = controller.clone();
                        async move {
                            controller
                                .wait_for_passive_notification_signal(version)
                                .await;
                        }
                    });
                    harness
                        .enqueue_notification(NewNotification {
                            kind: NotificationKind::Generic,
                            task_id: None,
                            title: "Ping".into(),
                            body: "Hello".into(),
                            payload: None,
                        })
                        .expect("enqueue notification");
                    wait_task.await.expect("wait task");
                });
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_passive_notification);
criterion_main!(benches);
