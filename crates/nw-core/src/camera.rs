//! Camera configuration types.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type CameraId = String;

/// Controls where a configuration is expected to run.
///
/// The explicit default preserves the existing single-host application behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    #[default]
    LocalStandalone,
}

impl RuntimeMode {
    pub fn is_local_standalone(&self) -> bool {
        matches!(self, Self::LocalStandalone)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraConfig {
    pub id: CameraId,
    pub name: String,
    pub url: String,
    pub sub_url: Option<String>,
    pub username: Option<String>,
    pub password_b64: Option<String>,
    pub transport: Transport,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    pub notes: String,
    pub wall_position: Option<WallPosition>,
    #[serde(default)]
    pub recording: CameraRecordingConfig,
    #[serde(default)]
    pub motion: CameraMotionConfig,
    /// Every stream the camera offers (ONVIF media profiles, or URLs entered
    /// by hand), selectable per view. `url` stays the highest-resolution
    /// stream (recorded, and the single-view default) and `sub_url` the grid
    /// default. Empty for cameras configured before stream lists existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<CameraStream>,
    /// How the camera was found by a network search, so later searches show
    /// it as already added. Absent for cameras entered by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<crate::camera_discovery::DiscoveryIdentity>,
    /// The camera's ONVIF device service, once known: filled when the camera
    /// is added from a network search or found later (camera events, PTZ).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onvif: Option<OnvifEndpoint>,
}

/// Where a camera answers ONVIF. URLs are pinned to the camera's address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnvifEndpoint {
    pub device_url: String,
    /// Event service, from the device's capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_url: Option<String>,
    /// PTZ service, from the device's capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptz_url: Option<String>,
}

/// Stream selectors that name a role rather than a listed stream.
pub const MAIN_STREAM: &str = "main";
pub const SUB_STREAM: &str = "sub";

/// One selectable video stream of a camera.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraStream {
    /// Stable within the camera: `"1"`, `"2"`, … in list order. Never
    /// [`MAIN_STREAM`] or [`SUB_STREAM`].
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl CameraConfig {
    /// Streams a viewer may pick. A camera without a stream list offers its
    /// main URL and, when configured, its sub URL.
    pub fn stream_options(&self) -> Vec<CameraStream> {
        if !self.streams.is_empty() {
            return self.streams.clone();
        }
        let mut options = vec![CameraStream {
            id: "1".to_owned(),
            name: "Main".to_owned(),
            url: self.url.clone(),
            width: None,
            height: None,
        }];
        if let Some(sub) = self.sub_url.as_deref().filter(|s| !s.is_empty()) {
            options.push(CameraStream {
                id: "2".to_owned(),
                name: "Sub".to_owned(),
                url: sub.to_owned(),
                width: None,
                height: None,
            });
        }
        options
    }

    /// URL for a stream selector: [`MAIN_STREAM`], [`SUB_STREAM`] (the main
    /// URL when no sub stream is configured) or a stream id from
    /// [`Self::stream_options`]. `None` for an unknown stream id.
    pub fn stream_url(&self, selector: &str) -> Option<String> {
        match selector {
            MAIN_STREAM => Some(self.url.clone()),
            SUB_STREAM => Some(
                self.sub_url
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.url.clone()),
            ),
            id => self
                .stream_options()
                .into_iter()
                .find(|stream| stream.id == id)
                .map(|stream| stream.url),
        }
    }

    /// Renumber stream ids `"1"`, `"2"`, … in list order so every id is
    /// unique and never collides with a role selector.
    pub fn normalize_stream_ids(&mut self) {
        for (index, stream) in self.streams.iter_mut().enumerate() {
            stream.id = (index + 1).to_string();
        }
    }

    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            url: url.into(),
            sub_url: None,
            username: None,
            password_b64: None,
            transport: Transport::Tcp,
            protocol: default_protocol(),
            notes: String::new(),
            wall_position: None,
            recording: CameraRecordingConfig::default(),
            motion: CameraMotionConfig::default(),
            streams: Vec::new(),
            discovery: None,
            onvif: None,
        }
    }
}

fn default_protocol() -> String {
    "auto".to_owned()
}

/// Per-camera local-disk recording destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LocalRecordingDest {
    pub enabled: bool,
    pub storage_path: Option<String>,
    pub quota_gb: Option<u32>,
}

