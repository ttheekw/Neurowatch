//! Application event types published on the EventBus.

use crate::camera::CameraId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NeuroEvent {
    /// Camera pipeline changed state (streaming, reconnecting, error, stopped).
    PipelineState {
        cam_id: CameraId,
        state: String,
        detail: Option<String>,
    },
    /// Motion detected on a camera (V2+).
    Motion {
        cam_id: CameraId,
        timestamp_us: i64,
        sensitivity: u8,
    },
    /// AI object detected on a camera (V3+).
    ObjectDetected {
        cam_id: CameraId,
        timestamp_us: i64,
        label: String,
        confidence: f32,
        bbox: [f32; 4], // [x, y, width, height] normalised 0..1
        payload: serde_json::Value,
    },
    /// Recording segment completed (V2+).
    RecordingSegment {
        cam_id: CameraId,
        path: String,
        start_us: i64,
        end_us: i64,
        flags: u32,
    },
}
