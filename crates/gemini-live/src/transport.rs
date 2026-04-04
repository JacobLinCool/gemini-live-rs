//! WebSocket transport layer.
//!
//! Handles raw connection establishment, frame I/O, and TLS (via `rustls`).
//! This is the lowest layer — it knows nothing about JSON or the Gemini
//! protocol.  The [`session`](crate::session) layer wraps [`Connection`] to
//! add protocol-level concerns.
//!
//! # Endpoints
//!
//! | Auth method     | Endpoint                                                                                                                   |
//! |-----------------|----------------------------------------------------------------------------------------------------------------------------|
//! | API key         | `wss://generativelanguage.googleapis.com/ws/…v1beta.GenerativeService.BidiGenerateContent?key={KEY}`                        |
//! | Ephemeral token | `wss://generativelanguage.googleapis.com/ws/…v1alpha.GenerativeService.BidiGenerateContentConstrained?access_token={TOKEN}` |
//!
//! Both can be overridden via [`TransportConfig::endpoint_override`] for
//! testing or Vertex AI endpoints.

use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use crate::error::{ConnectError, RecvError, SendError};

const DEFAULT_HOST: &str = "wss://generativelanguage.googleapis.com";
const API_KEY_PATH: &str =
    "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const EPHEMERAL_TOKEN_PATH: &str =
    "/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContentConstrained";

// ── Auth ─────────────────────────────────────────────────────────────────────

/// Authentication method for the Gemini Live API.
#[derive(Debug, Clone)]
pub enum Auth {
    /// Standard long-lived API key (sent as `?key=` query param).
    ApiKey(String),
    /// Short-lived token obtained via the ephemeral token endpoint (v1alpha).
    EphemeralToken(String),
}

// ── TransportConfig ──────────────────────────────────────────────────────────

/// Transport layer settings.
///
/// All fields have sensible defaults (see [`Default`] impl).  In most cases
/// only [`auth`](Self::auth) needs to be set explicitly.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub auth: Auth,
    /// Override the default endpoint (for testing or Vertex AI).
    pub endpoint_override: Option<String>,
    /// WebSocket write buffer size in bytes.  Default: 64 KB.
    pub write_buffer_size: usize,
    /// Maximum WebSocket frame size in bytes.  Default: 16 MB.
    pub max_frame_size: usize,
    /// Connection timeout.  Default: 10 s.
    pub connect_timeout: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            auth: Auth::ApiKey(String::new()),
            endpoint_override: None,
            write_buffer_size: 64 * 1024,
            max_frame_size: 16 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
        }
    }
}

// ── RawFrame ─────────────────────────────────────────────────────────────────

/// A raw WebSocket frame received from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum RawFrame {
    /// UTF-8 text frame (JSON on the Gemini Live protocol).
    Text(String),
    /// Binary frame.
    Binary(Vec<u8>),
    /// Close frame with an optional reason string.
    Close(Option<String>),
}

// ── Connection ───────────────────────────────────────────────────────────────

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Low-level WebSocket connection handle.
///
/// Wraps a split `tokio-tungstenite` stream (sink + source) and provides
/// simple send/recv methods for raw frames.  This type is **not** meant to
/// be used directly by application code — the session layer manages it.
pub struct Connection {
    sink: SplitSink<WsStream, Message>,
    stream: SplitStream<WsStream>,
}

impl Connection {
    /// Establish a WebSocket connection (does **not** send the `setup` message).
    pub async fn connect(config: &TransportConfig) -> Result<Self, ConnectError> {
        // Ensure a rustls CryptoProvider is installed (idempotent).
        let _ = rustls::crypto::ring::default_provider().install_default();

        let url = build_url(config);
        let mut ws_config = WebSocketConfig::default();
        ws_config.write_buffer_size = config.write_buffer_size;
        ws_config.max_write_buffer_size = config.write_buffer_size * 2;
        ws_config.max_frame_size = Some(config.max_frame_size);
        ws_config.max_message_size = Some(config.max_frame_size);

        let connect_fut = connect_async_with_config(url, Some(ws_config), false);

        let (ws_stream, _response) = tokio::time::timeout(config.connect_timeout, connect_fut)
            .await
            .map_err(|_| ConnectError::Timeout(config.connect_timeout))?
            .map_err(classify_connect_error)?;

        let (sink, stream) = ws_stream.split();
        tracing::debug!("WebSocket connection established");
        Ok(Self { sink, stream })
    }

