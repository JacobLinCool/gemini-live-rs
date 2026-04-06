//! Error types for the Discord host layer.

use gemini_live::SessionError;
use gemini_live_runtime::RuntimeError;
use serenity::all::GatewayError;
use songbird::error::JoinError;
use songbird::input::MakePlayableError;
use songbird::tracks::ControlError;

/// Startup configuration failures.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ConfigError {
    #[error("missing required environment variable {key}")]
    MissingEnv { key: &'static str },
    #[error("environment variable {key} must not be empty")]
    EmptyEnv { key: &'static str },
    #[error("environment variable {key} must be a non-zero Discord id, got {value:?}")]
    InvalidDiscordId { key: &'static str, value: String },
}

/// Host-layer service errors.
#[derive(Debug, thiserror::Error)]
pub enum DiscordServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Serenity(Box<serenity::Error>),
    #[error(transparent)]
    SongbirdJoin(Box<JoinError>),
    #[error(transparent)]
    SongbirdControl(Box<ControlError>),
    #[error(transparent)]
    SongbirdInput(Box<MakePlayableError>),
    #[error("songbird voice manager is not registered in the Serenity client")]
    SongbirdUnavailable,
    #[error("voice playback bridge is closed")]
    VoicePlaybackClosed,
    #[error("TODO(gemini-live-discord): {detail}")]
    Unimplemented { detail: &'static str },
}

impl DiscordServiceError {
    pub fn unimplemented(detail: &'static str) -> Self {
        Self::Unimplemented { detail }
    }

    pub fn startup_hint(&self) -> Option<&'static str> {
        match self {
            Self::Serenity(error)
                if matches!(
                    error.as_ref(),
                    serenity::Error::Gateway(GatewayError::DisallowedGatewayIntents)
                ) =>
            {
                Some(
                    "Discord rejected the requested gateway intents. \
Enable MESSAGE CONTENT INTENT for this bot in the Discord Developer Portal: \
Application -> Bot -> Privileged Gateway Intents.",
                )
            }
            _ => None,
        }
    }
}

impl From<serenity::Error> for DiscordServiceError {
    fn from(error: serenity::Error) -> Self {
        Self::Serenity(Box::new(error))
    }
}

impl From<JoinError> for DiscordServiceError {
    fn from(error: JoinError) -> Self {
        Self::SongbirdJoin(Box::new(error))
    }
}

impl From<ControlError> for DiscordServiceError {
    fn from(error: ControlError) -> Self {
        Self::SongbirdControl(Box::new(error))
    }
}

impl From<MakePlayableError> for DiscordServiceError {
    fn from(error: MakePlayableError) -> Self {
        Self::SongbirdInput(Box::new(error))
    }
}
