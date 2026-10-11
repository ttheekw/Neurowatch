//! Optional consumers of camera media inside the backend.
//!
//! The backend works without any of them. An extension (see
//! [`crate::extension`]) can install them on [`crate::app_state::AppState`]
//! before the cameras start, for example to make snapshots from pictures the
//! backend already has instead of opening another camera connection.
//!
//! Both are called on GStreamer streaming threads: return at once (see the
//! appsink rule in the coding standards).

/// Receives each keyframe a camera's recording ingest sees. GStreamer
/// buffers are reference counted, so passing one on is not a copy.
pub trait KeyframeSink: Send + Sync {
    fn record(&self, camera_id: &str, caps: gstreamer::Caps, buffer: gstreamer::Buffer);

    /// The camera was removed or its ingest restarted: drop what is kept for it.
    fn forget(&self, _camera_id: &str) {}
}

/// Wants decoded frames of some cameras from their desktop view decoders.
pub trait FrameTap: Send + Sync {
    /// Whether this camera's frames are wanted right now. A decoder that is
    /// not needed by a desktop view converts frames only while this is true.
    fn wants(&self, camera_id: &str) -> bool;

    /// One decoded frame of a camera whose frames are wanted.
    fn submit_sample(&self, camera_id: &str, sample: &gstreamer::Sample);
}
