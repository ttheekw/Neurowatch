//! What this plugin tells NeuroWatch about itself.
use nw_analytics_protocol::{
    messages::{
        Codec, EventKindInfo, Manifest, MediaRequest, OverlayLayer, StreamChoice, VideoRequest,
    },
    PROTOCOL_VERSION,
};
use serde_json::json;

pub const PLUGIN_ID: &str = "nw.motion";
/// The moving area while there is motion.
pub const DETECTIONS: &str = "detections";
/// What the detector sees: changed blocks, zones, the score.
pub const DEBUG: &str = "debug";

pub fn manifest() -> Manifest {
    Manifest {
        id: PLUGIN_ID.into(),
        name: "Motion detection".into(),
        vendor: "NeuroWatch".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL_VERSION,
        ping: true,
        locality_proof: None,
        identity: None,
        web_ui: Some(nw_analytics_protocol::web::WebUi {
            entry: "/".into(),
            title: "Motion dashboard".into(),
        }),
        permissions: vec![
            "video".into(),
            "events".into(),
            "overlays".into(),
            "decoded_frames".into(),
            "web_ui".into(),
        ],
        media: MediaRequest {
            decoded: Some(nw_analytics_protocol::decoded::DecodedRequest {
                formats: vec![
                    nw_analytics_protocol::decoded::RawFormat::Gray8,
                    nw_analytics_protocol::decoded::RawFormat::Nv12,
                    nw_analytics_protocol::decoded::RawFormat::Rgb24,
                ],
                max_fps: 5,
                min_width: 320,
                min_height: 180,
            }),
            video: VideoRequest {
                // 320×180 analysis: the sub stream is always enough.
                stream: StreamChoice::Sub,
                codecs: vec![Codec::H264, Codec::H265],
                keyframes_only: false,
                delivery: nw_analytics_protocol::messages::DeliveryMode::Latest,
                max_buffer_ms: 1000,
            },
        },
        config_schema: Some(config_schema()),
        event_kinds: vec![EventKindInfo {
            id: "motion".into(),
            label: "Motion".into(),
        }],
        overlay_layers: vec![
            OverlayLayer {
                id: DETECTIONS.into(),
                label: "Motion boxes".into(),
                default_on: true,
            },
            OverlayLayer {
                id: DEBUG.into(),
                label: "Analysis (changed blocks, zones, score)".into(),
                default_on: false,
            },
        ],
    }
}

/// JSON Schema of a binding's settings (`crate::settings::MotionSettings`).
fn config_schema() -> serde_json::Value {
    let point = json!({"type": "array", "items": {"type": "number", "minimum": 0, "maximum": 1},
                       "minItems": 2, "maxItems": 2});
    let decoder_values: Vec<&str> = crate::decoding::choices().iter().map(|c| c.0).collect();
    let decoder_labels: serde_json::Map<String, serde_json::Value> = crate::decoding::choices()
        .iter()
        .map(|(value, label)| ((*value).to_owned(), json!(label)))
        .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "sensitivity": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5,
                            "title": "Sensitivity",
                            "description": "Higher notices smaller changes, and gives more false alarms from rain, shadows or leaves."},
            "cooldown_sec": {"type": "number", "minimum": 0, "maximum": 600, "default": 10,
                             "title": "Ends after quiet (seconds)",
                             "description": "An event ends once nothing has moved for this long. Shorter splits a long visit into several events."},
            "min_duration_sec": {"type": "number", "minimum": 0, "maximum": 60, "default": 0.5,
                                 "title": "Minimum duration (seconds)",
                                 "description": "Movement shorter than this is ignored, such as a flash of light."},
            "low_cpu": {"type": "boolean", "default": false, "title": "Keyframes only (low CPU)",
                        "description": "Looks at about one picture every one or two seconds. Uses far less processing, and can miss quick movement."},
            "decoder": {
                "enum": decoder_values, "default": "auto", "x-nw-labels": decoder_labels,
                "x-nw-widget": "nw:decoder",
                "title": "Video decoder",
                "description": "What decodes this camera's video. Automatic and hardware decoders leave more of the processor free; a choice that cannot be used here falls back to the CPU, and the status says so."
            },
            "zones": {
                "type": "array", "maxItems": 32, "x-nw-widget": "nw:zones",
                "title": "Watched areas",
                "description": "Areas to watch or to ignore. With no watched area, the whole picture is watched.",
                "items": {
                    "type": "object", "required": ["id", "points"],
                    "properties": {
                        "id": {"type": "string", "minLength": 1},
                        "name": {"type": "string"},
                        "kind": {"enum": ["include", "exclude"], "default": "include"},
                        "points": {"type": "array", "items": point, "minItems": 3, "maxItems": 64}
                    }
                }
            }
        }
    })
}
