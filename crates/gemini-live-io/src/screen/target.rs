use xcap::{Monitor, Window};

use crate::error::ScreenCaptureError;

/// A shareable desktop capture target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTarget {
    pub id: usize,
    pub name: String,
    pub kind: CaptureTargetKind,
    pub width: u32,
    pub height: u32,
}

/// Logical type of a capture target exposed by the desktop host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTargetKind {
    Monitor,
    Window,
}

impl std::fmt::Display for CaptureTargetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Monitor => f.write_str("monitor"),
            Self::Window => f.write_str("window"),
        }
    }
}

pub fn list_targets() -> Result<Vec<CaptureTarget>, ScreenCaptureError> {
    Ok(enumerate_targets()?
        .into_iter()
        .map(|target| target.metadata)
        .collect())
}

pub(super) fn resolve_target(id: usize) -> Result<ResolvedCaptureTarget, ScreenCaptureError> {
    enumerate_targets()?
        .into_iter()
        .find(|target| target.metadata.id == id)
        .ok_or(ScreenCaptureError::TargetNotFound(id))
}

pub(super) struct ResolvedCaptureTarget {
    pub metadata: CaptureTarget,
    pub handle: CaptureHandle,
}

pub(super) enum CaptureHandle {
    Monitor(Monitor),
    Window(Window),
}

fn enumerate_targets() -> Result<Vec<ResolvedCaptureTarget>, ScreenCaptureError> {
    let mut targets = Vec::new();
    let mut id = 0usize;

    for monitor in
        Monitor::all().map_err(|e| ScreenCaptureError::EnumerateTargets(e.to_string()))?
    {
        let metadata = CaptureTarget {
            id,
            name: monitor.name().unwrap_or_default(),
            kind: CaptureTargetKind::Monitor,
            width: monitor.width().unwrap_or(0),
            height: monitor.height().unwrap_or(0),
        };
        targets.push(ResolvedCaptureTarget {
            metadata,
            handle: CaptureHandle::Monitor(monitor),
        });
        id += 1;
    }

    for window in Window::all().map_err(|e| ScreenCaptureError::EnumerateTargets(e.to_string()))? {
        let title = window.title().unwrap_or_default();
        let width = window.width().unwrap_or(0);
        let height = window.height().unwrap_or(0);
        if title.is_empty() || width == 0 || height == 0 {
            continue;
        }

        let metadata = CaptureTarget {
            id,
            name: title,
            kind: CaptureTargetKind::Window,
            width,
            height,
        };
        targets.push(ResolvedCaptureTarget {
            metadata,
            handle: CaptureHandle::Window(window),
        });
        id += 1;
    }

    Ok(targets)
}
