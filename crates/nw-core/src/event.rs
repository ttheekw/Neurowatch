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

/// What happened. Stored as its snake_case name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Motion,
    /// Something entered a watched area (camera analytics).
    Intrusion,
    LineCrossing,
    /// The camera was covered, moved or defocused.
    Tamper,
    /// A camera's alarm input changed.
    DigitalInput,
    /// Another camera event, named by its ONVIF topic.
    CameraEvent,
    CameraOffline,
    RecordingFailed,
    DiskLow,
    /// A recording location is not there (an unmounted drive or network
    /// share): nothing is recorded to it until it is back.
    StorageMissing,
    BackendStarted,
    Bookmark,
}

impl EventKind {
    pub const ALL: [EventKind; 12] = [
        Self::Motion,
        Self::Intrusion,
        Self::LineCrossing,
        Self::Tamper,
        Self::DigitalInput,
        Self::CameraEvent,
        Self::CameraOffline,
        Self::RecordingFailed,
        Self::DiskLow,
        Self::StorageMissing,
        Self::BackendStarted,
        Self::Bookmark,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Intrusion => "intrusion",
            Self::LineCrossing => "line_crossing",
            Self::Tamper => "tamper",
            Self::DigitalInput => "digital_input",
            Self::CameraEvent => "camera_event",
            Self::CameraOffline => "camera_offline",
            Self::RecordingFailed => "recording_failed",
            Self::DiskLow => "disk_low",
            Self::StorageMissing => "storage_missing",
            Self::BackendStarted => "backend_started",
            Self::Bookmark => "bookmark",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

/// Who reported an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// NeuroWatch's own motion detector.
    Builtin,
    /// The camera, over ONVIF.
    Onvif,
    /// NeuroWatch itself (camera offline, recording failed, disk).
    System,
    /// A person (bookmarks).
    User,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Onvif => "onvif",
            Self::System => "system",
            Self::User => "user",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [Self::Builtin, Self::Onvif, Self::System, Self::User]
            .into_iter()
            .find(|source| source.as_str() == name)
    }
}

/// One stored event. Times are milliseconds since the Unix epoch, like the
/// recording index. `end_ms` is `None` while the event is still going on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: i64,
    /// `None` for events of the whole node (disk low, backend started).
    pub camera_id: Option<CameraId>,
    pub kind: EventKind,
    pub source: EventSource,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    /// The camera's ONVIF topic, for camera events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Strength, 0..1 (motion area, analytics confidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// Ids of the zones that fired.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zones: Vec<String>,
    /// Union of the moving area, normalised `[x, y, width, height]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[f32; 4]>,
    /// Picture of the event, relative to the events directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    /// Short human-readable detail (the failure, the input's name, a note).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub acknowledged: bool,
    /// A bookmark whose recordings are kept: cleanup never deletes the
    /// footage it covers, and the bookmark never expires.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kept: bool,
}

/// What changed about an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPhase {
    Start,
    Update,
    End,
}

/// An event as it starts, changes and ends, numbered in order (`seq`) so a
/// client can ask for everything after the last one it saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventNotice {
    pub seq: u64,
    pub phase: EventPhase,
    pub event: EventRecord,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_names() {
        for kind in EventKind::ALL {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.as_str())
            );
        }
        assert_eq!(EventKind::parse("nope"), None);
    }

    #[test]
    fn sources_round_trip_through_their_names() {
        for source in [
            EventSource::Builtin,
            EventSource::Onvif,
            EventSource::System,
            EventSource::User,
        ] {
            assert_eq!(EventSource::parse(source.as_str()), Some(source));
        }
    }
}
