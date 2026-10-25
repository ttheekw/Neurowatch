//! JSON-RPC method dispatch.

use std::sync::Arc;

use nw_core::camera::{CameraConfig, WallPosition};
use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::cloud::cloud_handler::CloudHandler;
use crate::discovery::{self, DiscoveryService};
use crate::ipc::events_handler::EventsHandler;
use crate::ipc::export_handler::ExportHandler;
use crate::ipc::recording_handler::RecordingHandler;
use crate::ipc::storage_handler::StorageHandler;
use crate::ipc::viewer_handler::ViewerHandler;
use crate::pipeline;
use crate::playback::handler::PlaybackHandler;
use crate::stats::PerformanceAggregator;
use crate::viewer::ViewerManager;

/// Desktop view leases: default, shortest and longest lifetimes.
const VIEW_LEASE_DEFAULT_MS: u64 = 10_000;
const VIEW_LEASE_MIN_MS: u64 = 2_000;
const VIEW_LEASE_MAX_MS: u64 = 60_000;

fn view_client_id(params: &Value) -> Result<String, RpcError> {
    params["client_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params("Missing 'client_id'"))
}

pub struct RpcHandler {
    state: Arc<RwLock<AppState>>,
    recording: RecordingHandler,
    storage: StorageHandler,
    playback: PlaybackHandler,
    export: ExportHandler,
    alerts: super::alerts_handler::AlertsHandler,
    accounts: super::accounts_handler::AccountsHandler,
    events: EventsHandler,
    viewer: ViewerHandler,
    viewer_manager: Arc<ViewerManager>,
    stats: Arc<PerformanceAggregator>,
    /// `cloud.*` methods (see [`crate::cloud`]); None when the cloud node
    /// could not start, and in the web portal's own handler.
    cloud: Option<Arc<CloudHandler>>,
    discovery: Arc<DiscoveryService>,
    /// Stops the whole backend (`system.shutdown`); None in tests.
    shutdown: Option<tokio::sync::broadcast::Sender<()>>,
    started: std::time::Instant,
}

/// JSON-RPC error codes of the accounts gate.
pub const SIGN_IN_REQUIRED: i64 = -32001;
pub const NOT_ALLOWED: i64 = -32003;

/// How long `system.shutdown` waits before stopping, so its answer is sent.
const SHUTDOWN_REPLY_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

/// Who can reach a request's connection. Cloud methods (`cloud.*`) are
/// served only to `Protected` callers: the Unix socket (owner-only, peer uid
/// checked), or TCP with this user's token (`ipc::token`; Windows uses TCP).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcTransport {
    Protected,
    Tcp,
}

impl RpcHandler {
    pub fn new(
        state: Arc<RwLock<AppState>>,
        viewer_manager: Arc<ViewerManager>,
        stats: Arc<PerformanceAggregator>,
    ) -> Self {
        let recording = RecordingHandler::new(Arc::clone(&state));
        let storage = StorageHandler::new(Arc::clone(&state));
        let playback = PlaybackHandler::new(Arc::clone(&state));
        let export = ExportHandler::new(Arc::clone(&state));
        let alerts = super::alerts_handler::AlertsHandler::new(Arc::clone(&state));
        let accounts = super::accounts_handler::AccountsHandler::new(Arc::clone(&state));
        let events = EventsHandler::new(Arc::clone(&state));
        let viewer = ViewerHandler::new(Arc::clone(&state), Arc::clone(&viewer_manager));
        Self {
            state,
            recording,
            storage,
            playback,
            export,
            alerts,
            accounts,
            events,
            viewer,
            viewer_manager,
            stats,
            cloud: None,
            discovery: Arc::new(DiscoveryService::new()),
            shutdown: None,
            started: std::time::Instant::now(),
        }
    }

