//! Bounded, credential-free diagnostics of existing shared camera connections.
use super::{lock, queue::ConsumerKind, Phase, SourceKey, StreamHubs, VideoFormat};
use nw_core::camera::CameraConfig;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct StreamConnection {
    pub camera_id: String,
    pub stream: String,
    pub state: &'static str,
    pub consumers: u32,
    pub roles: BTreeMap<ConsumerKind, u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    pub buffer_bytes: usize,
    pub buffer_units: usize,
    pub queue_drops: u64,
    /// `h264` or `h265`: the latest keyframe's codec, while connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<&'static str>,
    /// How the plugins on this stream get their video, counted (what portals
    /// before plugin names read). See [`with_plugins`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_paths: Option<PluginPaths>,
    /// The plugins on this stream: name, path and the decoder they report.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<StreamPlugin>,
    /// The recorder keeps a decoder running on this stream for plugins
    /// (`always_decode`), whether or not a desktop shows it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub always_decoded: bool,
    /// Desktop views showing the always-on decoder's pictures (no decoder of
    /// their own; not counted in `roles.desktop`).
    #[serde(skip_serializing_if = "is_zero")]
    pub shared_views: u32,
    /// Cloud Backup is copying this stream in real time.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cloud_backup: bool,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// One plugin on a stream, for Diagnostics (desktop and portal).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamPlugin {
    pub name: String,
    pub pictures: crate::plugin_host::PluginPictures,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder: Option<String>,
    /// Pictures shared with it a second, while it uses the video wall's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_fps: Option<f32>,
}

/// Longest plugin name or decoder text a stream row carries (characters).
const PLUGIN_TEXT: usize = 96;

fn short(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(PLUGIN_TEXT)
        .collect()
}

/// Plugin bindings of a stream by how their video reaches them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PluginPaths {
    /// Compressed video, decoded by the plugin itself.
    pub compressed: u32,
    /// The video wall's decoded pictures (no decoder of the plugin's own).
    pub shared: u32,
}

