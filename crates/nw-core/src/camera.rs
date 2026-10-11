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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CameraRecordingConfig {
    pub schedule: Option<String>,
    #[serde(default)]
    pub local: LocalRecordingDest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraMotionConfig {
    pub enabled: bool,
    pub sensitivity: u8,
    pub cooldown_sec: f32,
    pub min_duration_sec: f32,
    pub exclusion_zones: Vec<serde_json::Value>,
}

impl Default for CameraMotionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sensitivity: 5,
            cooldown_sec: 10.0,
            min_duration_sec: 0.5,
            exclusion_zones: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CameraConfig, CameraRecordingConfig, CameraStream, MAIN_STREAM, SUB_STREAM};

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
