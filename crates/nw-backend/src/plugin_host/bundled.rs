//! The bundled detector is a separate child process. Only this process's child
//! is stopped; a user-supplied plugin daemon is never managed here.
use super::PluginServer;
use crate::{app_state::AppState, config::AppConfig};

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{broadcast, RwLock};

pub const ID: &str = "nw-motion-bundled";

pub fn address(config: &Path) -> String {
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

/// Preserve every legacy detector's settings and its ONVIF half. Never replace
/// an existing binding: an administrator may already have tuned the plugin.
pub fn migrate(config: &mut AppConfig, path: &Path) -> Result<bool, String> {
    let expected = address(path);
    if config.plugins_initialized && !config.plugins.servers.iter().any(|server| server.id == ID) {
        return Ok(false);
    }
    let mut changed = !config.plugins_initialized;
    config.plugins_initialized = true;
    if !config.plugins.servers.iter().any(|server| server.id == ID) {
        config.plugins.servers.push(PluginServer {
            id: ID.into(),
            address: expected,
            enabled: true,
            approval: None,
            credential_revision: 0,
            plugin_id: Some("nw.motion".into()),
            managed: None,
        });
        changed = true;
    }
    let key_path = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("plugins/nw-motion.key");
    let key =
        nw_analytics_sdk::secrets::signing_key(&key_path).map_err(|error| error.to_string())?;
    let approval = nw_analytics_protocol::auth::Approval {
        public_key: nw_analytics_sdk::identity::public_key(&key),
        permissions: vec![
            "video".into(),
            "events".into(),
            "overlays".into(),
            "decoded_frames".into(),
            "web_ui".into(),
        ],
    };
    if let Some(server) = config
        .plugins
        .servers
        .iter_mut()
        .find(|server| server.id == ID)
    {
        if server.plugin_id.as_deref() != Some("nw.motion") {
            server.plugin_id = Some("nw.motion".into());
            changed = true;
        }
        if server.address != address(path) {
            server.address = address(path);
            changed = true;
        }
        if server.approval.as_ref() != Some(&approval) {
            server.approval = Some(approval);
            changed = true;
        }
    }
    Ok(changed)
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

/// Development only: another `nw-motion` to run in place of the one beside
/// the backend (`scripts/dev.sh --motion`: the Neurowatch-Motion repository's build).
/// Release builds never read it.
const MOTION_BIN_ENV: &str = "NW_MOTION_BIN";

/// The program to run: the development override when one is given, else the
/// one shipped beside the backend.
fn chosen(given: Option<std::ffi::OsString>, beside: PathBuf) -> Result<PathBuf, String> {
    let (path, what) = match given.filter(|path| !path.is_empty()) {
        Some(path) => (PathBuf::from(path), MOTION_BIN_ENV),
        None => (beside, "The bundled motion daemon"),
    };
    if !path.is_file() {
        return Err(format!("{what} is missing: {}", path.display()));
    }
    Ok(path)
}

fn executable() -> Result<PathBuf, String> {
    let backend = std::env::current_exe().map_err(|error| error.to_string())?;
    let name = if cfg!(windows) {
        "nw-motion.exe"
    } else {
        "nw-motion"
    };
    let beside = backend
        .parent()
        .ok_or("Backend folder is unavailable")?
        .join(name);
    let given = if cfg!(debug_assertions) {
        std::env::var_os(MOTION_BIN_ENV)
    } else {
        None
    };
    chosen(given, beside)
}

pub async fn run(state: Arc<RwLock<AppState>>, mut shutdown: broadcast::Receiver<()>) {
    let mut child: Option<tokio::process::Child> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut retry = tokio::time::Instant::now();
    loop {
        tokio::select! { _ = shutdown.recv() => break, _ = tick.tick() => {} }
        let (wanted, identity) = {
            let state = state.read().await;
            let wanted = state
                .config
                .plugins
                .servers
                .iter()
                .find(|server| {
                    server.id == ID
                        && server.enabled
                        && server.address == address(&state.config_path)
                })
                .map(|server| server.address.clone());
            (
                wanted,
                state
                    .config_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("plugins/nw-motion.key"),
            )
        };
        if wanted.is_none() {
            if let Some(mut process) = child.take() {
                let _ = process.kill().await;
            }
            continue;
        }
        if let Some(process) = child.as_mut() {
            match process.try_wait() {
                Ok(None) => continue,
                result => {
                    tracing::warn!(?result, "Bundled motion daemon stopped; restarting");
                    child = None;
                    retry = tokio::time::Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if tokio::time::Instant::now() < retry {
            continue;
        }
        let started = executable().and_then(|executable| {
            let mut command = tokio::process::Command::new(executable);
            command
                .arg("--listen")
                .arg(wanted.unwrap_or_default())
                .arg("--parent-pid")
                .arg(std::process::id().to_string())
                .arg("--identity")
                .arg(identity)
                .kill_on_drop(true);
            #[cfg(windows)]
            command.creation_flags(0x0800_0000);
            command.spawn().map_err(|error| error.to_string())
        });
        match started {
            Ok(process) => {
                tracing::info!(pid = process.id(), "Bundled motion daemon started");
                child = Some(process);
            }
            Err(error) => {
                tracing::error!(%error, "Could not start bundled motion daemon");
                retry = tokio::time::Instant::now() + Duration::from_secs(30);
            }
        }
    }
    if let Some(mut process) = child {
        let _ = process.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_given_motion_program_replaces_the_one_beside_the_backend() {
        let folder = std::env::temp_dir().join(format!("nw-motion-bin-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let (beside, other) = (folder.join("nw-motion"), folder.join("other"));
        std::fs::write(&beside, b"").unwrap();
        std::fs::write(&other, b"").unwrap();
        assert_eq!(chosen(None, beside.clone()).unwrap(), beside);
        assert_eq!(chosen(Some("".into()), beside.clone()).unwrap(), beside);
        assert_eq!(
            chosen(Some(other.clone().into()), beside.clone()).unwrap(),
            other
        );
        let missing = chosen(Some(folder.join("absent").into()), beside).unwrap_err();
        assert!(missing.starts_with(MOTION_BIN_ENV), "{missing}");
        assert!(chosen(None, folder.join("absent")).is_err());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn migration_keeps_events_only_detection_settings_and_camera_events_and_is_idempotent() {
        let path = Path::new("/tmp/nw-migration/config.json");
        let mut config = AppConfig::default();
        let mut camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://camera.invalid");
        camera.motion.sensitivity = 9;
        camera.motion.low_cpu = true;
        camera.recording.mode = nw_core::camera::RecordingMode::Events;
        config.cameras.push(camera);
        let mut document = serde_json::to_value(config).unwrap();
        document["cameras"][0]["motion"]["source"] = serde_json::json!("both");
        assert!(migrate_document(&mut document).unwrap());
        assert!(!migrate_document(&mut document).unwrap());
        let mut config: AppConfig = serde_json::from_value(document).unwrap();
        assert!(migrate(&mut config, path).unwrap());
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
        assert!(!migrate(&mut config, path).unwrap());
        assert_eq!(config.plugins.bindings.len(), 1);
    }
}
