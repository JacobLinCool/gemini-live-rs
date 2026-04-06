//! Request-response desktop control port for CLI-local tools.
//!
//! Desktop tools remain CLI-specific because they depend on product-owned
//! device lifecycles and persistence. This module still gives them a clear
//! execution boundary so the tool adapter can request state changes without
//! reaching into terminal state or device objects directly.

use std::fmt;

use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub enum DesktopControlAction {
    GetState,
    #[cfg(feature = "mic")]
    SetMicrophone {
        enabled: bool,
    },
    #[cfg(feature = "speak")]
    SetSpeaker {
        enabled: bool,
    },
    #[cfg(feature = "share-screen")]
    ListScreenTargets,
    #[cfg(feature = "share-screen")]
    SetScreenShare(ScreenShareRequest),
}

#[cfg(feature = "share-screen")]
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenShareRequest {
    pub enabled: bool,
    pub target_id: Option<usize>,
    pub interval_secs: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DesktopState {
    #[cfg(feature = "mic")]
    pub microphone_enabled: bool,
    #[cfg(feature = "speak")]
    pub speaker_enabled: bool,
    #[cfg(feature = "share-screen")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_share: Option<ActiveScreenShare>,
}

#[cfg(feature = "share-screen")]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveScreenShare {
    pub target_id: usize,
    pub interval_secs: f64,
    pub target_name: String,
}

#[cfg(feature = "share-screen")]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenTargetInfo {
    pub id: usize,
    pub kind: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DesktopControlResult {
    State {
        state: DesktopState,
    },
    StateChange {
        changed: bool,
        state: DesktopState,
    },
    #[cfg(feature = "share-screen")]
    ScreenTargets {
        targets: Vec<ScreenTargetInfo>,
    },
}

impl DesktopControlResult {
    pub fn into_json(self) -> Value {
        serde_json::to_value(self).expect("desktop control results must serialize")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopControlError {
    InvalidRequest { message: String },
    ExecutionFailed { message: String },
    Unavailable { message: String },
}

impl DesktopControlError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    pub fn execution_failed(message: impl Into<String>) -> Self {
        Self::ExecutionFailed {
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }
}

impl fmt::Display for DesktopControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest { message } => f.write_str(message),
            Self::ExecutionFailed { message } => f.write_str(message),
            Self::Unavailable { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for DesktopControlError {}

pub trait DesktopControlPort: Send + Sync + 'static {
    fn execute<'a>(
        &'a self,
        action: DesktopControlAction,
    ) -> BoxFuture<'a, Result<DesktopControlResult, DesktopControlError>>;
}

#[derive(Clone)]
pub struct ChannelDesktopControlPort {
    tx: mpsc::Sender<DesktopControlRequest>,
}

pub struct DesktopControlServer {
    rx: mpsc::Receiver<DesktopControlRequest>,
}

pub struct DesktopControlRequest {
    action: DesktopControlAction,
    reply_tx: oneshot::Sender<Result<DesktopControlResult, DesktopControlError>>,
}

pub fn channel() -> (ChannelDesktopControlPort, DesktopControlServer) {
    let (tx, rx) = mpsc::channel(8);
    (
        ChannelDesktopControlPort { tx },
        DesktopControlServer { rx },
    )
}

impl DesktopControlServer {
    pub async fn recv(&mut self) -> Option<DesktopControlRequest> {
        self.rx.recv().await
    }
}

impl DesktopControlRequest {
    pub fn action(&self) -> &DesktopControlAction {
        &self.action
    }

    pub fn respond(self, result: Result<DesktopControlResult, DesktopControlError>) {
        let _ = self.reply_tx.send(result);
    }
}

impl DesktopControlPort for ChannelDesktopControlPort {
    fn execute<'a>(
        &'a self,
        action: DesktopControlAction,
    ) -> BoxFuture<'a, Result<DesktopControlResult, DesktopControlError>> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(DesktopControlRequest { action, reply_tx })
                .await
                .map_err(|_| {
                    DesktopControlError::unavailable(
                        "desktop control host is no longer accepting requests",
                    )
                })?;
            reply_rx.await.map_err(|_| {
                DesktopControlError::unavailable(
                    "desktop control host dropped the response channel",
                )
            })?
        })
    }
}
