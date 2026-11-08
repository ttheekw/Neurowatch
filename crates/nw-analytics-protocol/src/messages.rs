//! Typed parameters and results of every method (names in [`crate::method`]).
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---- Session ----------------------------------------------------------------

/// `hello` (node → plugin). The reply is the plugin's [`Manifest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub node: NodeInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locality: Option<crate::decoded::LocalityChallenge>,
    #[serde(default)]
    pub challenge: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// The site's name, for the plugin's logs and pages.
    pub name: String,
    pub version: String,
}

/// What a plugin is and what it needs. Everything NeuroWatch shows about a
/// plugin comes from here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Reverse-DNS style, stable across versions (`nw.motion`).
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub protocol: u32,
    /// What the plugin asks to receive and produce: `video`, `events`.
    #[serde(default)]
    pub permissions: Vec<String>,
    pub media: MediaRequest,
    /// JSON Schema of a binding's configuration document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    #[serde(default)]
    pub event_kinds: Vec<EventKindInfo>,
    /// What the plugin can draw over the camera's picture ([`Overlay`]).
    #[serde(default)]
    pub overlay_layers: Vec<OverlayLayer>,
    /// This plugin answers the optional liveness ping (older v1 peers do not).
    #[serde(default)]
    pub ping: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locality_proof: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<crate::auth::PluginIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_ui: Option<crate::web::WebUi>,
}

/// One kind of drawing a plugin sends, shown or hidden as a whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayLayer {
    pub id: String,
    pub label: String,
    /// Shown on the live wall unless the user chose otherwise. Layers that
    /// are not (debug drawings) show where the plugin's camera is examined.
    #[serde(default)]
    pub default_on: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventKindInfo {
    /// A NeuroWatch event kind (`motion`, `person`, `vehicle`, …).
    pub id: String,
    pub label: String,
}

// ---- Media ------------------------------------------------------------------

/// Which of the camera's streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamChoice {
    Main,
    /// The sub stream when the camera has one, else main.
    #[default]
    Sub,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    H265,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRequest {
    pub video: VideoRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoded: Option<crate::decoded::DecodedRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoRequest {
    #[serde(default)]
    pub stream: StreamChoice,
    pub codecs: Vec<Codec>,
    /// Send keyframes only (delta units are never sent).
    #[serde(default)]
    pub keyframes_only: bool,
    #[serde(default)]
    pub delivery: DeliveryMode,
    /// Reliable delivery may buffer this long; the node clamps to 100–10000 ms.
    #[serde(default = "default_buffer_ms")]
    pub max_buffer_ms: u32,
}

fn default_buffer_ms() -> u32 {
    1000
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    #[default]
    Latest,
    Reliable,
}

/// `stream_format` (node → plugin): the format of the units that follow,
/// before the first one and on every change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamFormat {
    pub binding: u32,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// The plugin fell behind; units were dropped.
    Behind,
    /// The camera connection was lost; units resume on a new connection.
    CameraLost,
}

/// `gap` (node → plugin): units were lost; the next unit is a keyframe
/// marked as a discontinuity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    pub binding: u32,
    pub reason: GapReason,
    /// Last delivered and first following identities (exclusive boundaries).
    /// Identities can be sparse; subtracting them does not count lost pictures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_seq: Option<u64>,
}

/// `camera_hint` (node → plugin): the camera is being moved (PTZ); a scene
/// change while it moves is not activity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraHint {
    pub binding: u32,
    pub moving: bool,
}

// ---- Bindings ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraInfo {
    pub id: String,
    pub name: String,
}

/// `attach` (node → plugin): bind a camera. Reply: [`Accepted`], or an
/// error `REJECTED` with a [`Refusal`] as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attach {
    pub binding: u32,
    pub camera: CameraInfo,
    pub config: Value,
}

/// What the plugin will consume for a binding (reply to `attach` and
/// `configure`). Fields left out keep the manifest's request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Accepted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyframes_only: Option<bool>,
}

/// Why `attach` was refused, shown to the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    /// `NO_CAPACITY`, `UNSUPPORTED_CODEC`, `HARDWARE`, `LICENSE_REQUIRED`, …
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `configure` (node → plugin): a running binding's new configuration.
/// Reply: [`Accepted`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Configure {
    pub binding: u32,
    pub config: Value,
}

/// `validate_config` (node → plugin), before a configuration is saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidateConfig {
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Validation {
    pub ok: bool,
    #[serde(default)]
    pub errors: Vec<ConfigError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigError {
    /// JSON pointer into the document (`""`: the whole document).
    pub path: String,
    pub message: String,
}

/// `detach` (node → plugin): stop a binding; the plugin ends its events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detach {
    pub binding: u32,
    pub reason: String,
}

// ---- Results ----------------------------------------------------------------

/// The palette a drawing uses; NeuroWatch picks the colours (themes, contrast).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    #[default]
    Accent,
    Success,
    Warning,
    Danger,
    Muted,
    /// Colours telling categories apart (one per vehicle type, say). A plugin
    /// names the same one on a drawing and on its [`Figure`], which is the key.
    Series1,
    Series2,
    Series3,
    Series4,
    Series5,
    Series6,
    Series7,
    Series8,
}

