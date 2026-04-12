use std::collections::VecDeque;
use std::hint::black_box;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use futures_util::future::BoxFuture;
use gemini_live::transport::TransportConfig;
use gemini_live::types::{
    ClientMessage, FunctionCallRequest, GoogleSearchTool, ServerEvent, SetupConfig, Tool,
};
use gemini_live::{ReconnectPolicy, SessionConfig, SessionError, SessionStatus};
use gemini_live_runtime::driver::{RuntimeSession, RuntimeSessionObservation, SessionDriver};
use gemini_live_runtime::{
    ManagedRuntime, RuntimeConfig, RuntimeEvent, RuntimeEventReceiver, RuntimeLifecycleEvent,
};

#[derive(Clone, Default)]
struct FakeDriver {
    sessions: Arc<Mutex<VecDeque<FakeSession>>>,
}

#[derive(Clone, Default)]
struct FakeSession {
    observations: Arc<Mutex<VecDeque<RuntimeSessionObservation>>>,
}

impl SessionDriver for FakeDriver {
    type Session = FakeSession;

    fn connect<'a>(
        &'a self,
        _config: SessionConfig,
    ) -> BoxFuture<'a, Result<Self::Session, SessionError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            Ok(sessions
                .lock()
                .expect("sessions lock")
                .pop_front()
                .expect("fake session"))
        })
    }
}

impl RuntimeSession for FakeSession {
    fn status(&self) -> SessionStatus {
        SessionStatus::Connected
    }

    fn send_raw<'a>(&'a self, _message: ClientMessage) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async { Ok(()) })
    }

    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Option<ServerEvent>> {
        Box::pin(async { None })
    }

    fn next_observed_event<'a>(&'a mut self) -> BoxFuture<'a, Option<RuntimeSessionObservation>> {
        let observations = Arc::clone(&self.observations);
        Box::pin(async move { observations.lock().expect("observations lock").pop_front() })
    }

    fn close(self) -> BoxFuture<'static, Result<(), SessionError>>
    where
        Self: Sized,
    {
        Box::pin(async { Ok(()) })
    }
}

fn runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        session: SessionConfig {
            transport: TransportConfig::default(),
            setup: SetupConfig {
                model: "models/test".into(),
                tools: Some(vec![Tool::GoogleSearch(GoogleSearchTool {})]),
                ..Default::default()
            },
            reconnect: ReconnectPolicy::default(),
        },
    }
}

fn managed_runtime_with_events(
    events: Vec<RuntimeSessionObservation>,
) -> (ManagedRuntime<FakeDriver>, RuntimeEventReceiver) {
    let driver = FakeDriver {
        sessions: Arc::new(Mutex::new(VecDeque::from([FakeSession {
            observations: Arc::new(Mutex::new(VecDeque::from(events))),
        }]))),
    };
    ManagedRuntime::new(runtime_config(), driver)
}

fn connect_and_drain_next(
    runtime: &tokio::runtime::Runtime,
    mut managed: ManagedRuntime<FakeDriver>,
    mut rx: RuntimeEventReceiver,
) -> RuntimeEvent {
    runtime.block_on(async move {
        managed.connect().await.expect("connect runtime");
        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Connecting))
        ));
        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Lifecycle(RuntimeLifecycleEvent::Connected))
        ));
        rx.recv().await.expect("forwarded runtime event")
    })
}

fn bench_managed_runtime(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark runtime");
    let mut group = c.benchmark_group("managed_runtime");

    group.bench_function("connect_forward_model_text", |b| {
        b.iter_batched(
            || {
                managed_runtime_with_events(vec![RuntimeSessionObservation::Event(
                    ServerEvent::ModelText("hello from bench".into()),
                )])
            },
            |(managed, rx)| {
                black_box(connect_and_drain_next(&runtime, managed, rx));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("connect_forward_model_audio_40ms", |b| {
        let audio = Bytes::from(vec![7_u8; 24_000 * 2 * 40 / 1_000]);
        b.iter_batched(
            || {
                managed_runtime_with_events(vec![RuntimeSessionObservation::Event(
                    ServerEvent::ModelAudio(audio.clone()),
                )])
            },
            |(managed, rx)| {
                black_box(connect_and_drain_next(&runtime, managed, rx));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("connect_forward_tool_call", |b| {
        b.iter_batched(
            || {
                managed_runtime_with_events(vec![RuntimeSessionObservation::Event(
                    ServerEvent::ToolCall(vec![FunctionCallRequest {
                        id: "call-1".into(),
                        name: "bench_tool".into(),
                        args: serde_json::json!({ "ok": true }),
                    }]),
                )])
            },
            |(managed, rx)| {
                black_box(connect_and_drain_next(&runtime, managed, rx));
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_managed_runtime);
criterion_main!(benches);