/// Per-camera recording configuration.
///
/// A legacy `s3` object from a pre-removal config file is silently ignored by
/// serde (no `deny_unknown_fields`); see the `serde_ignores_legacy_s3_object`
/// test below.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraRecordingConfig {
    /// Retained for old clients/configs; never interpreted as cron.
    pub schedule: Option<String>,
    #[serde(default)]
    pub local: LocalRecordingDest,
    pub mode: RecordingMode,
    pub weekly_rules: Vec<ScheduleRule>,
    pub event_pre_sec: u32,
    pub event_post_sec: u32,
    pub non_event_keep_hours: u32,
}
impl Default for CameraRecordingConfig {
    fn default() -> Self {
        Self {
            schedule: None,
            local: LocalRecordingDest::default(),
            mode: RecordingMode::Continuous,
            weekly_rules: Vec::new(),
            event_pre_sec: 10,
            event_post_sec: 15,
            non_event_keep_hours: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    #[default]
    Continuous,
    Events,
    Off,
}
/// Monday=0 through Sunday=6; node-local HH:MM boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRule {
    pub days: Vec<u8>,
    pub start: String,
    pub end: String,
    pub mode: RecordingMode,
}

/// Which motion detection a camera uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MotionSource {
    #[default]
    Off,
    /// NeuroWatch's own detector, on decoded pictures.
    Builtin,
    /// The camera's own detection, reported as ONVIF events.
    Onvif,
    Both,
}

impl MotionSource {
    pub fn uses_onvif(self) -> bool {
        matches!(self, Self::Onvif | Self::Both)
    }

    pub fn uses_builtin(self) -> bool {
        matches!(self, Self::Builtin | Self::Both)
    }
}

/// Whether a zone is watched or ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ZoneKind {
    #[default]
    Include,
    Exclude,
}

/// A polygon on the picture, in coordinates normalised to 0..1, so it
/// survives a change of stream resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MotionZone {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: ZoneKind,
    pub points: Vec<[f32; 2]>,
}

impl MotionZone {
    /// At least three points, every one inside the picture.
    pub fn is_valid(&self) -> bool {
        !self.id.is_empty()
            && self.points.len() >= 3
            && self
                .points
                .iter()
                .all(|[x, y]| (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y))
    }
}

/// Fields left out take their defaults, so a client may send only what it sets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraMotionConfig {
    /// Legacy switch for the built-in detector; [`Self::source`] decides.
    pub enabled: bool,
    pub source: MotionSource,
    pub sensitivity: u8,
    pub cooldown_sec: f32,
    pub min_duration_sec: f32,
    /// Decode independently decodable pictures only. Short movements between
    /// keyframes may be missed; normal analysis remains the default.
    pub low_cpu: bool,
    /// Include and exclude zones. Untyped entries from older configs (never
    /// written by any release) are dropped.
    #[serde(alias = "exclusion_zones", deserialize_with = "deserialize_zones")]
    pub zones: Vec<MotionZone>,
}

impl CameraMotionConfig {
    /// The detection in use: [`Self::source`], or the built-in detector for
    /// a config that only set the legacy `enabled` switch.
    pub fn effective_source(&self) -> MotionSource {
        match self.source {
            MotionSource::Off if self.enabled => MotionSource::Builtin,
            source => source,
        }
    }
}

fn deserialize_zones<'de, D>(deserializer: D) -> Result<Vec<MotionZone>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| serde_json::from_value::<MotionZone>(value).ok())
        .filter(MotionZone::is_valid)
        .collect())
}

