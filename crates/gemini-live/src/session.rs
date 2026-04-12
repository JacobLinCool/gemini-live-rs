//! Session layer — the primary interface for the Gemini Live API.
//!
//! [`Session`] manages the full connection lifecycle: WebSocket connect,
//! `setup` handshake, automatic reconnection, and typed send/receive.
//! Because Live API WebSocket sessions are finite-lived, this layer also owns
//! `goAway` handling and session-resumption handoff.
//!
//! # Architecture
//!
//! ```text
//!                        ┌──────────────┐
//!                        │   Session    │  ← cheap Clone (Arc)
//!                        │  (handle)    │
//!                        └──┬───────┬───┘
//!                 cmd_tx    │       │  event_rx (broadcast)
//!                           ▼       ▼
//!             ┌─────────────────────────────────┐
//!             │          Runner task            │  ← tokio::spawn
//!             │  ┌───────────┐  ┌────────────┐  │
//!             │  │ Send Loop │  │ Recv Loop  │  │
//!             │  │ (ws sink) │  │ (ws stream)│  │
//!             │  └───────────┘  └────────────┘  │
//!             │  reconnect · GoAway · resume    │
//!             └─────────────────────────────────┘
//! ```
//!
//! The runner is a single `tokio::spawn`'d task that uses `tokio::select!`
//! to multiplex user commands and WebSocket frames.  Reconnection is
//! transparent — messages buffer in the mpsc channel during downtime.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fmt::Write as _, sync::Arc as StdArc};

use base64::Engine;
use bytes::Bytes;
use futures_util::Stream;
use tokio::sync::{broadcast, mpsc};

use crate::audio::{AudioEncoder, INPUT_AUDIO_MIME, INPUT_SAMPLE_RATE};
use crate::codec;
use crate::error::SessionError;
use crate::transport::{Connection, RawFrame, TransportConfig};
use crate::types::*;

/// Timeout for the `setup` → `setupComplete` handshake.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_CHANNEL_CAPACITY: usize = 256;
const COMMAND_CHANNEL_CAPACITY: usize = 64;

// ── Public config types ──────────────────────────────────────────────────────

/// Complete session configuration combining transport, protocol, and
/// reconnection settings.
///
/// `setup` is sent on the initial connect and again on every reconnect. When a
/// fresh resume handle exists, the session layer injects it into
/// `setup.session_resumption.handle` before sending.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub transport: TransportConfig,
    pub setup: SetupConfig,
    pub reconnect: ReconnectPolicy,
}

/// Reconnection behaviour after an unexpected disconnect or `goAway`.
///
/// Backoff formula: `base_backoff × 2^(attempt − 1)`, capped at `max_backoff`.
///
/// Outbound messages continue to queue in the command channel while reconnect
/// attempts are in progress.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Enable automatic reconnection.  Default: `true`.
    pub enabled: bool,
    /// Initial backoff delay.  Default: 500 ms.
    pub base_backoff: Duration,
    /// Maximum backoff delay.  Default: 5 s.
    pub max_backoff: Duration,
    /// Maximum reconnection attempts.  `None` = unlimited.  Default: `Some(10)`.
    pub max_attempts: Option<u32>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(5),
            max_attempts: Some(10),
        }
    }
}

/// Observable session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Connecting = 0,
    Connected = 1,
    Reconnecting = 2,
    Closed = 3,
}

/// Observable items from the session receive cursor.
///
/// Most callers only care about [`ServerEvent`] values and can continue using
/// [`Session::next_event`]. Callers that need visibility into dropped events
/// should use [`Session::next_observed_event`].
#[derive(Debug, Clone)]
pub enum SessionObservation {
    Event(ServerEvent),
    Lagged { count: u64 },
}

// ── Session handle ───────────────────────────────────────────────────────────

