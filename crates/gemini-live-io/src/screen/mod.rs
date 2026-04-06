//! Desktop screen-capture adapters backed by `xcap`.
//!
//! Hosts use [`list_targets`] to discover shareable monitors and windows, then
//! create a [`ScreenCapture`] to stream JPEG-encoded frames into their runtime.

mod capture;
mod target;

pub use capture::{EncodedFrame, ScreenCapture, ScreenCaptureConfig};
pub use target::{CaptureTarget, CaptureTargetKind, list_targets};