impl Default for CameraMotionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: MotionSource::Off,
            sensitivity: 5,
            cooldown_sec: 10.0,
            min_duration_sec: 0.5,
            low_cpu: false,
            zones: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CameraConfig, CameraRecordingConfig, CameraStream, MAIN_STREAM, SUB_STREAM};

    #[test]
    fn recording_policy_defaults_preserve_continuous_capture_and_accept_weekly_rules() {
        let old: CameraRecordingConfig =
            serde_json::from_value(serde_json::json!({"local":{"enabled":true}})).unwrap();
        assert_eq!(old.mode, super::RecordingMode::Continuous);
        assert!(old.weekly_rules.is_empty());
        assert_eq!(old.event_pre_sec, 10);
        assert_eq!(old.event_post_sec, 15);
        let configured: CameraRecordingConfig = serde_json::from_value(serde_json::json!({
            "mode":"events", "weekly_rules":[{"days":[0,1,2,3,4],"start":"08:00","end":"18:00","mode":"continuous"}]
        })).unwrap();
        assert_eq!(configured.mode, super::RecordingMode::Events);
        assert_eq!(configured.weekly_rules.len(), 1);
    }

    #[test]
    fn serde_missing_protocol_defaults_to_auto() {
        let raw = serde_json::json!({
            "id": "cam-1",
            "name": "Cam 1",
            "url": "rtsp://example/stream",
            "sub_url": null,
            "username": null,
            "password_b64": null,
            "transport": "tcp",
            "notes": "",
            "wall_position": null
        });
        let cam: CameraConfig =
            serde_json::from_value(raw).expect("camera config should deserialize");
        assert_eq!(cam.protocol, "auto");
    }

    /// A legacy config file written before the direct-S3 feature was removed
    /// may still contain a `recording.s3` object with bucket/access/secret
    /// keys. It must deserialize without error and must not resurrect those
    /// credentials anywhere in the resulting config.
    #[test]
    fn serde_ignores_legacy_s3_object() {
        let raw = serde_json::json!({
            "id": "cam-legacy",
            "name": "Legacy Cam",
            "url": "rtsp://example/stream",
            "sub_url": null,
            "username": null,
            "password_b64": null,
            "transport": "tcp",
            "notes": "",
            "wall_position": null,
            "recording": {
                "schedule": null,
                "local": { "enabled": true, "storage_path": null, "quota_gb": null },
                "s3": {
                    "enabled": true,
                    "endpoint": "https://s3.example.invalid",
                    "bucket": "legacy-bucket",
                    "prefix": "site-a",
                    "access_key": "AKIA-LEGACY",
                    "secret_key": "legacy-secret",
                    "quota_gb": 50
                }
            }
        });
        let cam: CameraConfig =
            serde_json::from_value(raw).expect("legacy config with s3 object must deserialize");

        assert!(cam.recording.local.enabled);
        let reserialized =
            serde_json::to_value(&cam.recording).expect("recording config serializes");
        assert!(
            reserialized.get("s3").is_none(),
            "persisted config must not resurrect the legacy s3 object: {reserialized}"
        );
        let as_string = reserialized.to_string();
        for secret in ["legacy-bucket", "AKIA-LEGACY", "legacy-secret"] {
            assert!(
                !as_string.contains(secret),
                "persisted config leaked legacy S3 credential: {secret}"
            );
        }
    }

    /// The current recording config has no direct-S3 fields at all.
    #[test]
    fn recording_config_has_no_direct_s3_fields() {
        let rec = CameraRecordingConfig::default();
        let value = serde_json::to_value(&rec).expect("serializes");
        let obj = value.as_object().expect("recording config is an object");
        for key in obj.keys() {
            assert!(
                !key.eq_ignore_ascii_case("s3"),
                "recording config must not expose an s3 field"
            );
        }
    }

    fn stream(id: &str, url: &str) -> CameraStream {
        CameraStream {
            id: id.to_owned(),
            name: format!("Profile {id}"),
            url: url.to_owned(),
            width: Some(640),
            height: Some(360),
        }
    }

    #[test]
    fn camera_without_a_stream_list_offers_main_and_sub() {
        let mut cam = CameraConfig::new("Cam", "rtsp://cam/main");
        let ids: Vec<String> = cam.stream_options().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["1"]);

        cam.sub_url = Some("rtsp://cam/sub".to_owned());
        let options = cam.stream_options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[1].url, "rtsp://cam/sub");
        assert_eq!(cam.stream_url("2").as_deref(), Some("rtsp://cam/sub"));
    }

    #[test]
    fn stream_selectors_resolve_roles_and_listed_streams() {
        let mut cam = CameraConfig::new("Cam", "rtsp://cam/s1");
        assert_eq!(cam.stream_url(SUB_STREAM).as_deref(), Some("rtsp://cam/s1"));
        cam.sub_url = Some("rtsp://cam/s3".to_owned());
        cam.streams = vec![
            stream("1", "rtsp://cam/s1"),
            stream("2", "rtsp://cam/s2"),
            stream("3", "rtsp://cam/s3"),
        ];
        assert_eq!(
            cam.stream_url(MAIN_STREAM).as_deref(),
            Some("rtsp://cam/s1")
        );
        assert_eq!(cam.stream_url(SUB_STREAM).as_deref(), Some("rtsp://cam/s3"));
        assert_eq!(cam.stream_url("2").as_deref(), Some("rtsp://cam/s2"));
        assert_eq!(cam.stream_url("9"), None);
    }

    #[test]
    fn stream_ids_are_renumbered_so_they_never_collide_with_roles() {
        let mut cam = CameraConfig::new("Cam", "rtsp://cam/a");
        cam.streams = vec![
            stream("main", "rtsp://cam/a"),
            stream("main", "rtsp://cam/b"),
        ];
        cam.normalize_stream_ids();
        let ids: Vec<&str> = cam.streams.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn stream_list_round_trips_and_is_omitted_when_empty() {
        let mut cam = CameraConfig::new("Cam", "rtsp://cam/a");
        assert!(serde_json::to_value(&cam).unwrap().get("streams").is_none());
        cam.streams = vec![stream("1", "rtsp://cam/a")];
        let back: CameraConfig =
            serde_json::from_value(serde_json::to_value(&cam).unwrap()).unwrap();
        assert_eq!(back.streams, cam.streams);
    }

    #[test]
    fn wall_position_without_spans_is_a_single_cell() {
        let wp: super::WallPosition =
            serde_json::from_value(serde_json::json!({"page": 1, "row": 2, "col": 0})).unwrap();
        assert_eq!((wp.row_span, wp.col_span), (1, 1));
        let back = serde_json::to_value(&wp).unwrap();
        assert_eq!(back["row_span"], 1);
        assert_eq!(back["col_span"], 1);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Tcp,
    Udp,
}

/// Where a camera's tile sits on the desktop wall, and how many grid cells it
/// covers. `page`/`row`/`col` are the tile's top-left cell; the UI packs tiles
/// in that order, so sorting by them gives back the wall order. Spans are the
/// size the user chose; a smaller grid shows the tile clamped to fit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WallPosition {
    pub page: u32,
    pub row: u32,
    pub col: u32,
    #[serde(default = "WallPosition::single_span")]
    pub row_span: u32,
    #[serde(default = "WallPosition::single_span")]
    pub col_span: u32,
}