    /// Let `system.shutdown` stop the backend through its shutdown channel
    /// (the same path as SIGTERM / Ctrl+C).
    pub fn with_shutdown(mut self, shutdown: tokio::sync::broadcast::Sender<()>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// Share one discovery service (its search cache and one-search limit)
    /// between the local IPC handler and the web portal's handler.
    pub fn with_discovery(mut self, discovery: Arc<DiscoveryService>) -> Self {
        self.discovery = discovery;
        self
    }

    /// Serve the `cloud.*` methods (local socket only).
    pub fn with_cloud(mut self, cloud: Arc<CloudHandler>) -> Self {
        self.cloud = Some(cloud);
        self
    }

    pub async fn dispatch(&self, req: &Value) -> Value {
        self.dispatch_from(req, IpcTransport::Protected).await
    }

    pub async fn dispatch_from(&self, req: &Value, transport: IpcTransport) -> Value {
        let method = req["method"].as_str().unwrap_or("");
        let params = &req["params"];
        let id = &req["id"];

        tracing::debug!(method = %method, "IPC command received");

        let result = match method {
            "system.capabilities" => self.system_capabilities().await,
            "system.status" => self.system_status().await,
            "system.shutdown" => self.system_shutdown(),
            "camera.list" => self.camera_list().await,
            "camera.add" => self.camera_add(params).await,
            "camera.remove" => self.camera_remove(params).await,
            "camera.update" => self.camera_update(params).await,
            "camera.status_all" => self.camera_status_all().await,
            "camera.codec_all" => self.camera_codec_all().await,
            "camera.stream_info_all" => self.camera_stream_info_all().await,
            "camera.test" => self.camera_test(params).await,
            "camera.set_quality" => self.camera_set_quality(params).await,
            "camera.set_stream" => self.camera_set_stream(params).await,
            "view.update" => self.view_update(params).await,
            "view.release" => self.view_release(params).await,
            "config.get" => self.config_get().await,
            "config.set" => self.config_set(params).await,
            "config.save" => self.config_save().await,
            "camera.set_positions" => self.camera_set_positions(params).await,
            "stats.get_performance" => self.stats_get_performance().await,
            m if m.starts_with("camera.discovery.") => self.discovery_dispatch(m, params).await,
            m if m.starts_with("ptz.") => {
                async {
                    let id = params["camera_id"]
                        .as_str()
                        .ok_or_else(|| RpcError::invalid_params("Missing camera_id"))?;
                    let (camera, ptz, hub) = {
                        let st = self.state.read().await;
                        let camera = st
                            .config
                            .cameras
                            .iter()
                            .find(|cam| cam.id == id)
                            .cloned()
                            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;
                        (camera, Arc::clone(&st.ptz), Arc::clone(&st.events))
                    };
                    ptz.execute(&camera, hub, m, params)
                        .await
                        .map_err(|error| RpcError::invalid_params(error.to_string()))
                }
                .await
            }
            m if m.starts_with("recording.") => self.recording.dispatch(m, params).await,
            m if m.starts_with("storage.") => self.storage.dispatch(m, params).await,
            m if m.starts_with("playback.") => self.playback.dispatch(m, params).await,
            m if m.starts_with("export.") => self.export.dispatch(m, params).await,
            m if m.starts_with("alerts.") => self.alerts.dispatch(m, params).await,
            m if m.starts_with("talk.") => self.talk_dispatch(m, params).await,
            m if m.starts_with("auth.") || m.starts_with("accounts.") => {
                self.accounts.dispatch(m, params).await
            }
            m if m.starts_with("viewer.") => self.viewer.dispatch(m, params).await,
            m if m.starts_with("motion.") || m.starts_with("events.") => {
                self.events.dispatch(m, params).await
            }
            "camera.onvif.detect" | "snapshot.capture" => {
                self.events.dispatch(method, params).await
            }
            // Cloud methods are for local callers on the protected socket only.
            m if m.starts_with("cloud.") => match &self.cloud {
                Some(cloud) if transport == IpcTransport::Protected => {
                    cloud.dispatch(m, params).await
                }
                _ => Err(RpcError::method_not_found()),
            },
            _ => Err(RpcError::method_not_found()),
        };

        match result {
            Ok(val) => json!({"jsonrpc":"2.0","id":id,"result":val}),
            Err(ref e) => {
                tracing::warn!(method = %method, code = e.code, error = %e.message, "IPC error response sent");
                let mut error = json!({"code": e.code, "message": e.message});
                if let Some(data) = &e.data {
                    error["data"] = data.clone();
                }
                json!({"jsonrpc":"2.0","id":id,"error":error})
            }
        }
    }

    /// A request from the IPC server: with local accounts on, only for a
    /// signed-in user whose role and cameras allow it, with other cameras
    /// left out of what it returns (see [`crate::accounts::access`]).
    pub async fn dispatch_gated(&self, mut req: Value, transport: IpcTransport) -> Value {
        use crate::accounts::access;
        let method = req["method"].as_str().unwrap_or("").to_owned();
        let id = req["id"].clone();
        let refuse = |code: i64, message: String| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}});
        let session = {
            let st = self.state.read().await;
            if !st.config.accounts.enabled || access::open(&method) {
                None
            } else {
                match req["session"]
                    .as_str()
                    .and_then(|token| st.sessions.session(&st.config.accounts, token))
                {
                    Some(session) => Some(session),
                    None => return refuse(SIGN_IN_REQUIRED, "Sign in to use NeuroWatch".into()),
                }
            }
        };
        let Some(session) = session else {
            return self.dispatch_from(&req, transport).await;
        };
        if session.role < access::required_role(&method) {
            tracing::info!(username = %session.username, method = %method, "Refused: role");
            return refuse(
                NOT_ALLOWED,
                format!("Your account ({}) may not do this", session.role.label()),
            );
        }
        if let Err(message) = access::check_cameras(&session, &method, &mut req["params"]) {
            return refuse(NOT_ALLOWED, message);
        }
        if let Err(error) = self
            .events
            .authorize_ids(&session, &method, &req["params"])
            .await
        {
            return refuse(i64::from(error.code), error.message);
        }
        if let Err(error) = self
            .export
            .authorize_job(&session, &method, &req["params"])
            .await
        {
            return refuse(i64::from(error.code), error.message);
        }
        let mut response = self.dispatch_from(&req, transport).await;
        if let Some(result) = response.get_mut("result") {
            match access::filter_result(&session, &method, result.take()) {
                Ok(filtered) => *result = filtered,
                Err(message) => return refuse(NOT_ALLOWED, message),
            }
        }
        response
    }

    /// Two-way audio (`crate::talk`):
    ///   talk.start  — {camera_id, client_id, source?: "microphone" | "tone"} → {codec}
    ///   talk.renew  — {client_id} → {} (within `talk::LEASE` while the button is held)
    ///   talk.stop   — {client_id} → {}
    ///   talk.status — {} → {camera_id, client_id} or null
    ///   talk.probe  — {camera_id} → {codec} when the camera has a usable speaker
    async fn talk_dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let talk = Arc::clone(&self.state.read().await.talk);
        let client = || {
            params["client_id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 64)
                .ok_or_else(|| RpcError::invalid_params("Missing 'client_id'"))
        };
        match method {
            "talk.start" => {
                let client_id = client()?;
                let source = match params["source"].as_str().unwrap_or("microphone") {
                    "microphone" => crate::talk::TalkSource::Microphone,
                    "tone" => crate::talk::TalkSource::Tone,
                    _ => return Err(RpcError::invalid_params("'source' is microphone or tone")),
                };
                let (camera, headless) = {
                    let st = self.state.read().await;
                    let id = params["camera_id"]
                        .as_str()
                        .ok_or_else(|| RpcError::invalid_params("Missing 'camera_id'"))?;
                    let camera = st
                        .config
                        .cameras
                        .iter()
                        .find(|c| c.id == id)
                        .cloned()
                        .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;
                    (camera, st.headless())
                };
                if headless && matches!(source, crate::talk::TalkSource::Microphone) {
                    return Err(RpcError::invalid_params(
                        "this recorder runs without a desktop, so it has no microphone",
                    ));
                }
                let codec = talk
                    .start(&camera, client_id, source)
                    .await
                    .map_err(RpcError::invalid_params)?;
                Ok(json!({ "codec": codec }))
            }
            "talk.probe" => {
                let camera = {
                    let st = self.state.read().await;
                    let id = params["camera_id"]
                        .as_str()
                        .ok_or_else(|| RpcError::invalid_params("Missing 'camera_id'"))?;
                    st.config
                        .cameras
                        .iter()
                        .find(|c| c.id == id)
                        .cloned()
                        .ok_or_else(|| RpcError::invalid_params("Camera not found"))?
                };
                let codec = crate::talk::TalkService::probe(&camera)
                    .await
                    .map_err(RpcError::invalid_params)?;
                Ok(json!({ "codec": codec }))
            }
            "talk.renew" => {
                talk.renew(client()?).map_err(RpcError::invalid_params)?;
                Ok(json!({}))
            }
            "talk.stop" => {
                talk.stop(client()?);
                Ok(json!({}))
            }
            "talk.status" => Ok(match talk.status() {
                Some((camera_id, client_id)) => {
                    json!({"camera_id": camera_id, "client_id": client_id})
                }
                None => Value::Null,
            }),
            _ => Err(RpcError::method_not_found()),
        }
    }

    async fn system_capabilities(&self) -> Result<Value, RpcError> {
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "features": ["v1", "v2"],
        }))
    }

    async fn system_status(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        Ok(json!({
            "status": "running",
            "cameras": st.config.cameras.len(),
            "site_name": st.config.site_name,
            "video_acceleration": acceleration_json(st.hwaccel),
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_sec": self.started.elapsed().as_secs(),
            "headless": st.headless(),
            "edition": crate::edition::EDITION,
            "camera_limit": crate::edition::CAMERA_LIMIT,
            "cameras_over_limit": st
                .config
                .cameras_over_limit
                .iter()
                .map(|c| json!({ "id": c.id, "name": c.name }))
                .collect::<Vec<_>>(),
            "downloads_url": crate::edition::DOWNLOADS_URL,
        }))
    }

    /// Stop the backend gracefully, as SIGTERM would: the service manager
    /// (tray, `neurowatch service stop`) uses it on every platform. Local
    /// callers only (Unix socket or loopback TCP), like every other method.
    fn system_shutdown(&self) -> Result<Value, RpcError> {
        let shutdown = self
            .shutdown
            .clone()
            .ok_or_else(|| RpcError::internal("Shutdown is not available"))?;
        tracing::info!("Shutdown requested over IPC");
        tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_REPLY_GRACE).await;
            let _ = shutdown.send(());
        });
        Ok(json!({ "stopping": true }))
    }

    async fn camera_list(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        serde_json::to_value(&st.config.cameras).map_err(|e| RpcError::internal(e.to_string()))
    }

    async fn camera_add(&self, params: &Value) -> Result<Value, RpcError> {
        if let Some(motion) = params.get("motion") {
            crate::motion::validate_settings(motion).map_err(RpcError::invalid_params)?;
        }
        let cam: CameraConfig = serde_json::from_value(params.clone())
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        crate::recording::policy::validate_settings(&cam.recording)
            .map_err(RpcError::invalid_params)?;
        self.add_camera(cam).await
    }

    /// The one path that adds a camera, for `camera.add` and
    /// `camera.discovery.add`: id and duplicate checks happen
    /// under the config write lock, before any side effect.
    async fn add_camera(&self, mut cam: CameraConfig) -> Result<Value, RpcError> {
        cam.normalize_stream_ids();
        let mut st = self.state.write().await;
        if st
            .config
            .cameras
            .iter()
            .chain(&st.config.cameras_over_limit)
            .any(|c| c.id == cam.id)
        {
            return Err(RpcError::invalid_params("Camera ID already exists"));
        }
        if let Some(limit) = crate::edition::CAMERA_LIMIT {
            if st.config.cameras.len() >= limit {
                tracing::info!(
                    limit,
                    "Camera add refused: camera limit of this free version"
                );
                return Err(crate::edition::limit_reached(limit));
            }
        }
        if let Some(existing) = discovery::matchers::duplicate_of(&st.config.cameras, &cam) {
            tracing::info!(cam_id = %existing.id, "Camera add rejected: same stream already added");
            return Err(RpcError::invalid_params(format!(
                "This camera is already added as “{}”.",
                existing.name
            )));
        }
        tracing::info!(cam_id = %cam.id, name = %cam.name, "Camera added");
        st.spawn_supervisor(&cam);
        st.config.cameras.push(cam.clone());
        st.save_config()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        serde_json::to_value(&cam).map_err(|e| RpcError::internal(e.to_string()))
    }

    async fn camera_remove(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?
            .to_string();
        let mut st = self.state.write().await;
        if let Some(held) = st.config.cameras_over_limit.iter().position(|c| c.id == id) {
            // Over the camera limit: never started, so nothing to stop.
            tracing::info!(cam_id = %id, "Camera removed (over the camera limit)");
            st.config.cameras_over_limit.remove(held);
            st.save_config()
                .map_err(|e| RpcError::internal(e.to_string()))?;
            return Ok(json!({ "removed": true }));
        }
        let idx = st
            .config
            .cameras
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;
        tracing::info!(cam_id = %id, "Camera removed");
        st.stop_supervisor(&id);
        st.clear_live_stream(&id);
        st.stream_health.forget(&id);
        st.config.cameras.remove(idx);
        // A camera held over the limit takes the freed place.
        for camera in crate::edition::apply(&mut st.config) {
            tracing::info!(cam_id = %camera.id, "Camera runs again: within the camera limit");
            st.spawn_supervisor(&camera);
        }
        st.save_config()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({ "removed": true }))
    }

    async fn camera_status_all(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        let map: serde_json::Map<String, Value> = st
            .supervisor_states()
            .into_iter()
            .map(|(id, s)| (id, Value::String(s)))
            .collect();
        Ok(Value::Object(map))
    }

    async fn camera_codec_all(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        let map: serde_json::Map<String, Value> = st
            .supervisor_codecs()
            .into_iter()
            .map(|(id, c)| (id, Value::String(c)))
            .collect();
        Ok(Value::Object(map))
    }

    /// Codec and decoded resolution per camera; `width`/`height` are null
    /// until negotiated.
    async fn camera_stream_info_all(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        let map: serde_json::Map<String, Value> = st
            .supervisor_stream_infos()
            .into_iter()
            .map(|(id, info)| {
                let (width, height) = info.resolution.unzip();
                (
                    id,
                    json!({
                        "codec": info.codec,
                        "width": width,
                        "height": height,
                        "audio": info.audio,
                    }),
                )
            })
            .collect();
        Ok(Value::Object(map))
    }

    async fn camera_test(&self, params: &Value) -> Result<Value, RpcError> {
        let url = params["url"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'url'"))?;

        // Inject optional separately-supplied credentials into the URL so that
        // users who fill in the username/password fields get a real connection test.
        // Only a desktop dialog that shows the picture asks for `preview`; any
        // other test reads the stream's format without decoding.
        self.test_stream(
            url,
            params["username"].as_str(),
            params["password_b64"].as_str(),
            params["preview"].as_bool().unwrap_or(false),
        )
        .await
    }

    /// Connect to a stream once and report its format; with `preview`, also a
    /// decoded picture for a desktop dialog to show.
    async fn test_stream(
        &self,
        url: &str,
        username: Option<&str>,
        password_b64: Option<&str>,
        preview: bool,
    ) -> Result<Value, RpcError> {
        let effective_url =
            crate::pipeline::builder::url_with_credentials_str(url, username, password_b64);

        let st = self.state.read().await;
        let hwaccel = st.hwaccel;
        drop(st);

        if preview {
            if let Some(reason) = pipeline::hwaccel::live_view_block_reason() {
                return Ok(json!({
                    "ok": false,
                    "error": reason,
                }));
            }
        }
        let probe = if preview {
            pipeline::probe_url(&effective_url, hwaccel).await
        } else {
            pipeline::probe_encoded(&effective_url).await
        };
        match probe {
            Ok(res) => {
                tracing::info!(
                    width = res.width,
                    height = res.height,
                    fps = res.fps,
                    "Camera test succeeded"
                );
                Ok(json!({
                    "ok": true,
                    "width": res.width,
                    "height": res.height,
                    "fps": res.fps,
                    "codec": res.codec,
                    "thumbnail_b64": res.thumbnail_b64,
                }))
            }
            Err(ref e) => {
                let error = pipeline::sanitize_credentials(&e.to_string());
                tracing::warn!(error = %error, "Camera test failed");
                Ok(json!({
                    "ok": false,
                    "error": error,
                }))
            }
        }
    }

    async fn camera_update(&self, params: &Value) -> Result<Value, RpcError> {
        if let Some(motion) = params.get("motion") {
            crate::motion::validate_settings(motion).map_err(RpcError::invalid_params)?;
        }
        let mut cam: CameraConfig = serde_json::from_value(params.clone())
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        cam.normalize_stream_ids();
        let mut st = self.state.write().await;
        let idx = st
            .config
            .cameras
            .iter()
            .position(|c| c.id == cam.id)
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;
        // Settings a client does not edit here (`motion.configure`, the ONVIF
        // service found later) stay unless the request carries them.
        if params.get("motion").is_none() {
            cam.motion = st.config.cameras[idx].motion.clone();
        }
        let old = &st.config.cameras[idx];
        // The wall layout (`camera.set_positions`) places cameras; editing one
        // never moves it (the camera dialog sends none, which put the edited
        // camera last on the wall).
        if params.get("wall_position").map_or(true, Value::is_null) {
            cam.wall_position = old.wall_position.clone();
        }
        // Old clients must not erase modes/rules/padding they do not know.
        let mut recording =
            serde_json::to_value(&old.recording).map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(changes) = params.get("recording").and_then(Value::as_object) {
            for (key, value) in changes {
                if key == "local" && value.is_object() {
                    for (key, value) in value.as_object().into_iter().flatten() {
                        recording["local"][key] = value.clone();
                    }
                } else {
                    recording[key] = value.clone();
                }
            }
        }
        cam.recording = serde_json::from_value(recording)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        crate::recording::policy::validate_settings(&cam.recording)
            .map_err(RpcError::invalid_params)?;
        let same_host =
            crate::onvif_events::url_host(&old.url) == crate::onvif_events::url_host(&cam.url);
        if params.get("onvif").is_none() && same_host {
            cam.onvif = old.onvif.clone();
        }
        let mut pipeline_config = cam.clone();
        pipeline_config.recording.mode = old.recording.mode;
        pipeline_config
            .recording
            .weekly_rules
            .clone_from(&old.recording.weekly_rules);
        pipeline_config.recording.event_pre_sec = old.recording.event_pre_sec;
        pipeline_config.recording.event_post_sec = old.recording.event_post_sec;
        pipeline_config.recording.non_event_keep_hours = old.recording.non_event_keep_hours;
        // Motion settings are picked up by motion detection's own check every
        // second, and the wall position is the UI's: neither needs the
        // camera's connection and recording restarted.
        pipeline_config.motion = old.motion.clone();
        pipeline_config.wall_position = old.wall_position.clone();
        if pipeline_config == *old {
            // A policy-only edit needs the scheduler, not a new connection.
            // Persist first so a failed Save leaves memory and ingest alone.
            let mut next = st.config.clone();
            next.cameras[idx] = cam.clone();
            next.save(&st.config_path)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            st.config = next;
            st.reconcile_recording_policies();
            return serde_json::to_value(&cam).map_err(|e| RpcError::internal(e.to_string()));
        }
        tracing::info!(cam_id = %cam.id, name = %cam.name, "Camera updated");
        st.stop_supervisor(&cam.id);
        st.clear_live_stream(&cam.id);
        st.config.cameras[idx] = cam.clone();
        st.spawn_supervisor(&cam);
        st.save_config()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        serde_json::to_value(&cam).map_err(|e| RpcError::internal(e.to_string()))
    }

    /// Legacy main/sub switch; `camera.set_stream` also selects listed streams.
    async fn camera_set_quality(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?;
        let selector = if params["sub"].as_bool().unwrap_or(false) {
            nw_core::camera::SUB_STREAM
        } else {
            nw_core::camera::MAIN_STREAM
        };

        let mut st = self.state.write().await;
        st.set_live_stream(id, selector)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({ "ok": true }))
    }

    /// Live-view stream for one camera: `{"id", "stream"}` where `stream` is
    /// `"main"`, `"sub"` or a stream id from the camera's `streams`.
    async fn camera_set_stream(&self, params: &Value) -> Result<Value, RpcError> {
        let id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?;
        let stream = params["stream"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'stream'"))?;

        let mut st = self.state.write().await;
        let known = st
            .config
            .cameras
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?
            .stream_url(stream)
            .is_some();
        if !known {
            return Err(RpcError::invalid_params("Unknown stream"));
        }
        st.set_live_stream(id, stream)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({ "ok": true }))
    }

    /// A desktop UI's view lease: `{"client_id", "camera_ids", "ttl_ms"?,
    /// "audio_camera_id"?}`. Exactly these cameras are decoded for it until
    /// the lease lapses (default 10 s, renewed by calling again); `[]` shows
    /// none. `audio_camera_id` (or null / absent: none) is the camera to hear
    /// live on this computer's audio output, for as long as the lease.
    async fn view_update(&self, params: &Value) -> Result<Value, RpcError> {
        let client_id = view_client_id(params)?;
        let cameras = params["camera_ids"]
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("Missing 'camera_ids'"))?
            .iter()
            .map(|id| id.as_str().map(str::to_owned))
            .collect::<Option<std::collections::HashSet<_>>>()
            .ok_or_else(|| RpcError::invalid_params("'camera_ids' must be strings"))?;
        let ttl = std::time::Duration::from_millis(
            params["ttl_ms"]
                .as_u64()
                .unwrap_or(VIEW_LEASE_DEFAULT_MS)
                .clamp(VIEW_LEASE_MIN_MS, VIEW_LEASE_MAX_MS),
        );
        let listen = match &params["audio_camera_id"] {
            Value::Null => None,
            Value::String(id) if !id.is_empty() => Some(id.clone()),
            _ => {
                return Err(RpcError::invalid_params(
                    "'audio_camera_id' must be a string",
                ))
            }
        };
        let mut st = self.state.write().await;
        st.set_view_demand(&client_id, cameras, ttl);
        st.set_listen(&client_id, listen, ttl);
        Ok(json!({ "decoding": st.active_views(), "listening": st.listening() }))
    }

    /// The desktop UI shows no cameras any more: release its lease and any
    /// native sessions it attached.
    async fn view_release(&self, params: &Value) -> Result<Value, RpcError> {
        let client_id = view_client_id(params)?;
        self.state.write().await.release_view_demand(&client_id);
        let detached = self.viewer_manager.detach_owner(&client_id);
        Ok(json!({ "released": true, "native_sessions": detached }))
    }

    async fn config_get(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        let cfg = &st.config;
        Ok(json!({
            "site_name": cfg.site_name,
            "wall": {
                "rows": cfg.wall.rows,
                "cols": cfg.wall.cols,
                "auto_fit": cfg.wall.auto_fit,
                "page_interval_sec": cfg.wall.page_interval_sec,
            },
            "ui": {
                "theme": cfg.ui.theme,
                "fullscreen": cfg.ui.fullscreen,
                "fullscreen_status": cfg.ui.fullscreen_status,
                "fullscreen_status_opacity": cfg.ui.fullscreen_status_opacity,
                "preview_fps": cfg.ui.preview_fps,
            },
            "recording": {
                "storage_path": cfg.recording.storage_path,
                "segment_minutes": cfg.recording.segment_minutes,
                "pre_buffer_sec": cfg.recording.pre_buffer_sec,
                "local_all_cameras": cfg.recording.local_all_cameras,
                "disk_threshold_percent": cfg.recording.disk_threshold_percent,
                "archive": cfg.recording.archive,
            },
            "debug": {
                "gst_log": cfg.debug.gst_log,
            },
            "video": {
                "decoder": cfg.video.decoder.id(),
            },
            "events": {
                "retention_days": cfg.events.retention_days,
                "push_port": cfg.events.push_port,
            },
            "desktop": cfg.desktop,
        }))
    }

    async fn config_set(&self, params: &Value) -> Result<Value, RpcError> {
        let restart = {
            let mut st = self.state.write().await;
            let before = st.config.video.decoder;
            st.update_config_settings(params)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            crate::pipeline::hwaccel::needs_restart(before, st.config.video.decoder)
        };
        tracing::debug!("Config saved via config.set");
        if !restart {
            return Ok(json!({ "ok": true }));
        }
        // The decoder choice changes GStreamer's process-wide registry, which
        // cannot be undone: stop gracefully and start again in place, after
        // this answer is sent. Without a shutdown channel (the web portal's
        // handler), it applies at the next start.
        let Some(shutdown) = self.shutdown.clone() else {
            return Ok(json!({ "ok": true, "restart_required": true }));
        };
        tracing::info!("Video decoder choice changed; restarting the backend to apply it");
        crate::runtime::request_restart();
        tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_REPLY_GRACE).await;
            let _ = shutdown.send(());
        });
        Ok(json!({ "ok": true, "restarting": true }))
    }

    /// Force an atomic re-save of the current in-memory config to disk.
    /// Auto-save already runs on every mutation; this exposes an explicit
    /// "flush" the UI can invoke (e.g. Ctrl+S) without changing any values.
    async fn config_save(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        st.save_config()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({ "ok": true }))
    }

    async fn stats_get_performance(&self) -> Result<Value, RpcError> {
        let mut snap = self.stats.snapshot();
        let (hwaccel, views, motion) = {
            let st = self.state.read().await;
            (
                st.hwaccel,
                st.active_views(),
                Arc::clone(&st.builtin_motion),
            )
        };
        let motion = motion.metrics();
        snap.bottleneck = crate::stats::bottleneck::with_motion(snap.bottleneck, &motion);
        let mut value =
            serde_json::to_value(snap).map_err(|e| RpcError::internal(e.to_string()))?;
        value["video_acceleration"] = acceleration_json(hwaccel);
        // Additive runtime details, like desktop_decoders below. Keep the
        // shared snapshot's existing Rust construction contract compatible
        // with external extensions as well as older JSON consumers.
        value["motion_decoders"] = json!(motion.iter().filter(|m| m.decoder_running).count());
        value["motion"] = json!(motion);
        // Video decoders the node runs right now: one per camera a desktop
        // view shows, and one per native overlay session. Opt-in analysis is
        // counted independently in motion_decoders; snapshots are one-shot.
        value["desktop_decoders"] = json!(views + self.viewer_manager.session_count());
        Ok(value)
    }

    async fn camera_set_positions(&self, params: &Value) -> Result<Value, RpcError> {
        let entries = params
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("Expected array"))?;
        let mut st = self.state.write().await;
        for entry in entries {
            let id = entry["id"]
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("Missing 'id' in entry"))?;
            if let Some(cam) = st.config.cameras.iter_mut().find(|c| c.id == id) {
                let wp = &entry["wall_position"];
                if wp.is_object() {
                    let field = |key: &str, default: u32| {
                        wp[key]
                            .as_u64()
                            .and_then(|v| u32::try_from(v).ok())
                            .unwrap_or(default)
                    };
                    let span = |key: &str| field(key, 1).clamp(1, WallPosition::MAX_SPAN);
                    cam.wall_position = Some(WallPosition {
                        page: field("page", 0),
                        row: field("row", 0),
                        col: field("col", 0),
                        row_span: span("row_span"),
                        col_span: span("col_span"),
                    });
                }
            }
        }
        st.save_config()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({ "ok": true }))
    }
}

