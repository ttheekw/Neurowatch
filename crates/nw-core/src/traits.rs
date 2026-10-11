//! Extension-point traits — define once in V1, implement across versions.
//!
//! All traits have a Null implementation so V1 compiles without optional crates.

use crate::{camera::CameraId, event::NeuroEvent, frame::VideoFrame};

// ---------------------------------------------------------------------------
// FrameConsumer
// V1: NullFrameConsumer (no-op)
// V3: OnnxInferenceConsumer (AI inference)
// ---------------------------------------------------------------------------

pub trait FrameConsumer: Send + Sync {
    fn consume_frame(&self, cam_id: &CameraId, frame: &VideoFrame);
}

pub struct NullFrameConsumer;

impl FrameConsumer for NullFrameConsumer {
    fn consume_frame(&self, _cam_id: &CameraId, _frame: &VideoFrame) {}
}

// ---------------------------------------------------------------------------
// EventBus
// V1: LogEventBus (logs to tracing)
// V2+: BroadcastEventBus (sends to IPC subscribers + logger)
// ---------------------------------------------------------------------------

pub trait EventBus: Send + Sync {
    fn publish(&self, event: NeuroEvent);
}

pub struct LogEventBus;

impl EventBus for LogEventBus {
    fn publish(&self, event: NeuroEvent) {
        tracing::debug!(?event, "EventBus: event published");
    }
}

// ---------------------------------------------------------------------------
// RecordingEngine
// V1: NullRecordingEngine (no-op)
// V2: SegmentedMp4Engine
// ---------------------------------------------------------------------------

pub trait RecordingEngine: Send + Sync {
    fn set_recording(&self, cam_id: &CameraId, enabled: bool);
    fn on_event(&self, cam_id: &CameraId, event: &NeuroEvent);
}

pub struct NullRecordingEngine;

impl RecordingEngine for NullRecordingEngine {
    fn set_recording(&self, _cam_id: &CameraId, _enabled: bool) {}
    fn on_event(&self, _cam_id: &CameraId, _event: &NeuroEvent) {}
}