    /// Send a text frame (typically a serialised JSON message).
    pub async fn send_text(&mut self, json: &str) -> Result<(), SendError> {
        self.sink
            .send(Message::text(json))
            .await
            .map_err(classify_send_error)
    }

    /// Send a binary frame.
    pub async fn send_binary(&mut self, data: &[u8]) -> Result<(), SendError> {
        self.sink
            .send(Message::binary(data.to_vec()))
            .await
            .map_err(classify_send_error)
    }

    /// Receive the next meaningful frame, skipping ping/pong control frames.
    pub async fn recv(&mut self) -> Result<RawFrame, RecvError> {
        loop {
            match self.stream.next().await {
                Some(Ok(msg)) => {
                    tracing::trace!(msg_type = ?std::mem::discriminant(&msg), "raw ws frame received");
                    match msg {
                        Message::Text(text) => return Ok(RawFrame::Text(text.to_string())),
                        Message::Binary(data) => return Ok(RawFrame::Binary(data.to_vec())),
                        Message::Close(frame) => {
                            let reason = frame.map(|f| f.reason.to_string());
                            return Ok(RawFrame::Close(reason));
                        }
                        // Ping/Pong are handled at the tungstenite protocol level.
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                    }
                }
                Some(Err(e)) => return Err(RecvError::Ws(e)),
                None => return Err(RecvError::Closed),
            }
        }
    }

    /// Send a close frame without consuming the connection.
    pub(crate) async fn send_close(&mut self) -> Result<(), SendError> {
        self.sink
            .send(Message::Close(None))
            .await
            .map_err(classify_send_error)
    }

    /// Gracefully close the connection by sending a close frame.
    pub async fn close(mut self) -> Result<(), SendError> {
        self.send_close().await
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn build_url(config: &TransportConfig) -> String {
    if let Some(url) = &config.endpoint_override {
        return url.clone();
    }
    match &config.auth {
        Auth::ApiKey(key) => format!("{DEFAULT_HOST}{API_KEY_PATH}?key={key}"),
        Auth::EphemeralToken(token) => {
            format!("{DEFAULT_HOST}{EPHEMERAL_TOKEN_PATH}?access_token={token}")
        }
    }
}

fn classify_connect_error(e: tungstenite::Error) -> ConnectError {
    match e {
        tungstenite::Error::Http(response) => ConnectError::Rejected {
            status: response.status().as_u16(),
        },
        other => ConnectError::Ws(other),
    }
}

fn classify_send_error(e: tungstenite::Error) -> SendError {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            SendError::Closed
        }
        other => SendError::Ws(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_api_key() {
        let config = TransportConfig {
            auth: Auth::ApiKey("test-key-123".into()),
            ..Default::default()
        };
        let url = build_url(&config);
        assert!(url.starts_with("wss://generativelanguage.googleapis.com"));
        assert!(url.contains("BidiGenerateContent?key=test-key-123"));
        assert!(!url.contains("v1alpha"));
    }

    #[test]
    fn url_ephemeral_token() {
        let config = TransportConfig {
            auth: Auth::EphemeralToken("tok-abc".into()),
            ..Default::default()
        };
        let url = build_url(&config);
        assert!(url.contains("v1alpha"));
        assert!(url.contains("BidiGenerateContentConstrained?access_token=tok-abc"));
    }

    #[test]
    fn url_endpoint_override() {
        let config = TransportConfig {
            auth: Auth::ApiKey("ignored".into()),
            endpoint_override: Some("wss://custom.example.com/ws".into()),
            ..Default::default()
        };
        let url = build_url(&config);
        assert_eq!(url, "wss://custom.example.com/ws");
    }

    #[test]
    fn default_config_values() {
        let config = TransportConfig::default();
        assert_eq!(config.write_buffer_size, 64 * 1024);
        assert_eq!(config.max_frame_size, 16 * 1024 * 1024);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert!(config.endpoint_override.is_none());
    }
}
