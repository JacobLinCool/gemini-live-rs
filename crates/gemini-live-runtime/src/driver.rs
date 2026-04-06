//! Testable session-driver abstraction above `gemini_live::Session`.

use base64::Engine;
use futures_util::future::BoxFuture;
use gemini_live::session::{Session, SessionObservation};
use gemini_live::types::{
    Blob, ClientMessage, FunctionResponse, RealtimeInput, ServerEvent, ToolResponseMessage,
};
use gemini_live::{SessionConfig, SessionError, SessionStatus};

/// Observable items yielded by a runtime session receive cursor.
#[derive(Debug, Clone)]
pub enum RuntimeSessionObservation {
    Event(ServerEvent),
    Lagged { count: u64 },
}

/// A cloneable session handle understood by the shared runtime.
pub trait RuntimeSession: Clone + Send + Sync + 'static {
    fn status(&self) -> SessionStatus;
    fn send_raw<'a>(&'a self, message: ClientMessage) -> BoxFuture<'a, Result<(), SessionError>>;
    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Option<ServerEvent>>;
    fn next_observed_event<'a>(&'a mut self) -> BoxFuture<'a, Option<RuntimeSessionObservation>> {
        Box::pin(async move {
            self.next_event()
                .await
                .map(RuntimeSessionObservation::Event)
        })
    }
    fn close(self) -> BoxFuture<'static, Result<(), SessionError>>
    where
        Self: Sized;

    fn send_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
                text: Some(text.into()),
                ..Default::default()
            }))
            .await
        })
    }

    fn send_audio_at_rate<'a>(
        &'a self,
        pcm_i16_le: &'a [u8],
        sample_rate: u32,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            let data = base64::engine::general_purpose::STANDARD.encode(pcm_i16_le);
            let mime_type = format!("audio/pcm;rate={sample_rate}");
            self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
                audio: Some(Blob { data, mime_type }),
                ..Default::default()
            }))
            .await
        })
    }

    fn send_video<'a>(
        &'a self,
        data: &'a [u8],
        mime: &'a str,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            let data = base64::engine::general_purpose::STANDARD.encode(data);
            self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
                video: Some(Blob {
                    data,
                    mime_type: mime.into(),
                }),
                ..Default::default()
            }))
            .await
        })
    }

    fn audio_stream_end<'a>(&'a self) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
                audio_stream_end: Some(true),
                ..Default::default()
            }))
            .await
        })
    }

    fn send_tool_response<'a>(
        &'a self,
        responses: Vec<FunctionResponse>,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.send_raw(ClientMessage::ToolResponse(ToolResponseMessage {
                function_responses: responses,
            }))
            .await
        })
    }
}

/// Session-construction boundary used to keep the runtime testable.
pub trait SessionDriver: Clone + Send + Sync + 'static {
    type Session: RuntimeSession;

    fn connect<'a>(
        &'a self,
        config: SessionConfig,
    ) -> BoxFuture<'a, Result<Self::Session, SessionError>>;
}

/// Default driver backed by `gemini_live::Session`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiSessionDriver;

/// Default runtime-session adapter backed by `gemini_live::Session`.
#[derive(Clone)]
pub struct GeminiSessionHandle {
    inner: Session,
}

impl GeminiSessionHandle {
    pub fn new(inner: Session) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &Session {
        &self.inner
    }

    pub fn into_inner(self) -> Session {
        self.inner
    }
}

impl SessionDriver for GeminiSessionDriver {
    type Session = GeminiSessionHandle;

    fn connect<'a>(
        &'a self,
        config: SessionConfig,
    ) -> BoxFuture<'a, Result<Self::Session, SessionError>> {
        Box::pin(async move { Ok(GeminiSessionHandle::new(Session::connect(config).await?)) })
    }
}

impl RuntimeSession for GeminiSessionHandle {
    fn status(&self) -> SessionStatus {
        self.inner.status()
    }

    fn send_raw<'a>(&'a self, message: ClientMessage) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.inner.send_raw(message).await })
    }

    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Option<ServerEvent>> {
        Box::pin(async move { self.inner.next_event().await })
    }

    fn next_observed_event<'a>(&'a mut self) -> BoxFuture<'a, Option<RuntimeSessionObservation>> {
        Box::pin(async move {
            self.inner
                .next_observed_event()
                .await
                .map(|observation| match observation {
                    SessionObservation::Event(event) => RuntimeSessionObservation::Event(event),
                    SessionObservation::Lagged { count } => {
                        RuntimeSessionObservation::Lagged { count }
                    }
                })
        })
    }

    fn close(self) -> BoxFuture<'static, Result<(), SessionError>>
    where
        Self: Sized,
    {
        Box::pin(async move { self.inner.close().await })
    }

    fn send_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.inner.send_text(text).await })
    }

    fn send_audio_at_rate<'a>(
        &'a self,
        pcm_i16_le: &'a [u8],
        sample_rate: u32,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.inner.send_audio_at_rate(pcm_i16_le, sample_rate).await })
    }

    fn send_video<'a>(
        &'a self,
        data: &'a [u8],
        mime: &'a str,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.inner.send_video(data, mime).await })
    }

    fn audio_stream_end<'a>(&'a self) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.inner.audio_stream_end().await })
    }

    fn send_tool_response<'a>(
        &'a self,
        responses: Vec<FunctionResponse>,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.inner.send_tool_response(responses).await })
    }
}
