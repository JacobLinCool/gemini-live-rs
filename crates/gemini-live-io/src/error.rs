//! Typed errors for desktop media adapters.
//!
//! The public adapters in this crate return stable error enums instead of raw
//! strings so host applications can report failures consistently without
//! depending directly on backend-specific library errors.

use thiserror::Error;

/// Errors surfaced by microphone, speaker, and AEC setup.
#[derive(Debug, Error)]
pub enum AudioIoError {
    #[error("failed to create AEC processor: {0}")]
    AecInit(String),
    #[error("no default input device")]
    NoInputDevice,
    #[error("no default output device")]
    NoOutputDevice,
    #[error("failed to query default input config: {0}")]
    DefaultInputConfig(String),
    #[error("failed to query default output config: {0}")]
    DefaultOutputConfig(String),
    #[error("unsupported input sample format: {0}")]
    UnsupportedInputFormat(String),
    #[error("unsupported output sample format: {0}")]
    UnsupportedOutputFormat(String),
    #[error("failed to build input stream: {0}")]
    BuildInputStream(String),
    #[error("failed to build output stream: {0}")]
    BuildOutputStream(String),
    #[error("failed to start audio stream: {0}")]
    StartStream(String),
}

/// Errors surfaced by screen-target enumeration and frame capture.
#[derive(Debug, Error)]
pub enum ScreenCaptureError {
    #[error("failed to enumerate capture targets: {0}")]
    EnumerateTargets(String),
    #[error("capture target {0} not found")]
    TargetNotFound(usize),
    #[error("invalid JPEG quality {0}; expected 0..=100")]
    InvalidJpegQuality(u8),
    #[error("failed to capture frame: {0}")]
    CaptureFrame(String),
    #[error("failed to encode frame as JPEG: {0}")]
    EncodeFrame(String),
}