/// Each stream's active plugin bindings: named, and counted by path.
pub fn with_plugins(
    rows: &mut [StreamConnection],
    consumers: &[crate::plugin_host::PluginStreamConsumer],
) {
    for row in rows.iter_mut() {
        let mut paths = PluginPaths::default();
        for consumer in consumers
            .iter()
            .filter(|c| c.camera_id == row.camera_id && c.stream == row.stream)
        {
            match consumer.pictures {
                crate::plugin_host::PluginPictures::Compressed => paths.compressed += 1,
                crate::plugin_host::PluginPictures::Shared => paths.shared += 1,
            }
            row.plugins.push(StreamPlugin {
                name: short(&consumer.name),
                pictures: consumer.pictures,
                decoder: consumer
                    .decoder
                    .as_deref()
                    .map(short)
                    .filter(|text| !text.trim().is_empty()),
                shared_fps: consumer.shared_fps.filter(|fps| fps.is_finite()),
            });
        }
        if paths != PluginPaths::default() {
            row.plugin_paths = Some(paths);
        }
    }
}
impl StreamHubs {
    /// The picture size and measured rates of `camera`'s main and sub
    /// streams, as last seen since the recorder started (reading opens
    /// nothing). A stream not seen yet is left out; one closed since keeps
    /// what was seen. Only sizes and rates: never an address or credential.
    pub fn video_formats(&self, camera: &CameraConfig) -> BTreeMap<&'static str, VideoFormat> {
        let open: Vec<_> = lock(&self.hubs)
            .iter()
            .map(|(key, hub)| (key.clone(), hub.clone()))
            .collect();
        let now = Instant::now();
        let mut sources = vec![("main", SourceKey::of(camera))];
        if let Some(url) = camera.sub_url.as_ref().filter(|url| !url.trim().is_empty()) {
            let mut sub = camera.clone();
            sub.url = url.clone();
            sources.push(("sub", SourceKey::of(&sub)));
        }
        let mut remembered = lock(&self.formats);
        let mut formats = BTreeMap::new();
        for (stream, key) in sources {
            if let Some((_, hub)) = open.iter().find(|(source, _)| source == &key) {
                let core = lock(&hub.core);
                let size = core.size();
                let (_, rates, _) = core.metrics(now);
                drop(core);
                let seen = remembered.entry(key.clone()).or_default();
                if let Some((width, height)) = size {
                    seen.width = Some(width);
                    seen.height = Some(height);
                }
                if let Some(rates) = rates.filter(|rates| rates.fps > 0.0) {
                    seen.fps = Some(rates.fps);
                    seen.video_bps = Some(rates.video_bps);
                }
            }
            if let Some(format) = remembered
                .get(&key)
                .filter(|format| **format != VideoFormat::default())
            {
                formats.insert(stream, *format);
            }
        }
        formats
    }

    /// Reading never subscribes, opens a source, or resets another reader's rates.
    pub fn diagnostics(&self, cameras: &[CameraConfig]) -> Vec<StreamConnection> {
        let open: Vec<_> = lock(&self.hubs)
            .iter()
            .map(|(key, hub)| (key.clone(), hub.clone()))
            .collect();
        let now = Instant::now();
        let mut rows = Vec::new();
        for camera in cameras {
            let mut sources = vec![(SourceKey::of(camera), "main".to_owned())];
            if let Some(url) = camera.sub_url.as_ref().filter(|url| !url.is_empty()) {
                if url == &camera.url {
                    sources[0].1 = "main+sub".into();
                } else {
                    let mut sub = camera.clone();
                    sub.url = url.clone();
                    sources.push((SourceKey::of(&sub), "sub".into()));
                }
            }
            for (index, stream) in camera.streams.iter().enumerate() {
                let mut source = camera.clone();
                source.url = stream.url.clone();
                let key = SourceKey::of(&source);
                if !sources.iter().any(|(known, _)| known == &key) {
                    sources.push((key, format!("other_{}", index + 1)));
                }
            }
            // Include a previous source still lingering after a stream/configuration change.
            for (key, _) in open.iter().filter(|(key, _)| key.camera_id == camera.id) {
                if !sources.iter().any(|(known, _)| known == key) {
                    sources.push((key.clone(), format!("other_{}", sources.len() + 1)));
                }
            }
            for (key, stream) in sources {
                let mut row = StreamConnection {
                    camera_id: camera.id.clone(),
                    stream,
                    state: if super::enabled() && super::shareable(camera) {
                        "disconnected"
                    } else {
                        "unavailable"
                    },
                    consumers: 0,
                    roles: BTreeMap::new(),
                    video_bps: None,
                    audio_bps: None,
                    fps: None,
                    buffer_bytes: 0,
                    buffer_units: 0,
                    queue_drops: 0,
                    codec: None,
                    plugin_paths: None,
                    plugins: Vec::new(),
                    always_decoded: false,
                    shared_views: 0,
                    cloud_backup: false,
                };
                if let Some((_, hub)) = open.iter().find(|(source, _)| source == &key) {
                    let core = lock(&hub.core);
                    let (mut roles, rates, drops) = core.metrics(now);
                    row.always_decoded = roles.remove(&ConsumerKind::AlwaysDecode).is_some();
                    row.shared_views = roles.remove(&ConsumerKind::SharedView).unwrap_or(0);
                    row.cloud_backup = roles.remove(&ConsumerKind::CloudBackup).is_some();
                    row.consumers = roles.values().copied().fold(0u32, u32::saturating_add);
                    row.roles = roles;
                    (row.buffer_units, row.buffer_bytes) = core.cached();
                    row.queue_drops = drops;
                    row.codec = core.codec();
                    drop(core);
                    row.state = match hub.state().phase {
                        Phase::Connecting => "connecting",
                        Phase::Retrying => "retrying",
                        Phase::Closed => "disconnected",
                        Phase::Streaming if row.consumers == 0 => "idle",
                        Phase::Streaming => "streaming",
                    };
                    if let Some(rates) = rates {
                        row.video_bps = Some(rates.video_bps);
                        row.audio_bps = Some(rates.audio_bps);
                        row.fps = Some(rates.fps);
                    }
                }
                rows.push(row);
            }
        }
        rows
    }
}