impl WallPosition {
    /// Largest span accepted from a request; no wall grid is this large.
    pub const MAX_SPAN: u32 = 16;

    fn single_span() -> u32 {
        1
    }
}

#[cfg(test)]
mod motion_tests {
    use super::{CameraConfig, MotionSource, ZoneKind};

    #[test]
    fn old_motion_config_loads_with_source_off_and_no_zones() {
        let raw = serde_json::json!({
            "id": "cam-1", "name": "Cam 1", "url": "rtsp://example/stream",
            "sub_url": null, "username": null, "password_b64": null,
            "transport": "tcp", "notes": "", "wall_position": null,
            "motion": {"enabled": false, "sensitivity": 5, "cooldown_sec": 10.0,
                       "min_duration_sec": 0.5, "exclusion_zones": [{"junk": 1}]}
        });
        let cam: CameraConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cam.motion.source, MotionSource::Off);
        assert!(cam.motion.zones.is_empty());
        assert!(!cam.motion.low_cpu);
        assert_eq!(cam.motion.effective_source(), MotionSource::Off);
        assert!(cam.onvif.is_none());
    }

    #[test]
    fn a_partial_motion_object_takes_defaults() {
        let motion: super::CameraMotionConfig =
            serde_json::from_value(serde_json::json!({"source": "onvif"})).unwrap();
        assert_eq!(motion.source, MotionSource::Onvif);
        assert_eq!(motion.sensitivity, 5);
    }

    #[test]
    fn legacy_enabled_switch_means_the_builtin_detector() {
        let mut cam = CameraConfig::new("Cam", "rtsp://example/stream");
        cam.motion.enabled = true;
        assert_eq!(cam.motion.effective_source(), MotionSource::Builtin);
        cam.motion.source = MotionSource::Onvif;
        assert_eq!(cam.motion.effective_source(), MotionSource::Onvif);
    }

    #[test]
    fn zones_keep_valid_polygons_only() {
        let raw = serde_json::json!({"sensitivity": 5, "cooldown_sec": 10.0, "min_duration_sec": 0.5,
        "source": "both",
        "zones": [
            {"id": "z1", "name": "Gate", "kind": "exclude", "points": [[0.0,0.0],[0.5,0.0],[0.5,0.5]]},
            {"id": "z2", "points": [[0.0,0.0],[0.5,0.0]]},
            {"id": "z3", "points": [[0.0,0.0],[1.5,0.0],[0.5,0.5]]}
        ]});
        let motion: super::CameraMotionConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(motion.source, MotionSource::Both);
        assert_eq!(motion.zones.len(), 1);
        assert_eq!(motion.zones[0].kind, ZoneKind::Exclude);
        let saved = serde_json::to_value(&motion).unwrap();
        assert!(saved.get("exclusion_zones").is_none());
        assert_eq!(saved["zones"][0]["id"], "z1");
    }
}