/// An active session with the Gemini Live API.
///
/// Cheaply [`Clone`]able — all clones share the same underlying connection
/// and runner task.  Each clone has its own event cursor, so events are
/// never "stolen" between consumers.
///
/// Created via [`Session::connect`].
pub struct Session {
    cmd_tx: mpsc::Sender<Command>,
    event_tx: broadcast::Sender<ServerEvent>,
    event_rx: broadcast::Receiver<ServerEvent>,
    state: Arc<SharedState>,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            cmd_tx: self.cmd_tx.clone(),
            event_tx: self.event_tx.clone(),
            event_rx: self.event_tx.subscribe(),
            state: self.state.clone(),
        }
    }
}

impl Session {
    /// Connect to the Gemini Live API and complete the `setup` handshake.
    ///
    /// On success the session is immediately usable — `setupComplete` has
    /// already been received.  A background runner task is spawned to
    /// manage the connection and handle reconnection.
    pub async fn connect(config: SessionConfig) -> Result<Self, SessionError> {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let state = Arc::new(SharedState::new());

        // 1. Establish WebSocket connection
        state.set_status(SessionStatus::Connecting);
        let mut conn = Connection::connect(&config.transport)
            .await
            .map_err(|e| SessionError::SetupFailed(format_error_chain(&e)))?;

        // 2. Send setup and await setupComplete
        do_handshake(&mut conn, &config.setup, None).await?;
        state.set_status(SessionStatus::Connected);
        tracing::info!("session established");

        // 3. Spawn the background runner
        let runner = Runner {
            cmd_rx,
            event_tx: event_tx.clone(),
            conn,
            config,
            state: Arc::clone(&state),
            audio_encoder: AudioEncoder::new(),
            audio_mime_buf: String::with_capacity(INPUT_AUDIO_MIME.len() + 16),
            video_b64_buf: String::with_capacity(128 * 1024),
            send_json_buf: Vec::with_capacity(16 * 1024),
        };
        tokio::spawn(runner.run());

        let event_rx = event_tx.subscribe();
        Ok(Self {
            cmd_tx,
            event_tx,
            event_rx,
            state,
        })
    }

    /// Current session status.
    pub fn status(&self) -> SessionStatus {
        self.state.status()
    }

    // ── Send convenience methods ─────────────────────────────────────

    /// Stream audio.  Accepts raw i16 little-endian PCM bytes — base64
    /// encoding and `realtimeInput` wrapping are handled internally.
    ///
    /// Uses the session runner's reusable base64 and JSON buffers, so the hot
    /// streaming path no longer allocates a fresh base64 `String` per chunk.
    ///
    /// Each call still copies the raw PCM bytes into the runner command queue
    /// so the background task can own them safely.
    pub async fn send_audio(&self, pcm_i16_le: &[u8]) -> Result<(), SessionError> {
        self.send_audio_at_rate(pcm_i16_le, INPUT_SAMPLE_RATE).await
    }

    /// Stream audio at a specific sample rate.
    ///
    /// Like [`send_audio`](Self::send_audio) but lets you specify the sample
    /// rate (e.g. for mic capture at the device's native rate).  The server
    /// resamples as needed.
    ///
    /// Reuses the runner's base64, MIME, and JSON buffers; only the raw PCM
    /// command payload itself is copied into the queue.
    pub async fn send_audio_at_rate(
        &self,
        pcm_i16_le: &[u8],
        sample_rate: u32,
    ) -> Result<(), SessionError> {
        self.send_command(Command::SendAudio {
            pcm: Bytes::copy_from_slice(pcm_i16_le),
            sample_rate,
        })
        .await
    }

    /// Stream a video frame.  Accepts encoded JPEG/PNG bytes.
    ///
    /// The runner reuses its base64 and JSON buffers. The frame bytes and MIME
    /// string are still copied into the command queue so the background task
    /// can own them safely.
    pub async fn send_video(&self, data: &[u8], mime: &str) -> Result<(), SessionError> {
        self.send_command(Command::SendVideo {
            data: Bytes::copy_from_slice(data),
            mime_type: VideoMimeType::from_input(mime),
        })
        .await
    }

