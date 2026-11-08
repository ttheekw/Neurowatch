//! IPC handler for `plugins.*` ([`crate::plugin_host`]). Admin only (the
//! accounts gate requires admin for methods it does not list), except the
//! two reads a live view needs: `plugins.cameras` and `plugins.overlays`.
//!
//! Methods:
//!   plugins.get    — {} → {servers, bindings}
//!   plugins.set    — {servers, bindings} → the saved settings; validated
//!                    (addresses, ids, bound cameras exist); applied within a second
//!   plugins.status — {} → {plugins: [ServerStatus]}: connection, manifest,
//!                    and per bound camera the node's and the plugin's state
//!   plugins.cameras — {} → {cameras: {camera_id: [CameraAnalytics]}}: which
//!                    plugins analyse each camera, their drawing layers (viewer)
//!   plugins.overlays — {after?, timeout_ms? (≤ 10000), camera_ids?} →
//!                    {seq, overlays: [Shown]}: the drawings shown now; with
//!                    `after`, waits until something changed after it (viewer)

use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::plugin_host::PluginsConfig;

/// Longest `plugins.overlays` wait.
const MAX_WAIT_MS: u64 = 10_000;

pub struct PluginsHandler {
    state: Arc<RwLock<AppState>>,
}

impl PluginsHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self { state }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "plugins.discover" => {
                let (path, servers, statuses) = {
                    let state = self.state.read().await;
                    (
                        state.config_path.clone(),
                        state.config.plugins.servers.clone(),
                        state.plugins.status(&state.config.plugins.servers),
                    )
                };
                Ok(
                    json!({"plugins":crate::plugin_host::discovery::discover(&path,&servers,&statuses).await}),
                )
            }
            "plugins.inspect" => {
                let address = params["address"]
                    .as_str()
                    .filter(|address| address.len() <= 2048)
                    .ok_or_else(|| RpcError::invalid_params("Plugin address is required"))?;
                let manifest = crate::plugin_host::discovery::inspect(address.into())
                    .await
                    .map_err(RpcError::invalid_params)?;
                let state = self.state.read().await;
                let added_as = crate::plugin_host::discovery::existing(
                    address,
                    Some(&manifest),
                    &state.config.plugins.servers,
                    &state.plugins.status(&state.config.plugins.servers),
                );
                Ok(
                    json!({"address":crate::plugin_host::discovery::address_key(address).map_err(RpcError::invalid_params)?,"manifest":manifest,"added_as":added_as}),
                )
            }
            "plugins.add" => self.add(params).await,
            "plugins.pages" | "plugins.web.request" | "plugins.web.read" => {
                let viewer = match params.get("_nw_web_viewer") {
                    Some(viewer) => serde_json::from_value(viewer.clone())
                        .map_err(|_| RpcError::invalid_params("Invalid viewer context"))?,
                    None if !self.state.read().await.config.accounts.enabled => {
                        crate::plugin_host::web::Viewer::owner()
                    }
                    _ => {
                        return Err(RpcError::invalid_params(
                            "A signed-in viewer context is required",
                        ))
                    }
                };
                crate::plugin_host::web::dispatch(&self.state, method, params, &viewer)
                    .await
                    .map_err(RpcError::invalid_params)
            }
            "plugins.get" => {
                let plugins = self.state.read().await.config.plugins.clone();
                serde_json::to_value(plugins).map_err(|e| RpcError::internal(e.to_string()))
            }
            "plugins.set" => self.set(params).await,
            "plugins.approve" => self.approve(params).await,
            "plugins.credentials" => self.credentials(params).await,
            "plugins.cameras" => {
                let st = self.state.read().await;
                Ok(json!({ "cameras": st.plugins.cameras(&st.config.plugins) }))
            }
            "plugins.overlays" => self.overlays(params).await,
            "plugins.history" => self.history(params).await,
            "plugins.status" => {
                let (servers, host) = {
                    let st = self.state.read().await;
                    (st.config.plugins.servers.clone(), Arc::clone(&st.plugins))
                };
                Ok(json!({ "plugins": host.status(&servers) }))
            }
            _ => Err(RpcError::method_not_found()),
        }
    }

    async fn approve(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["server"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("'server' is required"))?;
        let key = params["public_key"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("'public_key' is required"))?;
        let mut state = self.state.write().await;
        let shown = state
            .plugins
            .status(&state.config.plugins.servers)
            .into_iter()
            .find(|server| server.id == id)
            .and_then(|server| server.manifest)
            .ok_or_else(|| RpcError::invalid_params("Wait for the plugin's signed manifest"))?;
        let identity = shown
            .identity
            .ok_or_else(|| RpcError::invalid_params("The plugin did not prove its identity"))?;
        if identity.public_key != key {
            return Err(RpcError::invalid_params(
                "The plugin key changed; review it again",
            ));
        }
        let server = state
            .config
            .plugins
            .servers
            .iter_mut()
            .find(|server| server.id == id)
            .ok_or_else(|| RpcError::invalid_params("Plugin not found"))?;
        let previous = server.approval.clone();
        server.approval = Some(nw_analytics_protocol::auth::Approval {
            public_key: key.into(),
            permissions: shown.permissions,
        });
        if let Err(error) = state.save_config() {
            if let Some(server) = state
                .config
                .plugins
                .servers
                .iter_mut()
                .find(|server| server.id == id)
            {
                server.approval = previous;
            }
            return Err(RpcError::internal(error.to_string()));
        }
        if let Some(previous) = previous.filter(|approval| approval.public_key != key) {
            crate::plugin_host::credentials::forget(&state.config_path, id, &previous.public_key)
                .map_err(RpcError::internal)?;
        }
        Ok(json!({"ok":true}))
    }

    async fn credentials(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["server"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("'server' is required"))?;
        let credential = params["credential"]
            .as_str()
            .filter(|value| value.len() <= 8192)
            .ok_or_else(|| {
                RpcError::invalid_params(
                    "A credential is at most 8192 bytes; use an empty string to clear it",
                )
            })?;
        let mut state = self.state.write().await;
        let server = state
            .config
            .plugins
            .servers
            .iter()
            .find(|server| server.id == id)
            .ok_or_else(|| RpcError::invalid_params("Plugin not found"))?;
        let approval = server.approval.as_ref().ok_or_else(|| {
            RpcError::invalid_params("Approve the plugin's key before supplying a credential")
        })?;
        let path =
            crate::plugin_host::credentials::path(&state.config_path, id, &approval.public_key)
                .map_err(RpcError::invalid_params)?;
        if credential.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(RpcError::internal(error.to_string())),
            }
        } else {
            nw_analytics_sdk::secrets::write(
                &path,
                crate::plugin_host::credentials::PURPOSE,
                credential.as_bytes(),
                true,
            )
            .map_err(|error| RpcError::internal(error.to_string()))?;
        }
        if let Some(server) = state
            .config
            .plugins
            .servers
            .iter_mut()
            .find(|server| server.id == id)
        {
            server.credential_revision = server.credential_revision.wrapping_add(1);
        }
        state
            .save_config()
            .map_err(|error| RpcError::internal(error.to_string()))?;
        Ok(json!({"ok":true,"credential_set":!credential.is_empty()}))
    }

    async fn history(&self, params: &Value) -> Result<Value, RpcError> {
        let camera = params
            .get("camera_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 64)
            .ok_or_else(|| RpcError::invalid_params("'camera_id' is required"))?
            .to_owned();
        let from = params
            .get("from_us")
            .and_then(Value::as_i64)
            .ok_or_else(|| RpcError::invalid_params("'from_us' is required"))?;
        let to = params
            .get("to_us")
            .and_then(Value::as_i64)
            .ok_or_else(|| RpcError::invalid_params("'to_us' is required"))?;
        if from > to {
            return Err(RpcError::invalid_params(
                "Drawing history range is reversed",
            ));
        }
        let after = match (
            params.get("after_us").and_then(Value::as_i64),
            params.get("after_id").and_then(Value::as_i64),
        ) {
            (None, None) => None,
            (Some(time), Some(id)) => Some((time, id)),
            _ => {
                return Err(RpcError::invalid_params(
                    "'after_us' and 'after_id' go together",
                ))
            }
        };
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(128)
            .clamp(1, 128) as usize;
        let history = self
            .state
            .read()
            .await
            .plugins
            .overlays
            .history
            .clone()
            .ok_or_else(|| RpcError::internal("Drawing history is unavailable"))?;
        let rows =
            tokio::task::spawn_blocking(move || history.list(&camera, from, to, after, limit))
                .await
                .map_err(|e| RpcError::internal(e.to_string()))?
                .map_err(RpcError::internal)?;
        // Leave room for the sealed reply envelope and base64 expansion. A
        // pathological layer is trimmed just as it is for the live data channel.
        let mut drawings = Vec::new();
        let mut bytes = 0;
        let mut trimmed = false;
        let count = rows.len();
        for mut row in rows {
            let mut size = serde_json::to_vec(&row)
                .map_err(|e| RpcError::internal(e.to_string()))?
                .len();
            if !drawings.is_empty() && bytes + size > 96 * 1024 {
                break;
            }
            while size > 96 * 1024 && !row.items.is_empty() {
                row.items.truncate(row.items.len() / 2);
                trimmed = true;
                size = serde_json::to_vec(&row)
                    .map_err(|e| RpcError::internal(e.to_string()))?
                    .len();
            }
            bytes += size;
            drawings.push(row);
        }
        let more = drawings.len() < count || count == limit;
        let next = drawings
            .last()
            .filter(|_| more)
            .map(|row| json!({"after_us":row.time_us,"after_id":row.id}));
        Ok(json!({"drawings":drawings,"next":next,"trimmed":trimmed}))
    }

    async fn overlays(&self, params: &Value) -> Result<Value, RpcError> {
        let cameras: Option<Vec<String>> = match params.get("camera_ids") {
            None | Some(Value::Null) => None,
            Some(ids) => Some(
                serde_json::from_value(ids.clone())
                    .map_err(|_| RpcError::invalid_params("'camera_ids' must be a list of ids"))?,
            ),
        };
        let store = Arc::clone(&self.state.read().await.plugins.overlays);
        if let Some(after) = params.get("after").and_then(Value::as_u64) {
            let timeout = params
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(MAX_WAIT_MS);
            // A long poll: waiting for drawings is not the recorder being slow
            // (a stopped plugin sends none, so every poll waits its timeout).
            let waiting = std::time::Instant::now();
            store
                .wait(after, std::time::Duration::from_millis(timeout))
                .await;
            crate::stats::ipc_metrics::record_idle_wait(waiting.elapsed());
        }
        let seq = store.seq();
        // The figures too: a count changing is a change, shown as soon as drawings are.
        Ok(json!({
            "seq": seq,
            "overlays": store.snapshot(cameras.as_deref()),
            "figures": store.figures_by_camera(cameras.as_deref()),
        }))
    }

    async fn set(&self, params: &Value) -> Result<Value, RpcError> {
        let mut next: PluginsConfig = serde_json::from_value(json!({
            "servers": params.get("servers").cloned().unwrap_or(json!([])),
            "bindings": params.get("bindings").cloned().unwrap_or(json!([])),
        }))
        .map_err(|e| RpcError::invalid_params(format!("Invalid plugin settings: {e}")))?;
        let mut st = self.state.write().await;
        for server in &mut next.servers {
            if let Some(current) = st
                .config
                .plugins
                .servers
                .iter()
                .find(|current| current.id == server.id && current.address == server.address)
            {
                server.plugin_id = current.plugin_id.clone().or_else(|| {
                    st.plugins
                        .status(&st.config.plugins.servers)
                        .into_iter()
                        .find(|status| status.id == server.id)
                        .and_then(|status| status.manifest.map(|manifest| manifest.id))
                });
            }
        }
        next.validate(st.config.cameras.iter().map(|camera| camera.id.as_str()))
            .map_err(RpcError::invalid_params)?;
        let previous = std::mem::replace(&mut st.config.plugins, next);
        if let Err(error) = st.save_config() {
            st.config.plugins = previous;
            return Err(RpcError::internal(error.to_string()));
        }
        for server in previous.servers {
            if let Some(approval) = server.approval {
                let retained = st.config.plugins.servers.iter().any(|current| {
                    current.id == server.id
                        && current
                            .approval
                            .as_ref()
                            .is_some_and(|current| current.public_key == approval.public_key)
                });
                if !retained {
                    crate::plugin_host::credentials::forget(
                        &st.config_path,
                        &server.id,
                        &approval.public_key,
                    )
                    .map_err(RpcError::internal)?;
                }
            }
        }
        tracing::info!(
            plugins = st.config.plugins.servers.len(),
            bindings = st.config.plugins.bindings.len(),
            "Analytics plugin settings changed"
        );
        serde_json::to_value(&st.config.plugins).map_err(|e| RpcError::internal(e.to_string()))
    }

    async fn add(&self, params: &Value) -> Result<Value, RpcError> {
        let address = params["address"]
            .as_str()
            .filter(|address| address.len() <= 2048)
            .ok_or_else(|| RpcError::invalid_params("Plugin address is required"))?;
        {
            let state = self.state.read().await;
            if let Some(id) = crate::plugin_host::discovery::existing(
                address,
                None,
                &state.config.plugins.servers,
                &[],
            ) {
                return Err(RpcError::invalid_params(format!(
                    "This plugin has already been added as {id:?}"
                )));
            }
        }
        let manifest = crate::plugin_host::discovery::inspect(address.into())
            .await
            .map_err(RpcError::invalid_params)?;
        let mut state = self.state.write().await;
        let statuses = state.plugins.status(&state.config.plugins.servers);
        if let Some(id) = crate::plugin_host::discovery::existing(
            address,
            Some(&manifest),
            &state.config.plugins.servers,
            &statuses,
        ) {
            return Err(RpcError::invalid_params(format!(
                "This plugin has already been added as {id:?}"
            )));
        }
        let server = crate::plugin_host::PluginServer {
            id: params["id"].as_str().unwrap_or(&manifest.id).trim().into(),
            address: crate::plugin_host::discovery::address_key(address)
                .map_err(RpcError::invalid_params)?,
            enabled: true,
            approval: None,
            credential_revision: 0,
            plugin_id: Some(manifest.id),
        };
        let mut next = state.config.plugins.clone();
        next.servers.push(server.clone());
        next.validate(state.config.cameras.iter().map(|camera| camera.id.as_str()))
            .map_err(RpcError::invalid_params)?;
        let previous = std::mem::replace(&mut state.config.plugins, next);
        if let Err(error) = state.save_config() {
            state.config.plugins = previous;
            return Err(RpcError::internal(error.to_string()));
        }
        Ok(json!(server))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;
    use nw_core::camera::CameraConfig;

    fn handler() -> (PluginsHandler, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("nw-plugins-{}.json", uuid::Uuid::new_v4()));
        let mut config = AppConfig::default();
        let mut camera = CameraConfig::new("Door", "rtsp://127.0.0.1:1/door");
        camera.id = "cam-1".into();
        config.cameras.push(camera);
        let state = Arc::new(RwLock::new(AppState::with_database(
            config,
            path.clone(),
            HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )));
        (PluginsHandler::new(state), path)
    }

    #[tokio::test]
    async fn settings_are_validated_saved_and_reported() {
        let (handler, path) = handler();
        let settings = json!({
            "servers": [{"id": "motion", "address": "unix:/tmp/nw-motion-test.sock"}],
            "bindings": [{"server": "motion", "camera_id": "cam-1", "config": {"sensitivity": 7}}]
        });
        let saved = handler.dispatch("plugins.set", &settings).await.unwrap();
        assert_eq!(saved["bindings"][0]["config"]["sensitivity"], 7);
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("nw-motion-test.sock"));
        assert_eq!(
            handler.dispatch("plugins.get", &json!({})).await.unwrap(),
            saved
        );
        let status = handler
            .dispatch("plugins.status", &json!({}))
            .await
            .unwrap();
        assert_eq!(status["plugins"][0]["id"], "motion");
        assert_eq!(status["plugins"][0]["state"], "connecting");
        let cameras = handler
            .dispatch("plugins.cameras", &json!({}))
            .await
            .unwrap();
        assert_eq!(cameras["cameras"]["cam-1"][0]["plugin"], "motion");
        assert_eq!(cameras["cameras"]["cam-1"][0]["state"], "offline");

        let unknown_camera = json!({
            "servers": [{"id": "motion", "address": "unix:/tmp/x.sock"}],
            "bindings": [{"server": "motion", "camera_id": "nope"}]
        });
        assert!(handler
            .dispatch("plugins.set", &unknown_camera)
            .await
            .is_err());
        assert_eq!(
            handler.dispatch("plugins.get", &json!({})).await.unwrap(),
            saved
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn overlays_are_read_at_once_or_after_a_change() {
        use nw_analytics_protocol::messages::{OverlayItem, Style};
        let (handler, path) = handler();
        let store = Arc::clone(&handler.state.read().await.plugins.overlays);
        let item = OverlayItem::Box {
            rect: [0.1, 0.1, 0.2, 0.2],
            label: Some("Motion".into()),
            style: Style::Accent,
            fill: false,
        };
        store.put(
            "cam-1",
            "motion",
            "detections",
            vec![item.clone()],
            Some(3),
            None,
        );
        store.put(
            "cam-2",
            "motion",
            "detections",
            vec![item.clone()],
            None,
            None,
        );
        let now = handler
            .dispatch("plugins.overlays", &json!({"camera_ids": ["cam-1"]}))
            .await
            .unwrap();
        assert_eq!(now["seq"], 2);
        assert_eq!(now["overlays"].as_array().unwrap().len(), 1);
        assert_eq!(now["overlays"][0]["items"][0]["type"], "box");
        assert_eq!(now["overlays"][0]["frame"], 3);

        let writer = Arc::clone(&store);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            writer.put("cam-1", "motion", "detections", vec![], None, None);
        });
        let started = std::time::Instant::now();
        let changed = handler
            .dispatch("plugins.overlays", &json!({"after": 2, "timeout_ms": 5000}))
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(changed["seq"], 3);
        assert_eq!(changed["overlays"][0]["camera_id"], "cam-2");

        // A count changing wakes the poll too, and the reply carries it.
        let writer = Arc::clone(&store);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            writer.put_figures(
                "cam-1",
                "vcount",
                vec![nw_analytics_protocol::messages::Figure {
                    label: "Cars".into(),
                    value: "7".into(),
                    icon: None,
                    style: None,
                }],
            );
        });
        let started = std::time::Instant::now();
        let counted = handler
            .dispatch(
                "plugins.overlays",
                &json!({"after": 3, "timeout_ms": 5000, "camera_ids": ["cam-1"]}),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(counted["figures"]["cam-1"][0]["value"], "7");
        assert_eq!(counted["figures"]["cam-1"][0]["plugin"], "vcount");
        assert!(handler
            .dispatch("plugins.overlays", &json!({"camera_ids": "cam-1"}))
            .await
            .is_err());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn waiting_for_drawings_is_not_counted_as_a_slow_request() {
        // A stopped plugin sends no drawings, so each long poll waits its whole
        // timeout: that must not read as the recorder answering slowly.
        let (handler, path) = handler();
        let ((answer, _), idle) = crate::stats::ipc_metrics::measure_handler(async {
            let started = std::time::Instant::now();
            let answer = handler
                .dispatch("plugins.overlays", &json!({"after": 0, "timeout_ms": 200}))
                .await;
            (answer, started.elapsed())
        })
        .await;
        assert!(answer.is_ok());
        assert!(
            idle >= std::time::Duration::from_millis(180),
            "idle {idle:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn history_is_paged_bounded_and_rejects_ambiguous_ranges() {
        let (handler, path) = handler();
        let store = Arc::clone(&handler.state.read().await.plugins.overlays);
        store.put_identified("cam-1", "m", "boxes", vec![], None, None);
        let history = store.history.as_ref().unwrap();
        for time in [100, 200, 300] {
            assert!(history.offer(crate::plugin_host::history::DrawingRecord {
                id: 0,
                camera_id: "cam-1".into(),
                plugin: "m".into(),
                layer: "boxes".into(),
                frame: Some(time as u64),
                time_us: time,
                ttl_ms: 1000,
                on_wall: true,
                items: vec![],
            }));
        }
        assert!(history.flush());
        let first = handler
            .dispatch(
                "plugins.history",
                &json!({"camera_id":"cam-1","from_us":0,"to_us":300,"limit":2}),
            )
            .await
            .unwrap();
        assert_eq!(first["drawings"].as_array().unwrap().len(), 2);
        assert_eq!(first["next"]["after_us"], 200);
        let next = handler.dispatch("plugins.history",&json!({"camera_id":"cam-1","from_us":0,"to_us":300,"after_us":200,"after_id":first["next"]["after_id"]})).await.unwrap();
        assert_eq!(next["drawings"].as_array().unwrap().len(), 1);
        assert!(next["next"].is_null());
        for bad in [
            json!({}),
            json!({"camera_id":"cam-1","from_us":300,"to_us":100}),
            json!({"camera_id":"cam-1","from_us":0,"to_us":300,"after_id":1}),
        ] {
            assert!(handler.dispatch("plugins.history", &bad).await.is_err());
        }
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(all(test, unix))]
mod discovery_tests {
    use super::*;
    use std::{
        os::unix::net::UnixListener,
        time::{Duration, Instant},
    };
    fn daemon(listener: UnixListener, calls: usize, key: u8) -> std::thread::JoinHandle<()> {
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            use nw_analytics_protocol::{
                frame::{read_frame, write_frame, Frame},
                messages::Manifest,
                rpc::Message,
            };
            for _ in 0..calls {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let Frame::Message(message) = read_frame(&mut socket).unwrap() else {
                    panic!("hello expected")
                };
                let Message::Request { id, method, params } = Message::from_value(message).unwrap()
                else {
                    panic!("hello expected")
                };
                assert_eq!(method, "hello");
                assert!(params.get("locality").is_none());
                let mut manifest:Manifest=serde_json::from_value(json!({"id":"test.discovery","name":"Discovery fixture","vendor":"Test","version":"1","protocol":1,"media":{"video":{"stream":"main","codecs":["h264"]}}})).unwrap();
                nw_analytics_sdk::identity::sign_manifest(
                    &mut manifest,
                    &nw_analytics_sdk::identity::SigningKey::from_bytes(&[key; 32]),
                    params["challenge"].as_str().unwrap(),
                    "",
                )
                .unwrap();
                write_frame(
                    &mut socket,
                    &Frame::Message(
                        Message::reply(id, Ok::<_, nw_analytics_protocol::rpc::RpcError>(manifest))
                            .to_value(),
                    ),
                )
                .unwrap();
                assert_eq!(
                    read_frame(&mut socket).unwrap_err().kind(),
                    std::io::ErrorKind::UnexpectedEof,
                    "discovery must not authorize or attach cameras"
                );
            }
        })
    }
    #[tokio::test]
    async fn local_discovery_and_atomic_add_reject_a_second_instance_of_the_plugin() {
        gstreamer::init().unwrap();
        let dir = std::env::temp_dir().join(format!("nw-discovery-{}", uuid::Uuid::new_v4()));
        let folder = dir.join("plugins");
        std::fs::create_dir_all(&folder).unwrap();
        let first = format!("unix:{}", folder.join("p.sock").display());
        let server = daemon(UnixListener::bind(folder.join("p.sock")).unwrap(), 2, 1);
        let config = crate::config::AppConfig {
            plugins_initialized: true,
            ..Default::default()
        };
        let state = Arc::new(RwLock::new(crate::app_state::AppState::new(
            config,
            dir.join("config.json"),
            crate::pipeline::hwaccel::HwAccelKind::Software,
        )));
        let handler = PluginsHandler::new(Arc::clone(&state));
        let found = handler
            .dispatch("plugins.discover", &json!({}))
            .await
            .unwrap();
        assert!(found["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["address"] == first && item["manifest"]["id"] == "test.discovery"));
        let added = handler
            .dispatch("plugins.add", &json!({"id":"first","address":first}))
            .await
            .unwrap();
        assert_eq!(added["plugin_id"], "test.discovery");
        assert!(added["approval"].is_null());
        server.join().unwrap();
        let second = format!("unix:{}", folder.join("q.sock").display());
        let server = daemon(UnixListener::bind(folder.join("q.sock")).unwrap(), 1, 2);
        let before = std::fs::read(dir.join("config.json")).unwrap();
        let refused = handler
            .dispatch(
                "plugins.add",
                &json!({"id":"another-name","address":second}),
            )
            .await
            .unwrap_err();
        assert!(refused.message.contains("already been added"));
        assert_eq!(state.read().await.config.plugins.servers.len(), 1);
        assert_eq!(
            std::fs::read(dir.join("config.json")).unwrap(),
            before,
            "duplicate adds must not change saved config"
        );
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
