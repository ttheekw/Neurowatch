//! Motion was bundled with NeuroWatch before 0.1.8: the recorder started its
//! own `nw-motion` child as the server [`ID`]. Motion is now a separate
//! plugin, installed from its own release and added like any other
//! (2026-11-16, the user's choice). This keeps configurations from both eras
//! working:
//!
//! - [`migrate_document`]: the much older detector built into the recorder
//!   (`motion.source` builtin/both, or the `enabled` switch) becomes a binding
//!   to the Motion server, with its settings, before the typed config loads.
//! - [`migrate`]: a server left from the bundled era is pointed at the address
//!   a separately installed Motion listens on by default, without the
//!   bundled copy's automatic approval. Its bindings (zones, sensitivity) are
//!   kept; once Motion is installed it connects and an administrator approves
//!   its key on the Plugins page. Until then it is offline, which the
//!   Plugins page, the tray and the status bar say.
//!
//! Nothing here starts a program or adds Motion to a configuration that never
//! had it.
use super::PluginServer;
use crate::config::AppConfig;

use std::path::{Path, PathBuf};

/// The id of the Motion server from the bundled era (kept: bindings name it).
pub const ID: &str = "nw-motion-bundled";
/// Motion's manifest id.
const PLUGIN_ID: &str = "nw.motion";

/// Where the bundled copy listened: one address per configuration file.
fn bundled_address(config: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(config.to_string_lossy().as_bytes());
    let suffix: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if cfg!(windows) {
        format!(r"pipe:\\.\pipe\neurowatch-motion-{suffix}")
    } else {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        format!(
            "unix:{}",
            runtime
                .join("neurowatch/plugins")
                .join(format!("motion-{suffix}.sock"))
                .display()
        )
    }
}

/// Where a separately installed Motion listens unless told otherwise
/// (Neurowatch-Motion `default_address`; the desktop's Add Plugin suggests it).
pub fn standalone_address() -> String {
    if cfg!(windows) {
        r"pipe:\\.\pipe\neurowatch-motion".into()
    } else {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|dir| !dir.is_empty())
            .unwrap_or_else(|| "/tmp".into());
        format!("unix:{runtime}/neurowatch/plugins/nw-motion.sock")
    }
}

/// Carry a bundled-era Motion server (or bindings converted from the older
/// detector) over to a separately installed Motion; `true` when changed.
pub fn migrate(config: &mut AppConfig, path: &Path) -> bool {
    let mut changed = false;
    let bound = config
        .plugins
        .bindings
        .iter()
        .any(|binding| binding.server == ID);
    if bound && !config.plugins.servers.iter().any(|server| server.id == ID) {
        config.plugins.servers.push(PluginServer {
            id: ID.into(),
            address: standalone_address(),
            enabled: true,
            approval: None,
            credential_revision: 0,
            plugin_id: Some(PLUGIN_ID.into()),
            managed: None,
        });
        changed = true;
    }
    if let Some(server) = config
        .plugins
        .servers
        .iter_mut()
        .find(|server| server.id == ID && server.address == bundled_address(path))
    {
        // The bundled copy was approved by its own key; an installed Motion
        // has another, which an administrator approves.
        server.address = standalone_address();
        server.approval = None;
        changed = true;
    }
    changed
}