    /// Send text via the real-time input channel.
    pub async fn send_text(&self, text: &str) -> Result<(), SessionError> {
        self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
            text: Some(text.into()),
            ..Default::default()
        }))
        .await
    }

    /// Send conversation history or incremental content.
    pub async fn send_client_content(&self, content: ClientContent) -> Result<(), SessionError> {
        self.send_raw(ClientMessage::ClientContent(content)).await
    }

    /// Manual VAD: signal that user activity (speech) has started.
    pub async fn activity_start(&self) -> Result<(), SessionError> {
        self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
            activity_start: Some(EmptyObject {}),
            ..Default::default()
        }))
        .await
    }

    /// Manual VAD: signal that user activity has ended.
    pub async fn activity_end(&self) -> Result<(), SessionError> {
        self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
            activity_end: Some(EmptyObject {}),
            ..Default::default()
        }))
        .await
    }

    /// Notify the server that the audio stream has ended (auto VAD mode).
    pub async fn audio_stream_end(&self) -> Result<(), SessionError> {
        self.send_raw(ClientMessage::RealtimeInput(RealtimeInput {
            audio_stream_end: Some(true),
            ..Default::default()
        }))
        .await
    }

    /// Reply to one or more server-initiated function calls.
    pub async fn send_tool_response(
        &self,
        responses: Vec<FunctionResponse>,
    ) -> Result<(), SessionError> {
        self.send_raw(ClientMessage::ToolResponse(ToolResponseMessage {
            function_responses: responses,
        }))
        .await
    }

    /// Send an arbitrary [`ClientMessage`] (escape hatch for future types).
    pub async fn send_raw(&self, msg: ClientMessage) -> Result<(), SessionError> {
        self.send_command(Command::Send(Box::new(msg))).await
    }

    // ── Receive ──────────────────────────────────────────────────────

    /// Create a new event [`Stream`].
    ///
    /// Each call produces an **independent** subscription — multiple streams
    /// can coexist without stealing events.
    pub fn events(&self) -> impl Stream<Item = ServerEvent> {
        let rx = self.event_tx.subscribe();
        futures_util::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(event) => return Some((event, rx)),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "event stream lagged, some events were missed");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
    }

    /// Wait for the next event on this handle's cursor.
    ///
    /// Returns `None` when the session is permanently closed.
    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        loop {
            match self.next_observed_event().await? {
                SessionObservation::Event(event) => return Some(event),
                SessionObservation::Lagged { count: n } => {
                    tracing::warn!(n, "event consumer lagged, some events were missed");
                }
            }
        }
    }

    /// Wait for the next observable item on this handle's cursor.
    ///
    /// Unlike [`Session::next_event`], this does not hide lagged broadcast
    /// notifications. Callers can therefore surface lost-event state directly
    /// in their own runtime or UI layer.
    pub async fn next_observed_event(&mut self) -> Option<SessionObservation> {
        match self.event_rx.recv().await {
            Ok(event) => Some(SessionObservation::Event(event)),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                Some(SessionObservation::Lagged { count: n })
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    // ── Lifecycle ────────────────────────────────────────────────────

    /// Gracefully close the session.
    ///
    /// Sends a WebSocket close frame and shuts down the background runner.
    /// Other clones of this session will observe `SessionStatus::Closed`.
    ///
    /// This only enqueues the close request; it does not await runner-task
    /// completion yet.
    pub async fn close(self) -> Result<(), SessionError> {
        let _ = self.cmd_tx.send(Command::Close).await;
        Ok(())
    }

    async fn send_command(&self, cmd: Command) -> Result<(), SessionError> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| SessionError::Closed)
    }
}

// ── Internals ──────────────────────────────��─────────────────────────────────

enum Command {
    Send(Box<ClientMessage>),
    SendAudio {
        pcm: Bytes,
        sample_rate: u32,
    },
    SendVideo {
        data: Bytes,
        mime_type: VideoMimeType,
    },
    Close,
}

enum VideoMimeType {
    Jpeg,
    Png,
    Other(StdArc<str>),
}