/// `camera.discovery.*`: search the network for cameras and add one.
///
/// `_portal: true` marks a request from the web portal (set by
/// `remote_config`): searches stay inside private networks and stream URLs
/// are left out of answers.
impl RpcHandler {
    async fn discovery_dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let from_portal = params["_portal"].as_bool().unwrap_or(false);
        let text = |key: &str| {
            params[key]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let required = |key: &str| {
            text(key).ok_or_else(|| RpcError::invalid_params(format!("Missing '{key}'")))
        };
        let failed = |e: discovery::DiscoveryError| RpcError::invalid_params(e.0);
        match method {
            "camera.discovery.interfaces" => Ok(json!({
                "protocol_version": nw_core::camera_discovery::DISCOVERY_PROTOCOL_VERSION,
                "interfaces": discovery::net::interfaces(),
            })),
            "camera.discovery.start" => {
                let scope = match text("scope").unwrap_or("current") {
                    "current" => discovery::Scope::Current,
                    "interface" => discovery::Scope::Interface(required("interface")?.to_owned()),
                    "cidr" => discovery::Scope::Cidr(required("cidr")?.to_owned()),
                    _ => {
                        return Err(RpcError::invalid_params(
                            "scope must be current, interface or cidr",
                        ))
                    }
                };
                let started = self.discovery.start(scope, from_portal).map_err(failed)?;
                to_value(&started)
            }
            "camera.discovery.results" => {
                let cameras = self.state.read().await.config.cameras.clone();
                let results = self
                    .discovery
                    .results(required("scan_id")?, &cameras)
                    .map_err(failed)?;
                to_value(&results)
            }
            "camera.discovery.cancel" => {
                self.discovery
                    .cancel(required("scan_id")?)
                    .map_err(failed)?;
                Ok(json!({ "cancelled": true }))
            }
            "camera.discovery.lookup" => {
                let port = match params.get("port") {
                    None | Some(Value::Null) => None,
                    Some(port) => Some(
                        port.as_u64()
                            .and_then(|p| u16::try_from(p).ok())
                            .filter(|p| *p > 0)
                            .ok_or_else(|| RpcError::invalid_params("port must be 1 to 65535"))?,
                    ),
                };
                let cameras = self.state.read().await.config.cameras.clone();
                let candidate = self
                    .discovery
                    .lookup(required("address")?, port, from_portal, &cameras)
                    .await
                    .map_err(failed)?;
                Ok(json!({ "candidate": candidate }))
            }
            "camera.discovery.profiles" => {
                let credentials = text("username").map(|username| discovery::onvif::Credentials {
                    username: username.to_owned(),
                    password: decode_password(params["password_b64"].as_str()),
                });
                let cameras = self.state.read().await.config.cameras.clone();
                let (candidate, streams, main, sub) = self
                    .discovery
                    .profiles(
                        required("candidate_id")?,
                        credentials,
                        !from_portal,
                        &cameras,
                    )
                    .await
                    .map_err(failed)?;
                Ok(json!({
                    "candidate": candidate,
                    "streams": streams,
                    "main_stream_id": main,
                    "sub_stream_id": sub,
                }))
            }
            "camera.discovery.test" => {
                let (url, username, password_b64) = self
                    .discovery
                    .stream_for_test(required("candidate_id")?, required("stream_id")?)
                    .map_err(failed)?;
                // Reachable from the web portal: never decodes.
                self.test_stream(&url, username.as_deref(), password_b64.as_deref(), false)
                    .await
            }
            "camera.discovery.add" => {
                let candidate_id = required("candidate_id")?;
                let mut camera = self
                    .discovery
                    .build_camera(
                        candidate_id,
                        required("main_stream_id")?,
                        text("sub_stream_id"),
                        text("name"),
                    )
                    .map_err(failed)?;
                camera.recording.local.enabled = params["record"].as_bool().unwrap_or(false);
                let added = self.add_camera(camera).await?;
                self.discovery.forget_credentials(candidate_id);
                Ok(added)
            }
            _ => Err(RpcError::method_not_found()),
        }
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(e.to_string()))
}