/// Fit the additive stream details inside the existing signed snapshot size limit.
/// A truncation marker makes missing entries explicit on large installations.
pub fn attach(snapshot: &mut serde_json::Value, rows: Vec<StreamConnection>, max_bytes: usize) {
    let mut output = Vec::new();
    let mut used = serde_json::to_vec(snapshot)
        .map_or(max_bytes, |bytes| bytes.len())
        .saturating_add(100);
    if used > max_bytes {
        return;
    }
    let mut truncated = false;
    for row in rows {
        let Ok(value) = serde_json::to_value(row) else {
            truncated = true;
            continue;
        };
        let length = serde_json::to_vec(&value)
            .map_or(max_bytes, |bytes| bytes.len())
            .saturating_add(1);
        if used.saturating_add(length) > max_bytes {
            truncated = true;
            break;
        }
        used += length;
        output.push(value);
    }
    snapshot["stream_connections"] = serde_json::Value::Array(output);
    snapshot["stream_connections_truncated"] = serde_json::Value::Bool(truncated);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reading_disconnected_main_and_sub_never_opens_them_or_exposes_source_details() {
        let hubs = StreamHubs::default();
        let mut camera = CameraConfig::new(
            "private camera name",
            "rtsp://private-user:private-password@private-host/main",
        );
        camera.sub_url = Some("rtsp://private-host/sub".into());
        let rows = hubs.diagnostics(&[camera.clone()]);
        assert_eq!(hubs.open_count(), 0);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.state == "disconnected"
            && row.consumers == 0
            && row.video_bps.is_none()));
        let text = serde_json::to_string(&rows).unwrap();
        assert!(!text.contains("private"));
        camera.sub_url = Some(camera.url.clone());
        assert_eq!(hubs.diagnostics(&[camera])[0].stream, "main+sub");
    }
    /// Real H.264 through a shared connection: the parser's size and the
    /// measured frame rate are reported, and kept once the stream closes.
    #[tokio::test]
    async fn a_streams_size_and_rate_are_reported_and_kept_after_it_closes() {
        use super::super::{testing::CameraFile, SubscriberPolicy};
        use std::time::Duration;
        let fixture = CameraFile::sized(150, 640, 360);
        let hubs = StreamHubs::default();
        let camera = fixture.camera.clone();
        assert!(hubs.video_formats(&camera).is_empty(), "nothing seen yet");
        assert_eq!(hubs.open_count(), 0, "and asking opened nothing");
        let watcher = hubs
            .subscribe(&camera, SubscriberPolicy::recorder(), None)
            .unwrap();
        let reader = hubs.clone();
        let read = camera.clone();
        let format = tokio::task::spawn_blocking(move || {
            // Long enough for a measured rate (one second of pictures).
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let _ = watcher.recv(Duration::from_millis(200));
                if let Some(format) = reader.video_formats(&read).get("main").copied() {
                    if format.fps.is_some() {
                        drop(watcher);
                        return Some(format);
                    }
                }
            }
            drop(watcher);
            None
        })
        .await
        .unwrap()
        .expect("a size and frame rate within 10 s");
        assert_eq!((format.width, format.height), (Some(640), Some(360)));
        assert!(
            format.fps.unwrap() > 1.0 && format.video_bps.unwrap() > 0,
            "{format:?}"
        );
        // Closed: still known.
        for _ in 0..100 {
            if hubs.open_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(hubs.open_count(), 0);
        assert_eq!(hubs.video_formats(&camera).get("main"), Some(&format));
    }

    #[test]
    fn signed_snapshot_bounds_are_preserved_with_explicit_truncation() {
        let hubs = StreamHubs::default();
        let camera = CameraConfig::new("test", "rtsp://example.invalid/main");
        let rows = hubs.diagnostics(&[camera]);
        let mut value = serde_json::json!({"cameras":[]});
        attach(&mut value, rows, 200);
        assert!(value["stream_connections_truncated"].as_bool().unwrap());
        assert!(serde_json::to_vec(&value).unwrap().len() <= 200);
    }
}