impl VideoMimeType {
    fn from_input(mime: &str) -> Self {
        match mime {
            "image/jpeg" => Self::Jpeg,
            "image/png" => Self::Png,
            _ => Self::Other(StdArc::<str>::from(mime)),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Other(mime) => mime,
        }
    }
}

// ── Shared state (survives reconnects) ─────────────────────────────���─────────

struct SharedState {
    resume_handle: Mutex<Option<String>>,
    status: AtomicU8,
}

impl SharedState {
    fn new() -> Self {
        Self {
            resume_handle: Mutex::new(None),
            status: AtomicU8::new(SessionStatus::Connecting as u8),
        }
    }

    fn status(&self) -> SessionStatus {
        match self.status.load(Ordering::Relaxed) {
            0 => SessionStatus::Connecting,
            1 => SessionStatus::Connected,
            2 => SessionStatus::Reconnecting,
            _ => SessionStatus::Closed,
        }
    }

    fn set_status(&self, s: SessionStatus) {
        self.status.store(s as u8, Ordering::Relaxed);
    }

    fn resume_handle(&self) -> Option<String> {
        self.resume_handle.lock().unwrap().clone()
    }

    fn set_resume_handle(&self, handle: Option<String>) {
        *self.resume_handle.lock().unwrap() = handle;
    }
}

// ── Runner task ──────────────────────────────────────────────────────────────

enum DisconnectReason {
    GoAway,
    ConnectionLost,
    UserClose,
    SendersDropped,
}

struct Runner {
    cmd_rx: mpsc::Receiver<Command>,
    event_tx: broadcast::Sender<ServerEvent>,
    conn: Connection,
    config: SessionConfig,
    state: Arc<SharedState>,
    audio_encoder: AudioEncoder,
    audio_mime_buf: String,
    video_b64_buf: String,
    send_json_buf: Vec<u8>,
}