fn decode_password(password_b64: Option<&str>) -> String {
    use base64::Engine as _;
    password_b64
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

pub fn acceleration_json(kind: crate::pipeline::hwaccel::HwAccelKind) -> Value {
    let mut value = if let Some(reason) = crate::pipeline::hwaccel::live_view_block_reason() {
        json!({
            "kind": "blocked",
            "label": "Live view blocked — update GStreamer",
            "hardware": false,
            "available": false,
            "reason": reason,
        })
    } else {
        let mut value = json!({
            "kind": kind.wire_kind(),
            "label": kind.to_string(),
            "hardware": kind.is_hardware(),
            "available": true,
        });
        if !kind.is_hardware() && crate::pipeline::hwaccel::waiting_for_gpu_access() {
            value["reason"] = json!(crate::pipeline::hwaccel::GPU_NOT_ACCESSIBLE);
        }
        value
    };
    add_decoder_choice(&mut value);
    value
}

/// The decoder choice (`video.decoder`) this backend applied when it started
/// and the hardware decoders Settings can offer (additive fields).
fn add_decoder_choice(value: &mut Value) {
    use crate::pipeline::hwaccel;
    value["decoder_preference"] = json!(hwaccel::applied_preference().unwrap_or_default().id());
    value["available_decoders"] = hwaccel::available_decoders()
        .iter()
        .map(|family| json!({ "id": family.preference().id(), "label": family.label() }))
        .collect();
    if let Some(warning) = hwaccel::preference_warning() {
        value["decoder_preference_warning"] = json!(warning);
    }
}

#[cfg(test)]
mod tests {
    include!("access_tests.rs");
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{json, Value};
    use tokio::sync::RwLock;

    use crate::app_state::AppState;
    use crate::config::AppConfig;
    use crate::ipc::handler::{IpcTransport, RpcHandler};
    use crate::pipeline::hwaccel::HwAccelKind;
    use crate::stats::test_support::{StaticCameras, StaticGpu, StaticIpc, StaticSystem};
    use crate::stats::{
        CameraHealthSource, GpuMetricsSource, IpcMetricsSource, PerformanceAggregator,
        SystemMetricsSource,
    };
    use crate::viewer::capabilities::ViewerCapabilities;
    use crate::viewer::ViewerManager;
    use nw_core::stats::{
        Bottleneck, BottleneckKind, GpuStatus, IpcMetrics, PerCameraHealth, ProcessMetrics,
        Severity,
    };
    use std::sync::Mutex;

    fn test_viewer_manager() -> std::sync::Arc<ViewerManager> {
        std::sync::Arc::new(ViewerManager::new(
            HwAccelKind::Software,
            ViewerCapabilities::unavailable("no_overlay_sink"),
        ))
    }

    fn test_aggregator() -> std::sync::Arc<PerformanceAggregator> {
        let sys: Arc<dyn SystemMetricsSource> = Arc::new(StaticSystem(ProcessMetrics {
            cpu_percent: 5.0,
            rss_bytes: 1024,
            memory_percent: 2.0,
            system_cpu_percent: Some(10.0),
            system_memory_percent: Some(20.0),
            network: None,
            siblings: vec![],
        }));
        let gpu: Arc<dyn GpuMetricsSource> = Arc::new(StaticGpu(GpuStatus::unavailable("test")));
        let ipc: Arc<dyn IpcMetricsSource> = Arc::new(StaticIpc(IpcMetrics::default()));
        let cams: Arc<dyn CameraHealthSource> = Arc::new(StaticCameras(Mutex::new(vec![])));
        Arc::new(PerformanceAggregator::new(sys, gpu, ipc, cams))
    }

    fn _unused_lint_silencers() {
        // Reference bottleneck types so unused imports lint stays quiet even
        // when handler test coverage grows without touching each variant.
        let _ = (
            Bottleneck::ok(""),
            BottleneckKind::None,
            PerCameraHealth {
                camera_id: "".into(),
                ingest_fps: None,
                renderer_fps: None,
                decoder_fps: 0.0,
                renderer_drops: 0,
                decoder_drops: 0,
                end_to_end_latency_ms: None,
                state: "".into(),
            },
            Severity::Ok,
        );
    }

    fn test_config_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("neurowatch-handler-{name}-{nanos}.json"))
    }

    async fn dispatch(handler: &RpcHandler, method: &str, params: Value) -> Value {
        handler
            .dispatch(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }))
            .await
    }

    #[tokio::test]
    async fn config_save_flushes_current_state_to_disk() {
        let _ = gstreamer::init();
        let cfg_path = test_config_path("save-flush");
        // Guarantee a clean slate — no file at the target path.
        let _ = std::fs::remove_file(&cfg_path);
        let state = Arc::new(RwLock::new(AppState::new(
            AppConfig::default(),
            cfg_path.clone(),
            HwAccelKind::Software,
        )));
        {
            let mut st = state.write().await;
            st.config.site_name = "forced-flush-site".to_owned();
        }
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        let response = dispatch(&handler, "config.save", json!({})).await;
        state.write().await.stop_all();

        assert!(
            response.get("error").is_none(),
            "config.save must be a real handler, got {response}"
        );
        assert_eq!(response["result"], json!({ "ok": true }));
        let on_disk = std::fs::read_to_string(&cfg_path).expect("config file written");
        assert!(
            on_disk.contains("forced-flush-site"),
            "in-memory config should be persisted, got {on_disk}"
        );
        let _ = std::fs::remove_file(&cfg_path);
    }

    #[tokio::test]
    async fn config_save_returns_error_when_target_path_is_unwritable() {
        let _ = gstreamer::init();
        // Make the "config file" a directory so the atomic-save rename fails.
        let cfg_path = test_config_path("save-unwritable");
        std::fs::create_dir_all(&cfg_path).expect("create dir stand-in for config path");
        let state = Arc::new(RwLock::new(AppState::new(
            AppConfig::default(),
            cfg_path.clone(),
            HwAccelKind::Software,
        )));
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        let response = dispatch(&handler, "config.save", json!({})).await;
        state.write().await.stop_all();
        let _ = std::fs::remove_dir_all(&cfg_path);

        let err = response
            .get("error")
            .expect("unwritable path must surface an IPC error");
        assert_eq!(err["code"], json!(-32603));
    }

    #[tokio::test]
    async fn stats_get_performance_returns_wire_shape() {
        let _ = gstreamer::init();
        let cfg_path = test_config_path("stats-shape");
        let _ = std::fs::remove_file(&cfg_path);
        let state = Arc::new(RwLock::new(AppState::new(
            AppConfig::default(),
            cfg_path.clone(),
            HwAccelKind::Software,
        )));
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        let response = dispatch(&handler, "stats.get_performance", json!({})).await;
        state.write().await.stop_all();
        let _ = std::fs::remove_file(&cfg_path);

        assert!(
            response.get("error").is_none(),
            "stats.get_performance must be a real handler, got {response}"
        );
        let result = &response["result"];
        for key in [
            "captured_at_utc",
            "process",
            "gpu",
            "ipc",
            "cameras",
            "bottleneck",
        ] {
            assert!(
                result.get(key).is_some(),
                "response missing '{key}': {response}"
            );
        }
        assert_eq!(result["gpu"]["status"], json!("unavailable"));
        assert!(result["gpu"]["reason"].is_string());
        assert_eq!(result["video_acceleration"]["kind"], json!("software"));
        assert_eq!(result["video_acceleration"]["hardware"], json!(false));
        assert_eq!(result["bottleneck"]["severity"], json!("ok"));
        assert_eq!(result["motion_decoders"], json!(0));
        assert_eq!(result["motion"], json!([]));
    }

    #[tokio::test]
    async fn system_status_reports_selected_video_acceleration() {
        let _ = gstreamer::init();
        let cfg_path = test_config_path("status-acceleration");
        let state = Arc::new(RwLock::new(AppState::new(
            AppConfig::default(),
            cfg_path.clone(),
            HwAccelKind::VaApi,
        )));
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        let response = dispatch(&handler, "system.status", json!({})).await;
        state.write().await.stop_all();
        let _ = std::fs::remove_file(&cfg_path);

        assert_eq!(response["result"]["video_acceleration"]["kind"], "vaapi");
        assert_eq!(response["result"]["video_acceleration"]["label"], "VA-API");
        assert_eq!(response["result"]["video_acceleration"]["hardware"], true);
        // The decoder choice: never probed in unit tests, so automatic.
        assert_eq!(
            response["result"]["video_acceleration"]["decoder_preference"],
            "auto"
        );
        assert!(response["result"]["video_acceleration"]["available_decoders"].is_array());
        assert_eq!(response["result"]["pid"], std::process::id());
        assert!(response["result"]["version"].is_string());
        assert!(response["result"]["uptime_sec"].is_u64());
        assert_eq!(response["result"]["headless"], false);
    }

    #[tokio::test]
    async fn system_shutdown_stops_the_backend_after_answering() {
        let _ = gstreamer::init();
        let cfg_path = test_config_path("system-shutdown");
        let state = Arc::new(RwLock::new(AppState::new(
            AppConfig::default(),
            cfg_path.clone(),
            HwAccelKind::Software,
        )));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);
        let unwired = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());
        let refused = dispatch(&unwired, "system.shutdown", json!({})).await;
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator())
            .with_shutdown(shutdown_tx);

        let response = dispatch(&handler, "system.shutdown", json!({})).await;
        assert_eq!(response["result"]["stopping"], true);
        assert!(
            shutdown_rx.try_recv().is_err(),
            "the answer goes out before the backend stops"
        );
        let stopped =
            tokio::time::timeout(std::time::Duration::from_secs(2), shutdown_rx.recv()).await;
        state.write().await.stop_all();
        let _ = std::fs::remove_file(&cfg_path);

        assert!(stopped.is_ok(), "the shutdown channel fired");
        assert!(refused.get("error").is_some(), "no channel, no shutdown");
    }

    #[tokio::test]
    async fn stats_response_never_leaks_urls_or_credentials() {
        let _ = gstreamer::init();
        let cfg_path = test_config_path("stats-safety");
        let _ = std::fs::remove_file(&cfg_path);
        let mut cfg = AppConfig::default();
        // Insert a camera whose URL / credentials must NEVER appear in stats.
        let mut cam = nw_core::camera::CameraConfig::new(
            "test-cam",
            "rtsp://secret:super@example.invalid/stream",
        );
        cam.id = "cam-leak-check".into();
        cfg.cameras.push(cam);
        let state = Arc::new(RwLock::new(AppState::new(
            cfg,
            cfg_path.clone(),
            HwAccelKind::Software,
        )));
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());
        let response = dispatch(&handler, "stats.get_performance", json!({})).await;
        state.write().await.stop_all();
        let _ = std::fs::remove_file(&cfg_path);
        let serialized = response.to_string();
        for forbidden in ["rtsp://", "example.invalid", "secret", "super"] {
            assert!(
                !serialized.contains(forbidden),
                "stats response leaked '{forbidden}': {serialized}"
            );
        }
    }

    /// Build a handler over a temp config path. Supervisors are never spawned
    /// by construction, so no RTSP/GStreamer pipelines start.
    fn handler_with_config(
        name: &str,
        cfg: AppConfig,
    ) -> (Arc<RwLock<AppState>>, RpcHandler, std::path::PathBuf) {
        let _ = gstreamer::init();
        let cfg_path = test_config_path(name);
        let _ = std::fs::remove_file(&cfg_path);
        let state = Arc::new(RwLock::new(AppState::new(
            cfg,
            cfg_path.clone(),
            HwAccelKind::Software,
        )));
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());
        (state, handler, cfg_path)
    }

    async fn teardown(state: &Arc<RwLock<AppState>>, cfg_path: &std::path::Path) {
        state.write().await.stop_all();
        let _ = std::fs::remove_file(cfg_path);
    }

    #[tokio::test]
    async fn with_accounts_on_requests_need_a_session_a_role_and_their_cameras() {
        use super::{NOT_ALLOWED, SIGN_IN_REQUIRED};
        use crate::accounts::{AccountsConfig, LocalUser, PasswordHash, Role};
        let mut cfg = config_with_camera("cam-1", "rtsp://127.0.0.1:1/a");
        let mut other = nw_core::camera::CameraConfig::new("cam-2", "rtsp://127.0.0.1:1/b");
        other.id = "cam-2".into();
        cfg.cameras.push(other);
        let user = |name: &str, role: Role, cameras: Vec<String>| LocalUser {
            username: name.into(),
            role,
            cameras,
            password: PasswordHash::new("password-1").unwrap(),
        };
        cfg.accounts = AccountsConfig {
            enabled: true,
            users: vec![
                user("root", Role::Admin, vec![]),
                user("guard", Role::Viewer, vec!["cam-1".into()]),
            ],
        };
        let (state, handler, cfg_path) = handler_with_config("accounts-gate", cfg);
        let call = |method: &str, params: Value, session: Option<&str>| {
            let mut request =
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
            if let Some(session) = session {
                request["session"] = json!(session);
            }
            handler.dispatch_gated(request, IpcTransport::Protected)
        };
        let anonymous = call("camera.list", json!({}), None).await;
        assert_eq!(anonymous["error"]["code"], SIGN_IN_REQUIRED);
        assert_eq!(
            call("system.status", json!({}), None).await["result"]["status"],
            "running"
        );
        let wrong = call(
            "auth.login",
            json!({"username": "guard", "password": "nope-nope"}),
            None,
        )
        .await;
        assert!(wrong["error"]["message"]
            .as_str()
            .unwrap()
            .contains("wrong"));
        let login = call(
            "auth.login",
            json!({"username": "guard", "password": "password-1"}),
            None,
        )
        .await;
        let guard = login["result"]["token"].as_str().unwrap().to_owned();
        assert_eq!(login["result"]["user"]["role"], "viewer");
        let cameras = call("camera.list", json!({}), Some(&guard)).await;
        let ids: Vec<_> = cameras["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].clone())
            .collect();
        assert_eq!(
            ids,
            vec![json!("cam-1")],
            "a camera-limited user sees their cameras only"
        );
        let refused = call("config.set", json!({"site_name": "X"}), Some(&guard)).await;
        assert_eq!(refused["error"]["code"], NOT_ALLOWED);
        let other_camera = call(
            "playback.open",
            json!({"camera_id": "cam-2", "ts_ms": 0}),
            Some(&guard),
        )
        .await;
        assert_eq!(other_camera["error"]["code"], NOT_ALLOWED);
        let status = call("auth.status", json!({"token": guard}), None).await;
        assert_eq!(status["result"]["user"]["username"], "guard");

        let root = call(
            "auth.login",
            json!({"username": "root", "password": "password-1"}),
            None,
        )
        .await;
        let root = root["result"]["token"].as_str().unwrap().to_owned();
        let accounts = call("accounts.get", json!({}), Some(&root)).await;
        assert!(!accounts.to_string().contains("salt") && !accounts.to_string().contains("hash"));
        let demoted = call(
            "accounts.set_user",
            json!({"username": "root", "role": "viewer"}),
            Some(&root),
        )
        .await;
        assert!(
            demoted["error"]["message"]
                .as_str()
                .unwrap()
                .contains("admin"),
            "the last admin stays"
        );
        let added = call(
            "accounts.set_user",
            json!({"username": "op", "role": "operator", "password": "password-2"}),
            Some(&root),
        )
        .await;
        assert_eq!(added["result"]["users"].as_array().unwrap().len(), 3);
        assert!(
            call(
                "accounts.set_user",
                json!({"username": "nopass", "role": "viewer"}),
                Some(&root)
            )
            .await["error"]
                .is_object(),
            "a new user needs a password"
        );
        // Removing the guard ends their session.
        call(
            "accounts.remove_user",
            json!({"username": "guard"}),
            Some(&root),
        )
        .await;
        assert_eq!(
            call("camera.list", json!({}), Some(&guard)).await["error"]["code"],
            SIGN_IN_REQUIRED
        );
        // Turned off, everything is open again.
        call(
            "accounts.set_enabled",
            json!({"enabled": false}),
            Some(&root),
        )
        .await;
        assert!(call("camera.list", json!({}), None).await["result"].is_array());
        teardown(&state, &cfg_path).await;
    }

    fn config_with_camera(id: &str, url: &str) -> AppConfig {
        let mut cfg = AppConfig::default();
        let mut cam = nw_core::camera::CameraConfig::new(id, url);
        cam.id = id.into();
        cfg.cameras.push(cam);
        cfg
    }

    fn error_code(response: &Value) -> i64 {
        response["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("expected error response, got {response}"))
    }

    #[tokio::test]
    async fn system_capabilities_reports_version_and_features() {
        let (state, handler, cfg_path) = handler_with_config("caps", AppConfig::default());

        let response = dispatch(&handler, "system.capabilities", json!({})).await;
        teardown(&state, &cfg_path).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(
            response["result"]["version"],
            json!(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(response["result"]["features"], json!(["v1", "v2"]));
    }

    fn config_with_cameras(count: usize) -> AppConfig {
        let mut cfg = AppConfig::default();
        for n in 0..count {
            let id = format!("cam-{n:02}");
            let mut cam =
                nw_core::camera::CameraConfig::new(&id, format!("rtsp://example.invalid/{n}"));
            cam.id = id;
            cfg.cameras.push(cam);
        }
        cfg
    }

    fn new_camera_params(id: &str) -> Value {
        let mut cam =
            nw_core::camera::CameraConfig::new(id, format!("rtsp://example.invalid/{id}"));
        cam.id = id.into();
        serde_json::to_value(&cam).expect("serialize camera")
    }

    #[tokio::test]
    async fn camera_add_refuses_the_same_stream_twice_whatever_the_credentials() {
        let (state, handler, cfg_path) = handler_with_config("dup-add", AppConfig::default());
        let mut first = nw_core::camera::CameraConfig::new("Gate", "rtsp://admin:a@10.0.0.5/live");
        first.id = "cam-a".into();
        let mut again = nw_core::camera::CameraConfig::new("Gate 2", "rtsp://10.0.0.5:554/live");
        again.id = "cam-b".into();
        let added = dispatch(
            &handler,
            "camera.add",
            serde_json::to_value(&first).unwrap(),
        )
        .await;
        let duplicate = dispatch(
            &handler,
            "camera.add",
            serde_json::to_value(&again).unwrap(),
        )
        .await;
        let count = state.read().await.config.cameras.len();
        teardown(&state, &cfg_path).await;

        assert!(added.get("result").is_some(), "{added}");
        assert_eq!(
            duplicate["error"]["message"],
            "This camera is already added as “Gate”."
        );
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn discovery_rejects_oversized_or_unknown_scopes_before_probing() {
        let (state, handler, cfg_path) = handler_with_config("disc-scope", AppConfig::default());
        let too_big = dispatch(
            &handler,
            "camera.discovery.start",
            json!({"scope": "cidr", "cidr": "10.0.0.0/16"}),
        )
        .await;
        let public = dispatch(
            &handler,
            "camera.discovery.start",
            json!({"scope": "cidr", "cidr": "8.8.8.0/24", "_portal": true}),
        )
        .await;
        let bad = dispatch(
            &handler,
            "camera.discovery.start",
            json!({"scope": "everything"}),
        )
        .await;
        let expired = dispatch(
            &handler,
            "camera.discovery.results",
            json!({"scan_id": "x"}),
        )
        .await;
        let interfaces = dispatch(&handler, "camera.discovery.interfaces", json!({})).await;
        teardown(&state, &cfg_path).await;

        assert!(
            too_big["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("1024"),
            "{too_big}"
        );
        assert!(
            public["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("private"),
            "{public}"
        );
        assert!(bad.get("error").is_some());
        assert!(expired.get("error").is_some());
        assert_eq!(interfaces["result"]["protocol_version"], 1);
        assert!(interfaces["result"]["interfaces"].is_array());
    }

    #[tokio::test]
    async fn a_free_build_refuses_a_camera_beyond_its_limit() {
        let limit = crate::edition::FREE_CAMERA_LIMIT;
        let (state, handler, cfg_path) =
            handler_with_config("camera-limit", config_with_cameras(limit));

        let response = dispatch(&handler, "camera.add", new_camera_params("cam-extra")).await;
        let status = dispatch(&handler, "system.status", json!({})).await;
        let cameras = state.read().await.config.cameras.len();
        teardown(&state, &cfg_path).await;

        if cfg!(feature = "unlimited") {
            assert!(response.get("error").is_none(), "{response}");
            assert_eq!(cameras, limit + 1);
            assert_eq!(status["result"]["edition"], "unlimited");
            assert!(status["result"]["camera_limit"].is_null());
        } else {
            assert_eq!(
                response["error"]["code"],
                crate::edition::CAMERA_LIMIT_REACHED,
                "{response}"
            );
            assert_eq!(
                response["error"]["data"]["downloads_url"],
                crate::edition::DOWNLOADS_URL
            );
            assert_eq!(cameras, limit);
            assert_eq!(status["result"]["edition"], "free");
            assert_eq!(status["result"]["camera_limit"], limit);
        }
    }

    #[tokio::test]
    async fn removing_a_camera_lets_a_held_one_run_and_held_ones_can_be_removed() {
        let mut config = config_with_cameras(crate::edition::FREE_CAMERA_LIMIT);
        for id in ["held-1", "held-2"] {
            let mut camera =
                nw_core::camera::CameraConfig::new(id, format!("rtsp://example.invalid/{id}"));
            camera.id = id.into();
            config.cameras_over_limit.push(camera);
        }
        let first = config.cameras[0].id.clone();
        let (state, handler, cfg_path) = handler_with_config("held-cameras", config);

        let status = dispatch(&handler, "system.status", json!({})).await;
        let removed = dispatch(&handler, "camera.remove", json!({"id": first})).await;
        let held_removed = dispatch(&handler, "camera.remove", json!({"id": "held-2"})).await;
        let (running, held) = {
            let st = state.read().await;
            (
                st.config
                    .cameras
                    .iter()
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>(),
                st.config.cameras_over_limit.len(),
            )
        };
        teardown(&state, &cfg_path).await;

        assert_eq!(status["result"]["cameras_over_limit"][0]["id"], "held-1");
        assert!(removed.get("error").is_none(), "{removed}");
        assert!(held_removed.get("error").is_none(), "{held_removed}");
        assert!(running.contains(&"held-1".to_string()), "{running:?}");
        assert_eq!(running.len(), crate::edition::FREE_CAMERA_LIMIT);
        assert_eq!(held, 0);
    }

    #[tokio::test]
    async fn a_desktop_view_lease_decodes_exactly_the_cameras_it_shows() {
        let _ = gstreamer::init();
        let mut config = AppConfig::default();
        let mut cam = nw_core::camera::CameraConfig::new("Cam", "rtsp://example.invalid/main");
        cam.id = "cam-lease".into();
        config.cameras.push(cam.clone());
        let (state, handler, cfg_path) = handler_with_config("view-lease", config);
        state.write().await.spawn_supervisor(&cam);

        let missing = dispatch(&handler, "view.update", json!({"camera_ids": []})).await;
        assert_eq!(error_code(&missing), -32602);
        let bad = dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": [1]}),
        )
        .await;
        assert_eq!(error_code(&bad), -32602);

        let shown = dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": ["cam-lease"]}),
        )
        .await;
        assert_eq!(shown["result"]["decoding"], 1);
        let none = dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": []}),
        )
        .await;
        assert_eq!(none["result"]["decoding"], 0);
        dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": ["cam-lease"]}),
        )
        .await;
        let released = dispatch(&handler, "view.release", json!({"client_id": "ui"})).await;
        assert_eq!(released["result"]["released"], true);
        assert_eq!(state.read().await.active_views(), 0);
        teardown(&state, &cfg_path).await;
    }

    #[tokio::test]
    async fn a_desktop_view_lease_can_listen_to_one_camera() {
        let _ = gstreamer::init();
        let mut config = AppConfig::default();
        // A file camera: the listener thread ends at once (live audio needs RTSP).
        let mut cam = nw_core::camera::CameraConfig::new("Cam", "/nonexistent/cam.mkv");
        cam.id = "cam-audio".into();
        config.cameras.push(cam);
        let (state, handler, cfg_path) = handler_with_config("view-audio", config);

        let listening = dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": [], "audio_camera_id": "cam-audio"}),
        )
        .await;
        assert_eq!(listening["result"]["listening"], "cam-audio");
        let silent = dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": []}),
        )
        .await;
        assert_eq!(
            silent["result"]["listening"],
            Value::Null,
            "absent: not listening"
        );
        let bad = dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": [], "audio_camera_id": 5}),
        )
        .await;
        assert_eq!(error_code(&bad), -32602);
        dispatch(
            &handler,
            "view.update",
            json!({"client_id": "ui", "camera_ids": [], "audio_camera_id": "cam-audio"}),
        )
        .await;
        dispatch(&handler, "view.release", json!({"client_id": "ui"})).await;
        assert_eq!(state.read().await.listening(), None, "released: silent");
        teardown(&state, &cfg_path).await;
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found_and_echoes_id() {
        let (state, handler, cfg_path) = handler_with_config("unknown", AppConfig::default());

        let response = handler
            .dispatch(&json!({"jsonrpc": "2.0", "id": "abc", "method": "nope.missing"}))
            .await;
        let missing_method = handler.dispatch(&json!({"jsonrpc": "2.0", "id": 7})).await;
        teardown(&state, &cfg_path).await;

        assert_eq!(error_code(&response), -32601);
        assert_eq!(response["id"], "abc");
        assert!(response.get("result").is_none());
        assert_eq!(error_code(&missing_method), -32601);
        assert_eq!(missing_method["id"], 7);
    }

    #[tokio::test]
    async fn motion_and_events_methods_are_served() {
        let (state, handler, cfg_path) = handler_with_config(
            "events",
            config_with_camera("cam-ev", "rtsp://example.invalid/a"),
        );

        let motion = dispatch(&handler, "motion.configure", json!({})).await;
        let events = dispatch(&handler, "events.list", json!({})).await;
        let status = dispatch(&handler, "motion.status", json!({})).await;
        let unknown = dispatch(&handler, "events.nope", json!({})).await;
        teardown(&state, &cfg_path).await;

        assert_eq!(error_code(&motion), -32602, "camera_id is required");
        assert!(events["result"]["events"].is_array(), "{events}");
        assert_eq!(status["result"]["cameras"]["cam-ev"]["source"], "off");
        assert_eq!(error_code(&unknown), -32601);
    }

    #[tokio::test]
    async fn camera_update_keeps_motion_and_onvif_settings_it_does_not_carry() {
        let mut config = config_with_camera("cam-keep", "rtsp://example.invalid/a");
        config.cameras[0].motion.source = nw_core::camera::MotionSource::Onvif;
        config.cameras[0].onvif = Some(nw_core::camera::OnvifEndpoint {
            device_url: "http://192.0.2.1/onvif/device_service".into(),
            events_url: None,
            ptz_url: None,
        });
        let (state, handler, cfg_path) = handler_with_config("keep", config);
        let mut edited = serde_json::to_value(&state.read().await.config.cameras[0]).unwrap();
        edited["name"] = json!("Renamed");
        edited.as_object_mut().unwrap().remove("motion");
        edited.as_object_mut().unwrap().remove("onvif");
        let reply = dispatch(&handler, "camera.update", edited).await;
        let cam = state.read().await.config.cameras[0].clone();
        teardown(&state, &cfg_path).await;

        assert!(reply["result"].is_object(), "{reply}");
        assert_eq!(cam.name, "Renamed");
        assert_eq!(cam.motion.source, nw_core::camera::MotionSource::Onvif);
        assert!(cam.onvif.is_some());
    }

    #[tokio::test]
    async fn editing_a_camera_keeps_its_place_on_the_wall_and_saves_motion() {
        let mut config = config_with_camera("cam-place", "rtsp://example.invalid/a");
        config.cameras[0].wall_position = Some(nw_core::camera::WallPosition {
            page: 1,
            row: 2,
            col: 3,
            row_span: 1,
            col_span: 1,
        });
        let placed = config.cameras[0].wall_position.clone();
        let (state, handler, cfg_path) = handler_with_config("place", config);
        let stored = serde_json::to_value(&state.read().await.config.cameras[0]).unwrap();

        // The camera dialog sends no position (or a null one) with its edit.
        let mut edited = stored.clone();
        edited["name"] = json!("Renamed");
        edited["wall_position"] = Value::Null;
        dispatch(&handler, "camera.update", edited).await;
        let mut motion_only = stored.clone();
        motion_only.as_object_mut().unwrap().remove("wall_position");
        motion_only["name"] = json!("Renamed");
        motion_only["motion"]["source"] = json!("builtin");
        let reply = dispatch(&handler, "camera.update", motion_only).await;
        let after = state.read().await.config.cameras[0].clone();
        // An explicit position still applies.
        let mut moved = serde_json::to_value(&after).unwrap();
        moved["wall_position"] = json!({"page": 0, "row": 0, "col": 0});
        dispatch(&handler, "camera.update", moved).await;
        let last = state.read().await.config.cameras[0].clone();
        teardown(&state, &cfg_path).await;

        assert!(reply["result"].is_object(), "{reply}");
        assert_eq!(after.wall_position, placed, "edits never move a camera");
        assert_eq!(after.motion.source, nw_core::camera::MotionSource::Builtin);
        assert_eq!(after.name, "Renamed");
        assert_eq!(
            last.wall_position.map(|p| (p.page, p.row, p.col)),
            Some((0, 0, 0))
        );
    }

    #[tokio::test]
    async fn camera_list_returns_configured_cameras() {
        let (state, handler, cfg_path) = handler_with_config(
            "list",
            config_with_camera("cam-list", "rtsp://example.invalid/a"),
        );

        let response = dispatch(&handler, "camera.list", json!({})).await;
        teardown(&state, &cfg_path).await;

        let cams = response["result"].as_array().expect("array result");
        assert_eq!(cams.len(), 1);
        assert_eq!(cams[0]["id"], "cam-list");
        assert_eq!(cams[0]["url"], "rtsp://example.invalid/a");
        assert!(
            !cfg_path.exists(),
            "camera.list must not persist the config"
        );
    }

    #[tokio::test]
    async fn camera_list_is_empty_array_without_cameras() {
        let (state, handler, cfg_path) = handler_with_config("list-empty", AppConfig::default());

        let response = dispatch(&handler, "camera.list", json!({})).await;
        teardown(&state, &cfg_path).await;

        assert_eq!(response["result"], json!([]));
    }

    #[tokio::test]
    async fn camera_remove_rejects_missing_and_unknown_ids() {
        let (state, handler, cfg_path) = handler_with_config(
            "remove-invalid",
            config_with_camera("cam-keep", "rtsp://example.invalid/a"),
        );

        let missing = dispatch(&handler, "camera.remove", json!({})).await;
        let non_string = dispatch(&handler, "camera.remove", json!({"id": 42})).await;
        let unknown = dispatch(&handler, "camera.remove", json!({"id": "cam-ghost"})).await;
        let remaining = state.read().await.config.cameras.len();
        let persisted = cfg_path.exists();
        teardown(&state, &cfg_path).await;

        assert_eq!(error_code(&missing), -32602);
        assert_eq!(missing["error"]["message"], "Missing 'id'");
        assert_eq!(error_code(&non_string), -32602);
        assert_eq!(error_code(&unknown), -32602);
        assert_eq!(unknown["error"]["message"], "Camera not found");
        assert_eq!(remaining, 1, "failed removals must not mutate state");
        assert!(!persisted, "failed removals must not write config");
    }

    #[tokio::test]
    async fn camera_remove_drops_known_camera_and_persists() {
        let (state, handler, cfg_path) = handler_with_config(
            "remove-known",
            config_with_camera("cam-gone", "rtsp://example.invalid/a"),
        );

        let response = dispatch(&handler, "camera.remove", json!({"id": "cam-gone"})).await;
        let remaining = state.read().await.config.cameras.len();
        let on_disk = std::fs::read_to_string(&cfg_path).unwrap_or_default();
        teardown(&state, &cfg_path).await;

        assert_eq!(response["result"], json!({ "removed": true }));
        assert_eq!(remaining, 0);
        assert!(
            !on_disk.contains("cam-gone"),
            "removed camera must not be persisted, got {on_disk}"
        );
    }

    #[tokio::test]
    async fn camera_set_positions_validates_shape_and_ids() {
        let (state, handler, cfg_path) = handler_with_config(
            "positions-invalid",
            config_with_camera("cam-pos", "rtsp://example.invalid/a"),
        );

        let not_array = dispatch(&handler, "camera.set_positions", json!({"id": "cam-pos"})).await;
        let missing_id = dispatch(
            &handler,
            "camera.set_positions",
            json!([{"wall_position": {"page": 1, "row": 0, "col": 0}}]),
        )
        .await;
        let position = state.read().await.config.cameras[0].wall_position.clone();
        teardown(&state, &cfg_path).await;

        assert_eq!(error_code(&not_array), -32602);
        assert_eq!(not_array["error"]["message"], "Expected array");
        assert_eq!(error_code(&missing_id), -32602);
        assert_eq!(missing_id["error"]["message"], "Missing 'id' in entry");
        assert!(
            position.is_none(),
            "rejected requests must not move cameras"
        );
    }

    #[tokio::test]
    async fn camera_set_positions_clamps_tile_spans() {
        let (state, handler, cfg_path) = handler_with_config(
            "positions-spans",
            config_with_camera("cam-pos", "rtsp://example.invalid/a"),
        );

        let response = dispatch(
            &handler,
            "camera.set_positions",
            json!([{"id": "cam-pos", "wall_position": {
                "page": 0, "row": 0, "col": 0, "row_span": 0, "col_span": 9_999_999_999u64
            }}]),
        )
        .await;
        let position = state.read().await.config.cameras[0].wall_position.clone();
        teardown(&state, &cfg_path).await;

        assert_eq!(response["result"], json!({ "ok": true }));
        let position = position.expect("position saved");
        assert_eq!(position.row_span, 1, "a zero span is one cell");
        assert_eq!(
            position.col_span, 1,
            "a span that does not fit u32 falls back to one cell"
        );
    }

    #[tokio::test]
    async fn camera_set_positions_updates_known_ids_and_ignores_unknown() {
        let (state, handler, cfg_path) = handler_with_config(
            "positions-apply",
            config_with_camera("cam-pos", "rtsp://example.invalid/a"),
        );

        let response = dispatch(
            &handler,
            "camera.set_positions",
            json!([
                {"id": "cam-ghost", "wall_position": {"page": 9, "row": 9, "col": 9}},
                {"id": "cam-pos", "wall_position": {"page": 2, "row": 1, "col": 3, "row_span": 2}},
            ]),
        )
        .await;
        let after_apply = state.read().await.config.cameras.clone();
        let cleared = dispatch(
            &handler,
            "camera.set_positions",
            json!([{"id": "cam-pos", "wall_position": null}]),
        )
        .await;
        let after_null = state.read().await.config.cameras[0].wall_position.clone();
        let persisted = cfg_path.exists();
        teardown(&state, &cfg_path).await;

        assert_eq!(response["result"], json!({ "ok": true }));
        assert_eq!(after_apply.len(), 1, "unknown ids must not add cameras");
        assert_eq!(
            after_apply[0].wall_position,
            Some(nw_core::camera::WallPosition {
                page: 2,
                row: 1,
                col: 3,
                row_span: 2,
                col_span: 1,
            })
        );
        assert_eq!(cleared["result"], json!({ "ok": true }));
        assert_eq!(
            after_null, after_apply[0].wall_position,
            "non-object wall_position leaves the existing position untouched"
        );
        assert!(persisted, "successful set_positions must save config");
    }

    #[tokio::test]
    async fn camera_set_quality_validates_id_and_skips_sub_without_substream() {
        let (state, handler, cfg_path) = handler_with_config(
            "quality",
            config_with_camera("cam-main-only", "rtsp://example.invalid/a"),
        );

        let missing = dispatch(&handler, "camera.set_quality", json!({"sub": true})).await;
        let unknown = dispatch(
            &handler,
            "camera.set_quality",
            json!({"id": "cam-ghost", "sub": true}),
        )
        .await;
        let no_substream = dispatch(
            &handler,
            "camera.set_quality",
            json!({"id": "cam-main-only", "sub": true}),
        )
        .await;
        let supervisors = state.read().await.supervisor_states();
        teardown(&state, &cfg_path).await;

        assert_eq!(error_code(&missing), -32602);
        assert_eq!(missing["error"]["message"], "Missing 'id'");
        assert_eq!(error_code(&unknown), -32603);
        assert_eq!(unknown["error"]["message"], "Camera not found");
        assert_eq!(no_substream["result"], json!({ "ok": true }));
        assert!(
            supervisors.is_empty(),
            "sub request without a substream must not restart the supervisor"
        );
    }

    #[tokio::test]
    async fn camera_set_stream_accepts_listed_streams_and_rejects_unknown_ones() {
        let mut cfg = config_with_camera("cam-streams", "rtsp://example.invalid/s1");
        cfg.cameras[0].streams = vec![
            nw_core::camera::CameraStream {
                id: "1".into(),
                name: "Main".into(),
                url: "rtsp://example.invalid/s1".into(),
                width: Some(1920),
                height: Some(1080),
            },
            nw_core::camera::CameraStream {
                id: "2".into(),
                name: "Third".into(),
                url: "rtsp://example.invalid/s3".into(),
                width: Some(320),
                height: Some(180),
            },
        ];
        let (state, handler, cfg_path) = handler_with_config("set-stream", cfg);

        let missing = dispatch(&handler, "camera.set_stream", json!({"id": "cam-streams"})).await;
        let unknown = dispatch(
            &handler,
            "camera.set_stream",
            json!({"id": "cam-streams", "stream": "7"}),
        )
        .await;
        let ghost = dispatch(
            &handler,
            "camera.set_stream",
            json!({"id": "cam-ghost", "stream": "1"}),
        )
        .await;
        // Stream 1 is the URL already in use (no sub stream): accepted, no restart.
        let same_url = dispatch(
            &handler,
            "camera.set_stream",
            json!({"id": "cam-streams", "stream": "1"}),
        )
        .await;
        let supervisors = state.read().await.supervisor_states();
        teardown(&state, &cfg_path).await;

        assert_eq!(error_code(&missing), -32602);
        assert_eq!(missing["error"]["message"], "Missing 'stream'");
        assert_eq!(error_code(&unknown), -32602);
        assert_eq!(unknown["error"]["message"], "Unknown stream");
        assert_eq!(error_code(&ghost), -32602);
        assert_eq!(same_url["result"], json!({ "ok": true }));
        assert!(supervisors.is_empty(), "an unchanged URL must not restart");
    }

    #[tokio::test]
    async fn the_desktop_lists_drives_and_chooses_only_listed_ones() {
        let (state, handler, cfg_path) = handler_with_config("storage", AppConfig::default());

        let listed = dispatch(&handler, "storage.list", json!({ "all": true })).await;
        let unknown = dispatch(
            &handler,
            "storage.select",
            json!({ "mount_point": "/not/a/drive" }),
        )
        .await;
        let missing = dispatch(&handler, "storage.select", json!({})).await;
        let storage_path = state.read().await.config.recording.storage_path.clone();
        teardown(&state, &cfg_path).await;

        let result = &listed["result"];
        assert!(result["drives"].is_array(), "{listed}");
        assert_eq!(result["folder"], "nw-storage");
        assert_eq!(result["storage_path"], storage_path.as_str());
        assert!(unknown["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("not available")));
        assert_eq!(error_code(&missing), -32602);
        assert_eq!(
            state.read().await.config.recording.storage_path,
            storage_path
        );
    }

    #[tokio::test]
    async fn the_desktop_windows_are_saved_and_come_back() {
        let (state, handler, cfg_path) = handler_with_config("desktop", AppConfig::default());
        let window = json!({
            "number": 2, "rows": 2, "cols": 2, "page": 1, "cycling": false,
            "tiles": [
                {"camera_id": "a", "page": 0, "row": 0, "col": 0, "row_span": 2, "col_span": 2},
                {"camera_id": "b", "page": 1, "row": 0, "col": 0}
            ],
            "geometry": {"screen": "DP-1", "x": 3900, "y": 40, "width": 1600, "height": 900, "state": "maximized"}
        });
        let saved = dispatch(
            &handler,
            "config.set",
            json!({"site_name": "Kept", "desktop": {"windows": [window]}}),
        )
        .await;
        let bad_state = dispatch(
            &handler,
            "config.set",
            json!({"site_name": "Changed", "desktop": {"windows": [{
                "number": 3, "rows": 2, "cols": 2, "tiles": [],
                "geometry": {"x": 0, "y": 0, "width": 10, "height": 10, "state": "minimized"}
            }]}}),
        )
        .await;
        let no_grid = dispatch(
            &handler,
            "config.set",
            json!({"desktop": {"windows": [{"number": 3, "rows": 0, "cols": 2, "tiles": []}]}}),
        )
        .await;
        let got = dispatch(&handler, "config.get", json!({})).await;
        let on_disk = std::fs::read_to_string(&cfg_path).unwrap_or_default();
        teardown(&state, &cfg_path).await;

        assert_eq!(saved["result"], json!({ "ok": true }));
        assert!(bad_state.get("error").is_some() && no_grid.get("error").is_some());
        // A refused layout changes nothing, not even the other settings sent with it.
        assert_eq!(got["result"]["site_name"], "Kept");
        let windows = &got["result"]["desktop"]["windows"];
        assert_eq!(windows.as_array().map(Vec::len), Some(1));
        assert_eq!(windows[0]["tiles"][1]["row_span"], 1); // defaults filled in
        assert_eq!(windows[0]["geometry"]["state"], "maximized");
        assert_eq!(windows[0]["cycling"], false);
        assert!(on_disk.contains("\"desktop\""));
    }

    #[tokio::test]
    async fn the_video_decoder_choice_is_saved_validated_and_returned() {
        let (state, handler, cfg_path) = handler_with_config("video-decoder", AppConfig::default());
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);
        let handler = handler.with_shutdown(shutdown_tx);
        let before = dispatch(&handler, "config.get", json!({})).await;
        let saved = dispatch(
            &handler,
            "config.set",
            json!({"video": {"decoder": "software"}}),
        )
        .await;
        let refused = dispatch(
            &handler,
            "config.set",
            json!({"site_name": "Changed", "video": {"decoder": "cuda"}}),
        )
        .await;
        let got = dispatch(&handler, "config.get", json!({})).await;
        let on_disk = std::fs::read_to_string(&cfg_path).unwrap_or_default();
        teardown(&state, &cfg_path).await;

        assert_eq!(before["result"]["video"]["decoder"], "auto");
        // No probe ran in this process, so there is nothing to restart.
        assert_eq!(saved["result"], json!({ "ok": true }));
        assert!(
            shutdown_rx.try_recv().is_err(),
            "no restart without a probe"
        );
        assert!(refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("video decoder")));
        assert_eq!(got["result"]["video"]["decoder"], "software");
        assert_ne!(
            got["result"]["site_name"], "Changed",
            "a refused value changes nothing"
        );
        assert!(on_disk.contains("\"decoder\": \"software\""), "{on_disk}");
    }

    #[test]
    fn a_config_without_desktop_windows_still_loads() {
        let mut value = serde_json::to_value(AppConfig::default()).expect("config serializes");
        value.as_object_mut().map(|o| o.remove("desktop"));
        let config: AppConfig = serde_json::from_value(value).expect("older config loads");
        assert!(config.desktop.windows.is_empty());
    }

    #[tokio::test]
    async fn config_get_returns_settings_without_cameras() {
        let mut cfg = config_with_camera("cam-cfg", "rtsp://secret@example.invalid/a");
        cfg.site_name = "cfg-get-site".into();
        cfg.wall.rows = 3;
        cfg.wall.cols = 4;
        let (state, handler, cfg_path) = handler_with_config("config-get", cfg);

        let response = dispatch(&handler, "config.get", json!({})).await;
        teardown(&state, &cfg_path).await;

        let result = &response["result"];
        assert_eq!(result["site_name"], "cfg-get-site");
        assert_eq!(result["wall"]["rows"], 3);
        assert_eq!(result["wall"]["cols"], 4);
        for key in ["auto_fit", "page_interval_sec"] {
            assert!(result["wall"].get(key).is_some(), "wall missing '{key}'");
        }
        assert_eq!(result["ui"]["fullscreen"], true);
        assert_eq!(result["ui"]["fullscreen_status"], true);
        assert_eq!(result["ui"]["fullscreen_status_opacity"], 50);
        assert_eq!(
            result["ui"]["preview_fps"],
            json!({"single": 25, "wall": 15, "dense": 4, "hidden": 2})
        );
        assert!(result["ui"]["theme"].is_string());
        for key in [
            "storage_path",
            "segment_minutes",
            "pre_buffer_sec",
            "local_all_cameras",
            "disk_threshold_percent",
        ] {
            assert!(
                result["recording"].get(key).is_some(),
                "recording missing '{key}'"
            );
        }
        assert!(result["debug"].get("gst_log").is_some());
        assert_eq!(result["events"]["retention_days"], 30);
        assert_eq!(result["events"]["push_port"], 7681);
        assert!(result.get("cameras").is_none());
        assert!(!response.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn event_settings_change_within_bounds_only() {
        let (state, handler, cfg_path) =
            handler_with_config("event-settings", AppConfig::default());
        let changed = dispatch(
            &handler,
            "config.set",
            json!({"events": {"retention_days": 90, "push_port": 8100}}),
        )
        .await;
        let pull_only = dispatch(&handler, "config.set", json!({"events": {"push_port": 0}})).await;
        let privileged =
            dispatch(&handler, "config.set", json!({"events": {"push_port": 80}})).await;
        let too_long = dispatch(
            &handler,
            "config.set",
            json!({"events": {"retention_days": 4000}}),
        )
        .await;
        let events = state.read().await.config.events.clone();
        teardown(&state, &cfg_path).await;

        assert!(changed.get("result").is_some(), "{changed}");
        assert!(pull_only.get("result").is_some(), "{pull_only}");
        assert!(privileged.get("error").is_some(), "{privileged}");
        assert!(too_long.get("error").is_some(), "{too_long}");
        assert_eq!(events.retention_days, 90);
        assert_eq!(events.push_port, 0);
    }

    #[tokio::test]
    async fn archive_settings_are_validated_and_kept_apart_from_recordings() {
        let mut cfg = AppConfig::default();
        cfg.recording.storage_path = "/srv/nw-recordings".into();
        let (state, handler, cfg_path) = handler_with_config("archive-settings", cfg);
        let set = |archive: Value| {
            dispatch(
                &handler,
                "config.set",
                json!({"recording": {"archive": archive}}),
            )
        };
        let inside = set(json!({"enabled": true, "path": "/srv/nw-recordings/archive"})).await;
        let no_folder = set(json!({"enabled": true})).await;
        let bad_hours = set(json!({"hours": {"start": "25:00", "end": "06:00"}})).await;
        let saved = set(json!({
            "enabled": true, "path": "/mnt/nas/nw-archive", "after_days": 14,
            "quota_gb": 500, "hours": {"start": "22:00", "end": "06:00"}, "max_mbps": 100
        }))
        .await;
        // Other recording settings sent without `archive` leave it alone.
        let other = dispatch(
            &handler,
            "config.set",
            json!({"recording": {"disk_threshold_percent": 80}}),
        )
        .await;
        let got = dispatch(&handler, "config.get", json!({})).await;
        let status = dispatch(&handler, "recording.archive_status", json!({})).await;
        teardown(&state, &cfg_path).await;

        for refused in [&inside, &no_folder, &bad_hours] {
            assert!(refused.get("error").is_some(), "{refused}");
        }
        assert!(inside
            .to_string()
            .contains("outside the recording location"));
        assert!(saved.get("result").is_some(), "{saved}");
        assert!(other.get("result").is_some(), "{other}");
        let archive = &got["result"]["recording"]["archive"];
        assert_eq!(archive["path"], "/mnt/nas/nw-archive");
        assert_eq!(archive["after_days"], 14);
        assert_eq!(archive["quota_gb"], 500);
        assert_eq!(archive["hours"]["end"], "06:00");
        assert_eq!(archive["max_mbps"], 100);
        let status = &status["result"];
        assert_eq!(status["state"], "idle", "on, no pass ran yet");
        for key in [
            "pending_files",
            "archived_bytes",
            "moved_files",
            "missing_locations",
        ] {
            assert!(status.get(key).is_some(), "archive status missing '{key}'");
        }
    }

    #[tokio::test]
    async fn malformed_params_return_invalid_params() {
        let (state, handler, cfg_path) = handler_with_config(
            "malformed",
            config_with_camera("cam-existing", "rtsp://example.invalid/a"),
        );

        let add = dispatch(&handler, "camera.add", json!({"name": 5})).await;
        let add_no_params = handler
            .dispatch(&json!({"jsonrpc": "2.0", "id": 2, "method": "camera.add"}))
            .await;
        let update = dispatch(&handler, "camera.update", json!("not-an-object")).await;
        let mut ghost = nw_core::camera::CameraConfig::new("ghost", "rtsp://example.invalid/g");
        ghost.id = "cam-ghost".into();
        let update_unknown = dispatch(
            &handler,
            "camera.update",
            serde_json::to_value(&ghost).expect("serialize camera"),
        )
        .await;
        let test_missing_url = dispatch(&handler, "camera.test", json!({})).await;
        let cameras = state.read().await.config.cameras.len();
        let supervisors = state.read().await.supervisor_states();
        let persisted = cfg_path.exists();
        teardown(&state, &cfg_path).await;

        assert_eq!(error_code(&add), -32602);
        assert_eq!(error_code(&add_no_params), -32602);
        assert_eq!(add_no_params["id"], 2);
        assert_eq!(error_code(&update), -32602);
        assert_eq!(error_code(&update_unknown), -32602);
        assert_eq!(update_unknown["error"]["message"], "Camera not found");
        assert_eq!(error_code(&test_missing_url), -32602);
        assert_eq!(test_missing_url["error"]["message"], "Missing 'url'");
        assert_eq!(cameras, 1);
        assert!(supervisors.is_empty());
        assert!(!persisted);
    }

    #[tokio::test]
    async fn cloud_methods_are_served_on_the_protected_socket_only() {
        let (state, handler, cfg_path) =
            handler_with_config("cloud-transport", AppConfig::default());
        let without_cloud = dispatch(&handler, "cloud.status", json!({})).await;
        let node_dir =
            std::env::temp_dir().join(format!("nw-cloud-transport-{}", uuid::Uuid::new_v4()));
        let node = crate::cloud::link::CloudNode::open(node_dir.clone()).expect("node opens");
        let handler = handler.with_cloud(Arc::new(crate::cloud::cloud_handler::CloudHandler::new(
            node,
        )));
        let request = json!({"jsonrpc":"2.0","id":1,"method":"cloud.status","params":{}});

        let local = handler
            .dispatch_from(&request, IpcTransport::Protected)
            .await;
        let tcp = handler.dispatch_from(&request, IpcTransport::Tcp).await;
        let unknown = dispatch(&handler, "other.method", json!({})).await;
        teardown(&state, &cfg_path).await;
        let _ = std::fs::remove_dir_all(node_dir);

        assert_eq!(local["result"]["phase"], "unlinked");
        assert_eq!(tcp["error"]["code"], -32601, "never over plain TCP");
        assert_eq!(error_code(&without_cloud), -32601);
        assert_eq!(error_code(&unknown), -32601);
    }
}