/// Runs before deserialization: obsolete detector sources never enter runtime
/// types. The original settings are normalized with the legacy data reader.
pub fn migrate_document(document: &mut serde_json::Value) -> Result<bool, String> {
    use serde_json::{json, Value};
    let mut bindings = Vec::new();
    let mut changed = false;
    for name in ["cameras", "cameras_over_limit"] {
        let Some(cameras) = document.get_mut(name).and_then(Value::as_array_mut) else {
            continue;
        };
        for camera in cameras {
            let Some(motion) = camera.get("motion") else {
                continue;
            };
            let source = motion["source"].as_str().unwrap_or("off");
            if !(matches!(source, "builtin" | "both")
                || source == "off" && motion["enabled"] == true)
            {
                continue;
            }
            let onvif = source == "both";
            let mut compatible = motion.clone();
            compatible["source"] = json!("off");
            let settings: nw_core::camera::CameraMotionConfig =
                serde_json::from_value(compatible).map_err(|error| error.to_string())?;
            bindings.push(
                json!({"server":ID,"camera_id":camera["id"],"enabled":true,"config":settings}),
            );
            camera["motion"]["source"] = json!(if onvif { "onvif" } else { "off" });
            camera["motion"]["enabled"] = json!(false);
            changed = true;
        }
    }
    if !bindings.is_empty() {
        if document.get("plugins").is_none() {
            document["plugins"] = json!({});
        }
        if document["plugins"].get("bindings").is_none() {
            document["plugins"]["bindings"] = json!([]);
        }
        let saved = document["plugins"]["bindings"]
            .as_array_mut()
            .ok_or("Plugin bindings must be an array")?;
        for binding in bindings {
            if !saved
                .iter()
                .any(|old| old["server"] == ID && old["camera_id"] == binding["camera_id"])
            {
                saved.push(binding);
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_config() -> serde_json::Value {
        let mut config = AppConfig::default();
        let mut camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://camera.invalid");
        camera.motion.sensitivity = 9;
        camera.motion.low_cpu = true;
        camera.recording.mode = nw_core::camera::RecordingMode::Events;
        config.cameras.push(camera);
        let mut document = serde_json::to_value(config).unwrap();
        document["cameras"][0]["motion"]["source"] = serde_json::json!("both");
        document
    }

    #[test]
    fn the_older_detectors_settings_become_a_binding_to_an_installed_motion() {
        let path = Path::new("/tmp/nw-migration/config.json");
        let mut document = legacy_config();
        assert!(migrate_document(&mut document).unwrap());
        assert!(!migrate_document(&mut document).unwrap());
        let mut config: AppConfig = serde_json::from_value(document).unwrap();
        assert!(migrate(&mut config, path));
        assert_eq!(
            config.cameras[0].motion.source,
            nw_core::camera::MotionSource::Onvif
        );
        assert_eq!(
            config.cameras[0].recording.mode,
            nw_core::camera::RecordingMode::Events
        );
        assert_eq!(config.plugins.bindings[0].config["sensitivity"], 9);
        assert_eq!(config.plugins.bindings[0].config["low_cpu"], true);
        let [server] = config.plugins.servers.as_slice() else {
            panic!("one Motion server: {:?}", config.plugins.servers);
        };
        assert_eq!(server.address, standalone_address());
        assert!(
            server.approval.is_none(),
            "an administrator approves its key"
        );
        assert!(!migrate(&mut config, path), "idempotent");
        assert_eq!(config.plugins.bindings.len(), 1);
    }

    #[test]
    fn a_bundled_era_server_moves_to_the_installed_motion_keeping_its_bindings() {
        let path = Path::new("/tmp/nw-migration/config.json");
        let mut config = AppConfig::default();
        config.plugins.servers.push(PluginServer {
            id: ID.into(),
            address: bundled_address(path),
            enabled: true,
            approval: Some(nw_analytics_protocol::auth::Approval {
                public_key: "00".repeat(32),
                permissions: vec!["video".into(), "events".into()],
            }),
            credential_revision: 0,
            plugin_id: Some(PLUGIN_ID.into()),
            managed: None,
        });
        config.plugins.bindings.push(
            serde_json::from_value(serde_json::json!({
                "server": ID, "camera_id": "gate", "enabled": true,
                "config": {"sensitivity": 7, "zones": []}
            }))
            .unwrap(),
        );
        assert!(migrate(&mut config, path));
        assert_eq!(config.plugins.servers[0].address, standalone_address());
        assert!(config.plugins.servers[0].approval.is_none());
        assert_eq!(config.plugins.bindings[0].config["sensitivity"], 7);
        assert!(!migrate(&mut config, path));
    }

    #[test]
    fn a_configuration_without_motion_gets_none_and_others_are_left_alone() {
        let path = Path::new("/tmp/nw-migration/config.json");
        let mut config = AppConfig::default();
        assert!(!migrate(&mut config, path));
        assert!(
            config.plugins.servers.is_empty(),
            "Motion is never added by itself"
        );
        // A Motion an administrator added elsewhere (another computer) stays.
        config.plugins.servers.push(PluginServer {
            id: ID.into(),
            address: "quic:motion.example.test:7700".into(),
            enabled: true,
            approval: None,
            credential_revision: 0,
            plugin_id: Some(PLUGIN_ID.into()),
            managed: None,
        });
        assert!(!migrate(&mut config, path));
        assert_eq!(
            config.plugins.servers[0].address,
            "quic:motion.example.test:7700"
        );
    }
}
