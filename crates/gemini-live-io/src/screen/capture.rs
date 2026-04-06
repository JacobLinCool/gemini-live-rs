use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::error::ScreenCaptureError;

use super::target::{CaptureHandle, CaptureTarget, resolve_target};

const JPEG_MIME_TYPE: &str = "image/jpeg";

/// A single encoded screen-capture frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
}

/// Configuration for a running screen-capture session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCaptureConfig {
    pub interval: Duration,
    pub max_dimension: u32,
    pub jpeg_quality: u8,
}

impl Default for ScreenCaptureConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            max_dimension: 1280,
            jpeg_quality: 75,
        }
    }
}

/// Active screen-capture worker. Dropping it stops the worker thread.
pub struct ScreenCapture {
    stop: Arc<AtomicBool>,
    pub target: CaptureTarget,
}

impl std::fmt::Debug for ScreenCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenCapture")
            .field("target", &self.target)
            .finish()
    }
}

impl ScreenCapture {
    /// Start capturing a monitor or window by target id.
    pub fn start(
        target_id: usize,
        config: ScreenCaptureConfig,
        tx: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self, ScreenCaptureError> {
        if config.jpeg_quality > 100 {
            return Err(ScreenCaptureError::InvalidJpegQuality(config.jpeg_quality));
        }

        let resolved = resolve_target(target_id)?;
        let target = resolved.metadata.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);

        std::thread::spawn(move || {
            capture_loop(resolved.handle, config, tx, worker_stop);
        });

        Ok(Self { stop, target })
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn capture_loop(
    target: CaptureHandle,
    config: ScreenCaptureConfig,
    tx: mpsc::Sender<EncodedFrame>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match capture_jpeg(&target, config.max_dimension, config.jpeg_quality) {
            Ok(bytes) => {
                if tx
                    .blocking_send(EncodedFrame {
                        bytes,
                        mime_type: JPEG_MIME_TYPE,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => tracing::warn!("capture error: {error}"),
        }

        std::thread::sleep(config.interval);
    }
}

fn capture_jpeg(
    target: &CaptureHandle,
    max_dimension: u32,
    jpeg_quality: u8,
) -> Result<Vec<u8>, ScreenCaptureError> {
    let rgba = match target {
        CaptureHandle::Monitor(monitor) => monitor
            .capture_image()
            .map_err(|e| ScreenCaptureError::CaptureFrame(e.to_string()))?,
        CaptureHandle::Window(window) => window
            .capture_image()
            .map_err(|e| ScreenCaptureError::CaptureFrame(e.to_string()))?,
    };

    let dynamic_image = image::DynamicImage::ImageRgba8(rgba);
    let resized = if dynamic_image.width() > max_dimension || dynamic_image.height() > max_dimension
    {
        dynamic_image.resize(
            max_dimension,
            max_dimension,
            image::imageops::FilterType::Triangle,
        )
    } else {
        dynamic_image
    };

    let mut buffer = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, jpeg_quality);
    resized
        .to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|e| ScreenCaptureError::EncodeFrame(e.to_string()))?;
    Ok(buffer.into_inner())
}