impl Runner {
    async fn run(mut self) {
        loop {
            let reason = self.run_connected().await;

            match reason {
                DisconnectReason::UserClose | DisconnectReason::SendersDropped => {
                    self.state.set_status(SessionStatus::Closed);
                    tracing::info!("session closed");
                    break;
                }
                DisconnectReason::GoAway | DisconnectReason::ConnectionLost => {
                    if !self.config.reconnect.enabled {
                        self.state.set_status(SessionStatus::Closed);
                        let _ = self.event_tx.send(ServerEvent::Closed {
                            reason: "disconnected (reconnect disabled)".into(),
                        });
                        break;
                    }

                    self.state.set_status(SessionStatus::Reconnecting);
                    tracing::info!("attempting reconnection");

                    match self.reconnect().await {
                        Ok(conn) => {
                            self.conn = conn;
                            self.state.set_status(SessionStatus::Connected);
                            tracing::info!("reconnected successfully");
                        }
                        Err(e) => {
                            self.state.set_status(SessionStatus::Closed);
                            let _ = self.event_tx.send(ServerEvent::Error(ApiError {
                                message: e.to_string(),
                            }));
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Drive the connection: forward commands to the WebSocket, broadcast
    /// received frames as events.  Returns the reason for disconnection.
    async fn run_connected(&mut self) -> DisconnectReason {
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(Command::Send(msg)) => { let msg = *msg;
                            match codec::encode_into(&mut self.send_json_buf, &msg) {
                                Ok(()) => {
                                    if let Err(e) = self.conn.send_json_bytes(&self.send_json_buf).await {
                                        tracing::warn!(error = %e, "send failed");
                                        return DisconnectReason::ConnectionLost;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "message encode failed, dropping");
                                }
                            }
                        }
                        Some(Command::SendAudio { pcm, sample_rate }) => {
                            let (audio_encoder, audio_mime_buf, send_json_buf) = (
                                &mut self.audio_encoder,
                                &mut self.audio_mime_buf,
                                &mut self.send_json_buf,
                            );
                            let b64 = audio_encoder.encode_i16_le(&pcm);
                            let mime = audio_mime_for_rate(sample_rate, audio_mime_buf);
                            match codec::encode_realtime_input_blob_into(
                                send_json_buf,
                                codec::RealtimeInputBlobKind::Audio,
                                mime,
                                b64,
                            ) {
                                Ok(()) => {
                                    if let Err(e) = self.conn.send_json_bytes(&self.send_json_buf).await {
                                        tracing::warn!(error = %e, "send failed");
                                        return DisconnectReason::ConnectionLost;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "audio command encode failed, dropping");
                                }
                            }
                        }
                        Some(Command::SendVideo { data, mime_type }) => {
                            let (video_b64_buf, send_json_buf) =
                                (&mut self.video_b64_buf, &mut self.send_json_buf);
                            video_b64_buf.clear();
                            base64::engine::general_purpose::STANDARD
                                .encode_string(&data, video_b64_buf);
                            match codec::encode_realtime_input_blob_into(
                                send_json_buf,
                                codec::RealtimeInputBlobKind::Video,
                                mime_type.as_str(),
                                video_b64_buf,
                            ) {
                                Ok(()) => {
                                    if let Err(e) = self.conn.send_json_bytes(&self.send_json_buf).await {
                                        tracing::warn!(error = %e, "send failed");
                                        return DisconnectReason::ConnectionLost;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "video command encode failed, dropping");
                                }
                            }
                        }
                        Some(Command::Close) => {
                            let _ = self.conn.send_close().await;
                            return DisconnectReason::UserClose;
                        }
                        None => {
                            let _ = self.conn.send_close().await;
                            return DisconnectReason::SendersDropped;
                        }
                    }
                }
                frame = self.conn.recv() => {
                    match frame {
                        Ok(RawFrame::Text(text)) => {
                            if let Some(reason) = self.try_decode_and_process(&text) {
                                return reason;
                            }
                        }
                        Ok(RawFrame::Binary(data)) => {
                            // Gemini Live API may send JSON as binary frames.
                            if let Ok(text) = std::str::from_utf8(&data)
                                && let Some(reason) = self.try_decode_and_process(text)
                            {
                                return reason;
                            }
                        }
                        Ok(RawFrame::Close(reason)) => {
                            let _ = self.event_tx.send(ServerEvent::Closed {
                                reason: reason.unwrap_or_default(),
                            });
                            return DisconnectReason::ConnectionLost;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "recv error");
                            return DisconnectReason::ConnectionLost;
                        }
                    }
                }
            }
        }
    }

    /// Decode a server message, track session state, and broadcast events.
    /// Returns `true` if the message contained a `goAway`.
    /// Try to decode a JSON string and process it. Returns `Some(reason)` if
    /// the connection loop should exit.
    fn try_decode_and_process(&self, text: &str) -> Option<DisconnectReason> {
        match codec::decode(text) {
            Ok(msg) => {
                if self.process_message(msg) {
                    Some(DisconnectReason::GoAway)
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to decode server message");
                None
            }
        }
    }

    fn process_message(&self, msg: ServerMessage) -> bool {
        // Track the latest resume handle for reconnection.
        if let Some(ref sr) = msg.session_resumption_update
            && let Some(ref handle) = sr.new_handle
        {
            self.state.set_resume_handle(Some(handle.clone()));
        }

        let is_go_away = msg.go_away.is_some();

        for event in codec::into_events(msg) {
            let _ = self.event_tx.send(event);
        }

        is_go_away
    }

    /// Attempt reconnection with exponential backoff.
    async fn reconnect(&mut self) -> Result<Connection, SessionError> {
        let policy = &self.config.reconnect;
        let mut attempt = 0u32;

        loop {
            attempt += 1;
            if policy.max_attempts.is_some_and(|max| attempt > max) {
                return Err(SessionError::ReconnectExhausted {
                    attempts: attempt - 1,
                });
            }

            let backoff = compute_backoff(policy, attempt);
            tracing::debug!(attempt, ?backoff, "reconnect backoff");
            tokio::time::sleep(backoff).await;

            let mut conn = match Connection::connect(&self.config.transport).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "reconnect connect failed");
                    continue;
                }
            };

            let resume_handle = self.state.resume_handle();
            match do_handshake(&mut conn, &self.config.setup, resume_handle).await {
                Ok(()) => return Ok(conn),
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "reconnect handshake failed");
                    continue;
                }
            }
        }
    }
}

