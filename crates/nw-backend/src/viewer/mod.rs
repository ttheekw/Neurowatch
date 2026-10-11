//! Native viewer runtime — the "nw-viewer" slice.
//!
//! Owns per-camera GStreamer pipelines that decode compressed camera streams
//! and render directly to a UI-provided `VideoOverlay` window handle
//! (`glimagesink` / `xvimagesink`), eliminating per-frame raw-RGB copies
//! through Python.
//!
//! The viewer only ever accepts a `camera_id` (never a URL) from the UI so
//! that camera credentials never traverse the local IPC boundary.

use std::fmt;

pub mod capabilities;
pub mod manager;
pub mod session;

pub use capabilities::probe_capabilities;
pub use manager::{ViewerManager, WindowHandle};

/// Errors returned by the viewer runtime.
///
/// `Display` output is safe for logs and IPC responses: camera URLs and
/// credentials are never included, only opaque `camera_id` / `session_id`
/// values and a small vocabulary of error classes.
#[derive(Debug, thiserror::Error)]
pub enum ViewerError {
    #[error("Unknown camera: {camera_id}")]
    UnknownCamera { camera_id: String },
    #[error("Unknown viewer session: {session_id}")]
    UnknownSession { session_id: String },
    #[error("Window handle required")]
    #[allow(dead_code)]
    MissingWindowHandle,
    #[error("Native viewer unavailable: {reason}")]
    Unavailable { reason: String },
    #[error("Viewer pipeline failed for camera {camera_id}: {class}")]
    Pipeline {
        camera_id: String,
        /// Short error class only — never the raw upstream message.
        class: PipelineErrorClass,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineErrorClass {
    Build,
    StateChange,
    OverlayAttach,
}

impl fmt::Display for PipelineErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build => f.write_str("pipeline_build"),
            Self::StateChange => f.write_str("state_change"),
            Self::OverlayAttach => f.write_str("overlay_attach"),
        }
    }
}
