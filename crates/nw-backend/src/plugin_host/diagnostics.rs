//! Attribution of current plugin media subscriptions: which plugin gets which
//! stream, and how its video reaches it. Names, paths and the plugin's own
//! decoder words (clipped, no control characters) also reach the portal's
//! diagnostics (`streams::diagnostics::with_plugins`); never addresses,
//! credentials or manifests.
use super::{PluginHost, PluginsConfig};
use nw_analytics_protocol::messages::StreamChoice;
use nw_core::camera::CameraConfig;
use serde::Serialize;
use std::collections::HashMap;

/// How a plugin binding gets its video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginPictures {
    /// The camera's compressed video, decoded by the plugin.
    Compressed,
    /// The video wall's decoded pictures, shared (no decoder of its own).
    Shared,
}

#[derive(Debug, Serialize)]
pub struct PluginStreamConsumer {
    pub camera_id: String,
    pub stream: &'static str,
    pub name: String,
    /// `h264` or `h265`: the compressed video sent to the plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<&'static str>,
    pub pictures: PluginPictures,
    /// The decoder the plugin reports using (its own words, clipped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder: Option<String>,
    /// Pictures shared with it a second, while `pictures` is `shared`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_fps: Option<f32>,
    /// Who decoded those pictures and on what, once known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_source: Option<super::SharedSource>,
}

impl PluginHost {
    /// Report active media bindings, excluding offline, refused, removed or disabled ones.
    /// The desktop intersects these names with measured plugin subscription counts.
    pub fn stream_consumers(
        &self,
        config: &PluginsConfig,
        cameras: &[CameraConfig],
    ) -> Vec<PluginStreamConsumer> {
        let Ok(statuses) = self.status.lock() else {
            return vec![];
        };
        let cameras: HashMap<_, _> = cameras
            .iter()
            .map(|camera| (camera.id.as_str(), camera))
            .collect();
        config
            .bindings
            .iter()
            .filter(|binding| binding.enabled)
            .filter_map(|binding| {
                let server = config
                    .servers
                    .iter()
                    .find(|server| server.id == binding.server && server.enabled)?;
                let status = statuses
                    .get(&server.id)
                    .filter(|status| status.state == "connected")?;
                let active = status
                    .bindings
                    .get(&binding.camera_id)
                    .filter(|binding| binding.state == "active")?;
                let choice = active.stream?;
                let camera = cameras.get(binding.camera_id.as_str())?;
                let sub = camera.sub_url.as_deref().filter(|url| !url.is_empty());
                let stream = if sub == Some(camera.url.as_str()) {
                    "main+sub"
                } else if choice == StreamChoice::Sub && sub.is_some() {
                    "sub"
                } else {
                    "main"
                };
                let name: String = server
                    .shown_name(Some(&status.manifest.as_ref()?.name))
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(128)
                    .collect();
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                let codec = active.format.as_deref().and_then(|format| {
                    if format.starts_with("H.264") {
                        Some("h264")
                    } else if format.starts_with("H.265") {
                        Some("h265")
                    } else {
                        None
                    }
                });
                Some(PluginStreamConsumer {
                    camera_id: binding.camera_id.clone(),
                    stream,
                    name: name.to_owned(),
                    codec,
                    pictures: if active.shared_pictures {
                        PluginPictures::Shared
                    } else {
                        PluginPictures::Compressed
                    },
                    decoder: active
                        .plugin
                        .as_ref()
                        .and_then(|status| status.decoder.clone())
                        .filter(|decoder| !decoder.trim().is_empty()),
                    shared_fps: active.shared_fps.filter(|_| active.shared_pictures),
                    shared_source: active.shared_source.filter(|_| active.shared_pictures),
                })
            })
            .collect()
    }
}