fn audio_mime_for_rate(sample_rate: u32, mime_buf: &mut String) -> &str {
    if sample_rate == INPUT_SAMPLE_RATE {
        INPUT_AUDIO_MIME
    } else {
        mime_buf.clear();
        write!(mime_buf, "audio/pcm;rate={sample_rate}").expect("write! into String cannot fail");
        mime_buf.as_str()
    }
}

// ── Handshake ────────────────────────────────────────────────────────────────

/// Send `setup` and wait for `setupComplete`.
///
/// If `resume_handle` is `Some`, it is injected into the setup's
/// `sessionResumption` config so the server can resume state.
async fn do_handshake(
    conn: &mut Connection,
    setup: &SetupConfig,
    resume_handle: Option<String>,
) -> Result<(), SessionError> {
    let setup = setup_for_handshake(setup, resume_handle);
    let mut json = Vec::with_capacity(4096);
    codec::encode_into(&mut json, &ClientMessage::Setup(setup))?;
    tracing::debug!(setup_json = %String::from_utf8_lossy(&json), "sending setup message");
    conn.send_json_bytes(&json)
        .await
        .map_err(|e| SessionError::SetupFailed(format_error_chain(&e)))?;

    tokio::time::timeout(SETUP_TIMEOUT, wait_setup_complete(conn))
        .await
        .map_err(|_| SessionError::SetupTimeout(SETUP_TIMEOUT))?
}

fn setup_for_handshake(setup: &SetupConfig, resume_handle: Option<String>) -> SetupConfig {
    let mut setup = setup.clone();
    if let Some(handle) = resume_handle {
        let sr = setup
            .session_resumption
            .get_or_insert_with(SessionResumptionConfig::default);
        sr.handle = Some(handle);
        // Initial-history mode is only valid on a fresh session before the
        // first realtime input. Resumed sessions continue existing history and
        // must not wait for new bootstrap `clientContent`.
        setup.history_config = None;
    }
    setup
}

async fn wait_setup_complete(conn: &mut Connection) -> Result<(), SessionError> {
    loop {
        match conn.recv().await {
            Ok(RawFrame::Text(text)) => {
                tracing::debug!(raw = %text, "received text during setup");
                match try_parse_setup_response(&text)? {
                    SetupResult::Complete => return Ok(()),
                    SetupResult::Continue => {}
                }
            }
            Ok(RawFrame::Binary(data)) => {
                // Gemini Live API may send JSON as binary frames.
                if let Ok(text) = std::str::from_utf8(&data) {
                    tracing::debug!(raw = %text, "received binary (UTF-8) during setup");
                    match try_parse_setup_response(text)? {
                        SetupResult::Complete => return Ok(()),
                        SetupResult::Continue => {}
                    }
                }
            }
            Ok(RawFrame::Close(reason)) => {
                return Err(SessionError::SetupFailed(format!(
                    "closed during setup: {}",
                    reason.unwrap_or_default()
                )));
            }
            Err(e) => return Err(SessionError::SetupFailed(format_error_chain(&e))),
        }
    }
}

enum SetupResult {
    Complete,
    Continue,
}

fn try_parse_setup_response(text: &str) -> Result<SetupResult, SessionError> {
    let msg = codec::decode(text).map_err(|e| SessionError::SetupFailed(format_error_chain(&e)))?;
    if msg.setup_complete.is_some() {
        return Ok(SetupResult::Complete);
    }
    if let Some(err) = msg.error {
        return Err(SessionError::Api(err.message));
    }
    Ok(SetupResult::Continue)
}

