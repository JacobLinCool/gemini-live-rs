//! Screen capture for `/share-screen`.
//!
//! Captures a monitor or window at a configurable interval, encodes to JPEG,
//! and sends frames through a channel for the main loop to forward via
//! `session.send_video`.

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use xcap::{Monitor, Window};

/// Max dimension for captured frames (keeps JPEG size reasonable).
const MAX_DIMENSION: u32 = 1280;
const JPEG_QUALITY: u8 = 75;

// ── Listing ──────────────────────────────────────────────────────────────────

pub struct TargetInfo {
    pub id: usize,
    pub name: String,
    pub kind: &'static str,
    pub width: u32,
    pub height: u32,
}

/// List available capture targets (monitors first, then named windows).
pub fn list() -> Vec<TargetInfo> {
    let mut targets = Vec::new();
    let mut id = 0;

    if let Ok(monitors) = Monitor::all() {
        for m in monitors {
            let name = m.name().unwrap_or_default();
            let width = m.width().unwrap_or(0);
            let height = m.height().unwrap_or(0);
            targets.push(TargetInfo {
                id,
                name,
                kind: "monitor",
                width,
                height,
            });
            id += 1;
        }
    }

    if let Ok(windows) = Window::all() {
        for w in windows {
            let title = w.title().unwrap_or_default();
            let width = w.width().unwrap_or(0);
            let height = w.height().unwrap_or(0);
            if title.is_empty() || width == 0 || height == 0 {
                continue;
            }
            targets.push(TargetInfo {
                id,
                name: title,
                kind: "window",
                width,
                height,
            });
            id += 1;
        }
    }

    targets
}

// ── Capture session ──────────────────────────────────────────────────────────

/// An active screen-sharing session. Dropping it stops the capture thread.
pub struct ScreenShare {
    stop: Arc<AtomicBool>,
    pub target_name: String,
}

impl Drop for ScreenShare {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start capturing the target with the given `id` at `interval`.
/// JPEG frames are sent through `frame_tx`.
pub fn start(
    id: usize,
    interval: Duration,
    frame_tx: mpsc::Sender<Vec<u8>>,
) -> Result<ScreenShare, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    let monitor_count = monitors.len();

    let (target, name) = if id < monitor_count {
        let m = monitors.into_iter().nth(id).unwrap();
        let name = m.name().unwrap_or_default();
        (Target::Monitor(m), name)
    } else {
        let win_id = id - monitor_count;
        let windows: Vec<_> = Window::all()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|w| !w.title().unwrap_or_default().is_empty() && w.width().unwrap_or(0) > 0)
            .collect();
        let w = windows
            .into_iter()
            .nth(win_id)
            .ok_or_else(|| format!("target {id} not found"))?;
        let name = w.title().unwrap_or_default();
        (Target::Window(w), name)
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    std::thread::spawn(move || {
        capture_loop(target, interval, frame_tx, stop_clone);
    });

    Ok(ScreenShare {
        stop,
        target_name: name,
    })
}

// ── Internals ────────────────────────────────────────────────────────────────

enum Target {
    Monitor(Monitor),
    Window(Window),
}

fn capture_loop(
    target: Target,
    interval: Duration,
    tx: mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match capture_jpeg(&target) {
            Ok(jpeg) => {
                if tx.blocking_send(jpeg).is_err() {
                    break;
                }
            }
            Err(e) => tracing::warn!("capture error: {e}"),
        }
        std::thread::sleep(interval);
    }
}

fn capture_jpeg(target: &Target) -> Result<Vec<u8>, String> {
    let rgba = match target {
        Target::Monitor(m) => m.capture_image().map_err(|e| e.to_string())?,
        Target::Window(w) => w.capture_image().map_err(|e| e.to_string())?,
    };

    let dynamic = image::DynamicImage::ImageRgba8(rgba);

    let resized = if dynamic.width() > MAX_DIMENSION || dynamic.height() > MAX_DIMENSION {
        dynamic.resize(
            MAX_DIMENSION,
            MAX_DIMENSION,
            image::imageops::FilterType::Triangle,
        )
    } else {
        dynamic
    };

    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    resized
        .to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|e| e.to_string())?;
    Ok(buf.into_inner())
}