/// Where a text panel sits on the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// One drawing. Coordinates are normalised to the picture (0..1), so they
/// fit whichever stream and size shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayItem {
    /// `[x, y, width, height]`, with an optional label above it.
    Box {
        rect: [f32; 4],
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default)]
        style: Style,
        /// Filled lightly (a changed area) rather than outlined only.
        #[serde(default)]
        fill: bool,
    },
    Polygon {
        points: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default)]
        style: Style,
        #[serde(default)]
        fill: bool,
    },
    Polyline {
        points: Vec<[f32; 2]>,
        #[serde(default)]
        style: Style,
    },
    /// Lines of text in a corner of the picture.
    Text {
        #[serde(default)]
        anchor: Anchor,
        lines: Vec<String>,
        #[serde(default)]
        style: Style,
    },
}

/// `overlay` (plugin → node): what one layer shows now, replacing what it
/// showed before. Empty `items` clear it. Shown for `ttl_ms` at most (the
/// plugin sends again sooner while it has something to show).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overlay {
    pub binding: u32,
    pub layer: String,
    pub items: Vec<OverlayItem>,
    /// The frame it describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u32>,
}

/// `event.start` (plugin → node). `event` is the plugin's own number for
/// it, unique within the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventStart {
    pub binding: u32,
    pub event: u64,
    /// One of the manifest's `event_kinds`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// The frame it started on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// `event.update` (plugin → node): what the event covers so far.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventUpdate {
    pub binding: u32,
    pub event: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// Normalised `[x, y, width, height]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[f32; 4]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zones: Vec<String>,
}

/// `event.end` (plugin → node).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnd {
    pub binding: u32,
    pub event: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    /// Waiting for pictures (first keyframe, after a gap).
    Waiting,
    /// Analysing.
    Running,
    /// Paused while the camera moves.
    Settling,
    /// Failing; `detail` says why. The plugin retries on its own.
    Error,
}

/// `status` (plugin → node), at least every 5 s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub bindings: Vec<BindingStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingStatus {
    pub binding: u32,
    pub state: BindingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Pictures analysed per second.
    #[serde(default)]
    pub fps: f32,
    #[serde(default)]
    pub frames: u64,
    /// The plugin's decoder, if it decodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoder: Option<String>,
    /// A few numbers about this camera that viewers show beside its
    /// picture, never on it (today's counts, the analysis rate), in order.
    /// The node keeps at most [`MAX_FIGURES`], each clipped to
    /// [`MAX_FIGURE_TEXT`] characters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub figures: Vec<Figure>,
}

/// Most [`BindingStatus::figures`] a node keeps.
pub const MAX_FIGURES: usize = 16;
/// Longest label or value of a [`Figure`], in characters.
pub const MAX_FIGURE_TEXT: usize = 40;

/// One named number (`Lane 1`, `34`), shown as text, or with an icon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Figure {
    pub label: String,
    pub value: String,
    /// An icon NeuroWatch has: `car`, `bus`, `lorry`, `motorbike`, `person`,
    /// `van`, `three_wheeler`, `bicycle` or `total`. Others show the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The colour of what this figure counts in the plugin's drawings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<Style>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manifests_take_defaults_for_what_they_leave_out() {
        let manifest: Manifest = serde_json::from_value(json!({
            "id": "x.y", "name": "Y", "vendor": "X", "version": "1", "protocol": 1,
            "media": {"video": {"codecs": ["h264"]}}
        }))
        .unwrap();
        assert_eq!(manifest.media.video.stream, StreamChoice::Sub);
        assert!(!manifest.media.video.keyframes_only);
        assert!(manifest.event_kinds.is_empty());
    }

    #[test]
    fn overlays_read_with_defaults() {
        let overlay: Overlay = serde_json::from_value(json!({
            "binding": 1, "layer": "detections",
            "items": [
                {"type": "box", "rect": [0.1, 0.2, 0.3, 0.4], "label": "Motion"},
                {"type": "polygon", "points": [[0, 0], [1, 0], [1, 1]], "style": "danger", "fill": true},
                {"type": "polyline", "points": [[0, 0.5], [1, 0.5]]},
                {"type": "text", "anchor": "bottom_right", "lines": ["5 fps"]}
            ]
        }))
        .unwrap();
        assert_eq!(overlay.items.len(), 4);
        assert!(matches!(
            overlay.items[0],
            OverlayItem::Box {
                style: Style::Accent,
                fill: false,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(&overlay.items[3]).unwrap()["anchor"],
            "bottom_right"
        );
        assert!(serde_json::from_value::<OverlayItem>(json!({"type": "circle"})).is_err());
    }

    #[test]
    fn wire_names_are_snake_case() {
        let status = Status {
            bindings: vec![BindingStatus {
                binding: 1,
                state: BindingState::Running,
                detail: None,
                fps: 5.0,
                frames: 10,
                decoder: Some("avdec_h264".into()),
                figures: vec![],
            }],
        };
        assert_eq!(
            serde_json::to_value(status).unwrap()["bindings"][0]["state"],
            "running"
        );
        let gap = Gap {
            binding: 1,
            reason: GapReason::CameraLost,
            from_seq: None,
            to_seq: None,
        };
        assert_eq!(serde_json::to_value(gap).unwrap()["reason"], "camera_lost");
        assert_eq!(serde_json::to_value(Codec::H265).unwrap(), "h265");
    }
}