fn format_error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut current = error.source();
    while let Some(source) = current {
        let source_text = source.to_string();
        if !source_text.is_empty() && !message.ends_with(&source_text) {
            message.push_str(": ");
            message.push_str(&source_text);
        }
        current = source.source();
    }
    message
}

// ── Backoff ──────────────────────────────────────────────────────────────────

/// Exponential backoff: `base × 2^(attempt − 1)`, capped at `max`.
fn compute_backoff(policy: &ReconnectPolicy, attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(10);
    let factor = 2u64.saturating_pow(exp);
    let ms = policy.base_backoff.as_millis() as u64 * factor;
    Duration::from_millis(ms.min(policy.max_backoff.as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use crate::error::{BearerTokenError, ConnectError};
    use crate::types::HistoryConfig;

    use super::*;

    #[test]
    fn backoff_exponential_with_cap() {
        let policy = ReconnectPolicy {
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(5),
            ..Default::default()
        };
        assert_eq!(compute_backoff(&policy, 1), Duration::from_millis(500));
        assert_eq!(compute_backoff(&policy, 2), Duration::from_millis(1000));
        assert_eq!(compute_backoff(&policy, 3), Duration::from_millis(2000));
        assert_eq!(compute_backoff(&policy, 4), Duration::from_millis(4000));
        assert_eq!(compute_backoff(&policy, 5), Duration::from_secs(5)); // capped
        assert_eq!(compute_backoff(&policy, 100), Duration::from_secs(5));
    }

    #[test]
    fn status_round_trip() {
        let state = SharedState::new();
        assert_eq!(state.status(), SessionStatus::Connecting);

        state.set_status(SessionStatus::Connected);
        assert_eq!(state.status(), SessionStatus::Connected);

        state.set_status(SessionStatus::Reconnecting);
        assert_eq!(state.status(), SessionStatus::Reconnecting);

        state.set_status(SessionStatus::Closed);
        assert_eq!(state.status(), SessionStatus::Closed);
    }

    #[test]
    fn resume_handle_tracking() {
        let state = SharedState::new();
        assert!(state.resume_handle().is_none());

        state.set_resume_handle(Some("h1".into()));
        assert_eq!(state.resume_handle().as_deref(), Some("h1"));

        state.set_resume_handle(Some("h2".into()));
        assert_eq!(state.resume_handle().as_deref(), Some("h2"));

        state.set_resume_handle(None);
        assert!(state.resume_handle().is_none());
    }

    #[test]
    fn default_reconnect_policy() {
        let p = ReconnectPolicy::default();
        assert!(p.enabled);
        assert_eq!(p.base_backoff, Duration::from_millis(500));
        assert_eq!(p.max_backoff, Duration::from_secs(5));
        assert_eq!(p.max_attempts, Some(10));
    }

    #[test]
    fn handshake_setup_strips_initial_history_when_resuming() {
        let setup = SetupConfig {
            model: "models/test".into(),
            history_config: Some(HistoryConfig {
                initial_history_in_client_content: Some(true),
            }),
            ..Default::default()
        };

        let resumed = setup_for_handshake(&setup, Some("resume-1".into()));
        assert_eq!(
            resumed
                .session_resumption
                .as_ref()
                .and_then(|config| config.handle.as_deref()),
            Some("resume-1")
        );
        assert!(resumed.history_config.is_none());

        let fresh = setup_for_handshake(&setup, None);
        assert_eq!(fresh.history_config, setup.history_config);
    }

    #[test]
    fn format_error_chain_includes_sources() {
        let err = ConnectError::Auth(BearerTokenError::with_source(
            "failed to refresh Google Cloud access token from Application Default Credentials",
            std::io::Error::other("invalid_grant: Account has been deleted"),
        ));

        assert_eq!(
            format_error_chain(&err),
            "failed to obtain bearer token: failed to refresh Google Cloud access token from Application Default Credentials: invalid_grant: Account has been deleted"
        );
    }
}
