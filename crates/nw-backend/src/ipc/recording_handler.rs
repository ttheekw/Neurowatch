//! IPC handler for `recording.*` methods.

use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::recording::{quota, segment_tracker};

pub struct RecordingHandler {
    state: Arc<RwLock<AppState>>,
}

impl RecordingHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self { state }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "recording.start" => self.start(params).await,
            "recording.stop" => self.stop(params).await,
            "recording.status_all" => self.status_all().await,
            "recording.list_segments" => self.list_segments(params).await,
            "recording.set_config" => self.set_config(params).await,
            _ => Err(RpcError::method_not_found()),
        }
    }

    // ------------------------------------------------------------------
    // recording.start
    // ------------------------------------------------------------------

    async fn start(&self, params: &Value) -> Result<Value, RpcError> {
        let cam_id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?
            .to_string();

        let mut st = self.state.write().await;

        // Verify camera exists
        let cam = st
            .config
            .cameras
            .iter()
            .find(|c| c.id == cam_id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;

        // Resolve storage path to: <base>/<camera_id>/<YYYY-MM-DD>.
        let storage_dir = st.local_recording_output_dir(&cam);

        if let Err(e) = std::fs::create_dir_all(&storage_dir) {
            return Err(RpcError::internal(format!(
                "Cannot create storage dir: {e}"
            )));
        }

        st.recording.start(cam_id.clone(), storage_dir.clone());

        // Restart only the camera's ingest; its desktop view keeps running.
        st.restart_ingest(&cam);

        tracing::info!(cam_id = %cam_id, storage = %storage_dir.display(), "Recording started");
        Ok(json!({ "ok": true }))
    }

    // ------------------------------------------------------------------
    // recording.stop
    // ------------------------------------------------------------------

    async fn stop(&self, params: &Value) -> Result<Value, RpcError> {
        let cam_id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?
            .to_string();

        let mut st = self.state.write().await;

        let cam = st
            .config
            .cameras
            .iter()
            .find(|c| c.id == cam_id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;

        // Capture storage path before stopping (need it for segment scan)
        let storage_dir = st.recording.recording_storage_path(&cam_id).cloned();

        st.recording.stop(&cam_id);

        // Restart only the camera's ingest; its desktop view keeps running.
        st.restart_ingest(&cam);

        let db = Arc::clone(&st.db);
        let local_quota_bytes = cam
            .recording
            .local
            .quota_gb
            .map(|q| q as i64 * 1024 * 1024 * 1024);
        let cam_id_clone = cam_id.clone();
        let segment_duration_ms = (st.config.recording.segment_minutes as i64) * 60_000;

        // Scan → local quota, off the IPC hot path.
        tokio::spawn(async move {
            if let Some(dir) = storage_dir {
                // 1. Register any finished segments written since the last scan.
                let db2 = Arc::clone(&db);
                let cid = cam_id_clone.clone();
                let dir2 = dir.clone();
                tokio::task::spawn_blocking(move || {
                    segment_tracker::scan_and_record(&db2, &cid, &dir2, segment_duration_ms);
                })
                .await
                .ok();

                // 2. Enforce local disk quota.
                if let Some(lq) = local_quota_bytes {
                    let db3 = Arc::clone(&db);
                    let cid3 = cam_id_clone.clone();
                    tokio::task::spawn_blocking(move || {
                        quota::enforce(&db3, &cid3, lq, None);
                    })
                    .await
                    .ok();
                }
            }
        });

        tracing::info!(cam_id = %cam_id, "Recording stopped");
        Ok(json!({ "ok": true }))
    }

    // ------------------------------------------------------------------
    // recording.status_all
    // ------------------------------------------------------------------

    async fn status_all(&self) -> Result<Value, RpcError> {
        let st = self.state.read().await;
        let mut map = st
            .recording
            .status_all_with_pipeline_states(&st.supervisor_states());
        for cam in &st.config.cameras {
            map.entry(cam.id.clone())
                .or_insert_with(|| "idle".to_owned());
            // Asked to record, but there is nothing compressed to keep (a
            // V4L2 device): recording fails explicitly; nothing transcodes.
            if st.recording_unsupported(&cam.id) {
                map.insert(cam.id.clone(), "failed".to_owned());
            }
        }
        serde_json::to_value(map).map_err(|e| RpcError::internal(e.to_string()))
    }

    // ------------------------------------------------------------------
    // recording.list_segments
    // ------------------------------------------------------------------

    async fn list_segments(&self, params: &Value) -> Result<Value, RpcError> {
        let cam_id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?;
        let start_ts = params["start_ts"].as_i64();
        let end_ts = params["end_ts"].as_i64();

        let st = self.state.read().await;
        let conn = st
            .db
            .lock()
            .map_err(|_| RpcError::internal("DB lock poisoned"))?;

        // Every placeholder is always bound; absent bounds use the full range.
        let mut stmt = conn
            .prepare(
                "SELECT id, camera_id, start_ts, end_ts, path, size_bytes \
                 FROM segments WHERE camera_id = ?1 AND start_ts >= ?2 AND start_ts <= ?3 \
                 ORDER BY start_ts ASC",
            )
            .map_err(|e| RpcError::internal(e.to_string()))?;

        let rows: Vec<Value> = stmt
            .query_map(
                rusqlite::params![
                    cam_id,
                    start_ts.unwrap_or(i64::MIN),
                    end_ts.unwrap_or(i64::MAX)
                ],
                |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "camera_id": r.get::<_, String>(1)?,
                        "start_ts": r.get::<_, i64>(2)?,
                        "end_ts": r.get::<_, Option<i64>>(3)?,
                        "path": r.get::<_, String>(4)?,
                        "size_bytes": r.get::<_, i64>(5)?,
                    }))
                },
            )
            .map_err(|e| RpcError::internal(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(Value::Array(rows))
    }

    // ------------------------------------------------------------------
    // recording.set_config
    // ------------------------------------------------------------------

    async fn set_config(&self, params: &Value) -> Result<Value, RpcError> {
        let cam_id = params["id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'id'"))?;

        let mut st = self.state.write().await;
        let idx = st
            .config
            .cameras
            .iter()
            .position(|c| c.id == cam_id)
            .ok_or_else(|| RpcError::invalid_params("Camera not found"))?;
        let mut next_config = st.config.clone();
        let cam = {
            let cam = &mut next_config.cameras[idx];

            // Shared
            if let Some(v) = params["schedule"].as_str() {
                cam.recording.schedule = Some(v.to_owned());
            }
            // Local destination
            if let Some(v) = params["local_enabled"].as_bool() {
                cam.recording.local.enabled = v;
            }
            if let Some(v) = params["local_storage_path"].as_str() {
                cam.recording.local.storage_path = Some(v.to_owned());
            }
            if let Some(v) = params["local_quota_gb"].as_u64() {
                cam.recording.local.quota_gb = Some(v as u32);
            }
            cam.clone()
        };

        if cam.recording.local.enabled {
            let storage_dir =
                local_recording_output_dir_for(&st.config.recording.storage_path, &cam);
            std::fs::create_dir_all(&storage_dir).map_err(|e| {
                RpcError::internal(format!(
                    "Cannot prepare local recording storage '{}': {e}",
                    storage_dir.display()
                ))
            })?;
        }

        next_config
            .save(&st.config_path)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        st.config = next_config;
        st.restart_ingest(&cam);
        Ok(json!({ "ok": true }))
    }
}

fn local_recording_output_dir_for(
    global_storage_path: &str,
    cam: &nw_core::camera::CameraConfig,
) -> std::path::PathBuf {
    let raw_path = cam
        .recording
        .local
        .storage_path
        .clone()
        .unwrap_or_else(|| global_storage_path.to_owned());
    crate::app_state::expand_user_path(&raw_path)
        .join(&cam.id)
        .join(crate::app_state::current_local_date_stamp())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use nw_core::camera::CameraConfig;
    use serde_json::{json, Value};
    use tokio::sync::RwLock;

    use crate::app_state::AppState;
    use crate::config::AppConfig;
    use crate::ipc::handler::RpcHandler;
    use crate::pipeline::hwaccel::HwAccelKind;
    use crate::stats::test_support::{StaticCameras, StaticGpu, StaticIpc, StaticSystem};
    use crate::stats::{
        CameraHealthSource, GpuMetricsSource, IpcMetricsSource, PerformanceAggregator,
        SystemMetricsSource,
    };
    use crate::viewer::capabilities::ViewerCapabilities;
    use crate::viewer::ViewerManager;
    use nw_core::stats::{GpuStatus, IpcMetrics, ProcessMetrics};
    use std::sync::Mutex;

    fn test_viewer_manager() -> Arc<ViewerManager> {
        Arc::new(ViewerManager::new(
            HwAccelKind::Software,
            ViewerCapabilities::unavailable("no_overlay_sink"),
        ))
    }

    fn test_aggregator() -> Arc<PerformanceAggregator> {
        let sys: Arc<dyn SystemMetricsSource> = Arc::new(StaticSystem(ProcessMetrics::default()));
        let gpu: Arc<dyn GpuMetricsSource> = Arc::new(StaticGpu(GpuStatus::unavailable("test")));
        let ipc: Arc<dyn IpcMetricsSource> = Arc::new(StaticIpc(IpcMetrics::default()));
        let cams: Arc<dyn CameraHealthSource> = Arc::new(StaticCameras(Mutex::new(vec![])));
        Arc::new(PerformanceAggregator::new(sys, gpu, ipc, cams))
    }

    fn test_config_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("neurowatch-recording-{name}-{nanos}.json"))
    }

    fn camera(id: &str) -> CameraConfig {
        let mut cam = CameraConfig::new("Camera", "rtsp://example.invalid/stream");
        cam.id = id.to_owned();
        cam
    }

    fn state_with_config(config: AppConfig) -> Arc<RwLock<AppState>> {
        let _ = gstreamer::init();
        Arc::new(RwLock::new(AppState::new(
            config,
            test_config_path("rpc"),
            HwAccelKind::Software,
        )))
    }

    async fn rpc(handler: &RpcHandler, method: &str, params: Value) -> Value {
        let response = handler
            .dispatch(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }))
            .await;
        assert!(
            response.get("error").is_none(),
            "rpc {method} failed: {response}"
        );
        response["result"].clone()
    }

    #[tokio::test]
    async fn camera_add_with_local_recording_enabled_starts_and_reports_recording() {
        let state = state_with_config(AppConfig::default());
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());
        let mut cam = camera("cam-add-local-enabled");
        cam.recording.local.enabled = true;
        cam.recording.local.storage_path = Some(
            test_config_path("add-storage")
                .to_string_lossy()
                .into_owned(),
        );

        rpc(&handler, "camera.add", serde_json::to_value(&cam).unwrap()).await;
        let status = rpc(&handler, "recording.status_all", json!({})).await;
        let recording_status = status[&cam.id].clone();
        state.write().await.stop_all();

        assert_eq!(recording_status, "starting");
    }

    #[tokio::test]
    async fn configured_startup_disabled_camera_remains_idle_in_status() {
        let mut config = AppConfig::default();
        let cam = camera("cam-startup-local-disabled");
        config.cameras.push(cam.clone());
        let state = state_with_config(config);
        {
            let mut st = state.write().await;
            st.spawn_supervisor(&cam);
        }
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        let status = rpc(&handler, "recording.status_all", json!({})).await;
        state.write().await.stop_all();

        assert_eq!(status.get(&cam.id), Some(&Value::String("idle".to_owned())));
    }

    #[tokio::test]
    async fn set_config_enable_repath_disable_reconciles_live_runtime_and_status() {
        let mut config = AppConfig::default();
        let cam = camera("cam-config-transition");
        config.cameras.push(cam.clone());
        let state = state_with_config(config);
        {
            let mut st = state.write().await;
            st.spawn_supervisor(&cam);
        }
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());
        let path1 = test_config_path("storage-one");
        let path2 = test_config_path("storage-two");

        rpc(
            &handler,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "local_enabled": true,
                "local_storage_path": path1,
            }),
        )
        .await;
        let status = rpc(&handler, "recording.status_all", json!({})).await;
        let first_status = status[&cam.id].clone();
        let first_storage = {
            let st = state.read().await;
            st.recording.recording_storage_path(&cam.id).cloned()
        };
        state.write().await.stop_all();

        assert_eq!(first_status, "starting");
        let storage = first_storage.expect("enabled config should start recording");
        assert!(storage.starts_with(Path::new(&path1)));
        {
            let mut st = state.write().await;
            let cam = st.config.cameras[0].clone();
            st.spawn_supervisor(&cam);
        }

        rpc(
            &handler,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "local_storage_path": path2,
            }),
        )
        .await;
        let second_storage = {
            let st = state.read().await;
            st.recording.recording_storage_path(&cam.id).cloned()
        };
        state.write().await.stop_all();
        let storage = second_storage.expect("path update should keep recording active");
        assert!(storage.starts_with(Path::new(&path2)));
        {
            let mut st = state.write().await;
            let cam = st.config.cameras[0].clone();
            st.spawn_supervisor(&cam);
        }

        rpc(
            &handler,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "local_enabled": false,
            }),
        )
        .await;
        let status = rpc(&handler, "recording.status_all", json!({})).await;
        let final_status = status[&cam.id].clone();
        let final_storage = state
            .read()
            .await
            .recording
            .recording_storage_path(&cam.id)
            .cloned();
        state.write().await.stop_all();

        assert_eq!(final_status, "idle");
        assert!(final_storage.is_none());
    }

    #[tokio::test]
    async fn configured_off_falls_back_to_global_all_camera_recording() {
        let mut config = AppConfig::default();
        config.recording.local_all_cameras = true;
        config.recording.storage_path = test_config_path("global-storage")
            .to_string_lossy()
            .into_owned();
        let mut cam = camera("cam-global-fallback");
        cam.recording.local.enabled = true;
        cam.recording.local.storage_path = Some(
            test_config_path("configured-storage")
                .to_string_lossy()
                .into_owned(),
        );
        config.cameras.push(cam.clone());
        let state = state_with_config(config);
        {
            let mut st = state.write().await;
            st.spawn_supervisor(&cam);
        }
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        rpc(
            &handler,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "local_enabled": false,
            }),
        )
        .await;
        let (status, storage, global_root) = {
            let st = state.read().await;
            (
                st.recording.status_all().get(&cam.id).cloned(),
                st.recording.recording_storage_path(&cam.id).cloned(),
                std::path::PathBuf::from(&st.config.recording.storage_path),
            )
        };
        state.write().await.stop_all();

        assert_eq!(status.as_deref(), Some("starting"));
        assert!(
            storage
                .as_ref()
                .is_some_and(|path| path.starts_with(&global_root)),
            "expected global storage path, got {storage:?}"
        );
    }

    #[tokio::test]
    async fn set_config_rejects_unpreparable_local_storage_path_without_mutating_config() {
        let mut config = AppConfig::default();
        let mut cam = camera("cam-invalid-repath");
        let old_root = test_config_path("old-storage");
        cam.recording.local.enabled = true;
        cam.recording.local.storage_path = Some(old_root.to_string_lossy().into_owned());
        config.cameras.push(cam.clone());
        let state = state_with_config(config);
        {
            let mut st = state.write().await;
            st.spawn_supervisor(&cam);
        }
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());
        let invalid_root = test_config_path("storage-file");
        std::fs::write(&invalid_root, b"not a directory").unwrap();

        let response = handler
            .dispatch(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "recording.set_config",
                "params": {
                    "id": &cam.id,
                    "local_storage_path": invalid_root,
                },
            }))
            .await;

        let (persisted_path, runtime_path) = {
            let st = state.read().await;
            (
                st.config.cameras[0].recording.local.storage_path.clone(),
                st.recording.recording_storage_path(&cam.id).cloned(),
            )
        };
        state.write().await.stop_all();

        assert!(
            response.get("error").is_some(),
            "expected error: {response}"
        );
        assert_eq!(persisted_path.as_deref(), Some(old_root.to_str().unwrap()));
        assert!(
            runtime_path
                .as_ref()
                .is_some_and(|path| path.starts_with(&old_root)),
            "expected old runtime path, got {runtime_path:?}"
        );
    }

    // ------------------------------------------------------------------
    // Deterministic handler paths (no RTSP or network sources)
    // ------------------------------------------------------------------

    const INVALID_PARAMS: i32 = -32602;
    const INTERNAL_ERROR: i32 = -32603;
    const METHOD_NOT_FOUND: i32 = -32601;

    /// Camera backed by a local placeholder file so any supervisor that a
    /// handler spawns never opens a network connection.
    fn local_camera(id: &str) -> CameraConfig {
        let source = test_config_path(&format!("{id}-source"));
        std::fs::write(&source, b"not a video").unwrap();
        let mut cam = CameraConfig::new("Local", source.to_string_lossy());
        cam.id = id.to_owned();
        cam
    }

    /// State whose segment index is a private in-memory SQLite database and
    /// whose config is persisted to a unique temp path.
    async fn isolated_state(cameras: Vec<CameraConfig>) -> Arc<RwLock<AppState>> {
        let mut config = AppConfig::default();
        config.recording.storage_path = test_config_path("default-storage")
            .to_string_lossy()
            .into_owned();
        config.cameras = cameras;
        let state = state_with_config(config);
        {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            crate::db::schema::apply_migrations(&conn).unwrap();
            let mut st = state.write().await;
            st.db = Arc::new(Mutex::new(conn));
            st.config_path = test_config_path("isolated-config");
        }
        state
    }

    fn seed_segment(
        state: &AppState,
        camera_id: &str,
        start_ts: i64,
        end_ts: Option<i64>,
        path: &str,
        size_bytes: i64,
    ) {
        state
            .db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO segments (camera_id, start_ts, end_ts, path, size_bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![camera_id, start_ts, end_ts, path, size_bytes],
            )
            .unwrap();
    }

    async fn call(
        state: &Arc<RwLock<AppState>>,
        method: &str,
        params: Value,
    ) -> Result<Value, nw_core::ipc::RpcError> {
        super::RecordingHandler::new(Arc::clone(state))
            .dispatch(method, &params)
            .await
    }

    async fn call_err(state: &Arc<RwLock<AppState>>, method: &str, params: Value) -> (i32, String) {
        let err = call(state, method, params)
            .await
            .expect_err("expected an RPC error");
        (err.code, err.message)
    }

    fn segment_starts(result: &Value) -> Vec<i64> {
        result
            .as_array()
            .expect("list_segments returns an array")
            .iter()
            .map(|row| row["start_ts"].as_i64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn unknown_recording_method_is_method_not_found() {
        let state = isolated_state(vec![]).await;

        let (code, _) = call_err(&state, "recording.pause", json!({})).await;

        assert_eq!(code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_recording_method_via_rpc_router_returns_error_response() {
        let state = isolated_state(vec![]).await;
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        let response = handler
            .dispatch(&json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "recording.does_not_exist",
                "params": {},
            }))
            .await;

        assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);
        assert!(response.get("result").is_none());
    }

    #[tokio::test]
    async fn every_camera_scoped_method_rejects_missing_or_non_string_id() {
        let state = isolated_state(vec![local_camera("cam-param-check")]).await;
        let methods = [
            "recording.start",
            "recording.stop",
            "recording.list_segments",
            "recording.set_config",
        ];
        let bad_params = [
            json!({}),
            json!({ "id": 42 }),
            json!({ "id": null }),
            json!(null),
        ];

        for method in methods {
            for params in &bad_params {
                let (code, message) = call_err(&state, method, params.clone()).await;
                assert_eq!(code, INVALID_PARAMS, "{method} with {params}");
                assert_eq!(message, "Missing 'id'", "{method} with {params}");
            }
        }
        let st = state.read().await;
        assert!(
            st.supervisor_states().is_empty(),
            "no supervisor may be spawned"
        );
        assert!(st.recording.status_all().is_empty());
        assert!(!st.config_path.exists(), "config must not be persisted");
    }

    #[tokio::test]
    async fn start_stop_and_set_config_reject_unknown_camera_without_side_effects() {
        let known = local_camera("cam-known");
        let state = isolated_state(vec![known.clone()]).await;

        for method in ["recording.start", "recording.stop", "recording.set_config"] {
            let (code, message) = call_err(
                &state,
                method,
                json!({ "id": "cam-missing", "local_enabled": true }),
            )
            .await;
            assert_eq!(code, INVALID_PARAMS, "{method}");
            assert_eq!(message, "Camera not found", "{method}");
        }

        let st = state.read().await;
        assert!(
            st.supervisor_states().is_empty(),
            "no supervisor may be spawned"
        );
        assert!(st.recording.status_all().is_empty());
        assert_eq!(st.config.cameras, vec![known]);
        assert!(!st.config_path.exists(), "config must not be persisted");
    }

    async fn seeded_segment_state() -> Arc<RwLock<AppState>> {
        let state = isolated_state(vec![]).await;
        {
            let st = state.read().await;
            // Inserted out of order to prove the handler sorts by start_ts.
            seed_segment(&st, "cam-a", 3_000, Some(3_900), "/rec/cam-a_3.mkv", 30);
            seed_segment(&st, "cam-a", 1_000, Some(1_900), "/rec/cam-a_1.mkv", 10);
            seed_segment(&st, "cam-b", 2_000, Some(2_900), "/rec/cam-b_2.mkv", 99);
            seed_segment(&st, "cam-a", 4_000, None, "/rec/cam-a_4.mkv", 40);
            seed_segment(&st, "cam-a", 2_000, Some(2_900), "/rec/cam-a_2.mkv", 20);
        }
        state
    }

    #[tokio::test]
    async fn list_segments_without_filters_returns_camera_rows_in_start_order() {
        let state = seeded_segment_state().await;

        let result = call(&state, "recording.list_segments", json!({ "id": "cam-a" }))
            .await
            .unwrap();

        assert_eq!(segment_starts(&result), vec![1_000, 2_000, 3_000, 4_000]);
        let rows = result.as_array().unwrap();
        assert!(rows.iter().all(|row| row["camera_id"] == "cam-a"));
        assert!(rows[0]["id"].as_i64().is_some());
        assert_eq!(rows[0]["end_ts"], 1_900);
        assert_eq!(rows[0]["path"], "/rec/cam-a_1.mkv");
        assert_eq!(rows[0]["size_bytes"], 10);
        assert_eq!(
            rows[3]["end_ts"],
            Value::Null,
            "open segment keeps null end"
        );
    }

    #[tokio::test]
    async fn list_segments_start_filter_is_inclusive_lower_bound() {
        let state = seeded_segment_state().await;

        let result = call(
            &state,
            "recording.list_segments",
            json!({ "id": "cam-a", "start_ts": 2_000 }),
        )
        .await
        .unwrap();

        assert_eq!(segment_starts(&result), vec![2_000, 3_000, 4_000]);
    }

    #[tokio::test]
    async fn list_segments_end_filter_is_inclusive_upper_bound_on_start_ts() {
        let state = seeded_segment_state().await;

        let result = call(
            &state,
            "recording.list_segments",
            json!({ "id": "cam-a", "end_ts": 3_000 }),
        )
        .await
        .unwrap();

        // The 2_000 segment ends at 2_900 and the 3_000 segment starts at the
        // bound; both are included because filtering uses start_ts.
        assert_eq!(segment_starts(&result), vec![1_000, 2_000, 3_000]);
    }

    #[tokio::test]
    async fn list_segments_with_both_bounds_returns_window() {
        let state = seeded_segment_state().await;

        let result = call(
            &state,
            "recording.list_segments",
            json!({ "id": "cam-a", "start_ts": 1_500, "end_ts": 3_500 }),
        )
        .await
        .unwrap();

        assert_eq!(segment_starts(&result), vec![2_000, 3_000]);
        let paths: Vec<&str> = result
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/rec/cam-a_2.mkv", "/rec/cam-a_3.mkv"]);
    }

    #[tokio::test]
    async fn list_segments_empty_window_and_unknown_camera_return_empty_arrays() {
        let state = seeded_segment_state().await;

        let inverted = call(
            &state,
            "recording.list_segments",
            json!({ "id": "cam-a", "start_ts": 3_500, "end_ts": 1_500 }),
        )
        .await
        .unwrap();
        let unknown = call(&state, "recording.list_segments", json!({ "id": "cam-z" }))
            .await
            .unwrap();

        assert_eq!(inverted, json!([]));
        assert_eq!(unknown, json!([]));
    }

    #[tokio::test]
    async fn list_segments_ignores_non_integer_bounds() {
        let state = seeded_segment_state().await;

        let result = call(
            &state,
            "recording.list_segments",
            json!({ "id": "cam-b", "start_ts": "later", "end_ts": 1.5 }),
        )
        .await
        .unwrap();

        assert_eq!(segment_starts(&result), vec![2_000]);
        assert_eq!(result[0]["size_bytes"], 99);
    }

    #[tokio::test]
    async fn list_segments_over_rpc_router_returns_seeded_rows() {
        let state = seeded_segment_state().await;
        let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());

        let result = rpc(
            &handler,
            "recording.list_segments",
            json!({ "id": "cam-b", "start_ts": 0, "end_ts": 10_000 }),
        )
        .await;

        assert_eq!(segment_starts(&result), vec![2_000]);
        assert_eq!(result[0]["camera_id"], "cam-b");
    }

    #[tokio::test]
    async fn set_config_applies_every_field_and_persists_to_disk() {
        let cam = local_camera("cam-persist");
        let state = isolated_state(vec![cam.clone()]).await;
        let storage_root = test_config_path("persist-storage");

        let result = call(
            &state,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "schedule": "0 8-18 * * 1-5",
                "local_enabled": false,
                "local_storage_path": storage_root,
                "local_quota_gb": 12,
            }),
        )
        .await;
        let (in_memory, config_path, recording_path) = {
            let st = state.read().await;
            (
                st.config.cameras[0].recording.clone(),
                st.config_path.clone(),
                st.recording.recording_storage_path(&cam.id).cloned(),
            )
        };
        state.write().await.stop_all();

        assert_eq!(result.unwrap(), json!({ "ok": true }));
        let persisted = AppConfig::load_or_default(&config_path).unwrap();
        assert_eq!(persisted.cameras.len(), 1);
        let on_disk = &persisted.cameras[0].recording;
        for rec in [&in_memory, on_disk] {
            assert_eq!(rec.schedule.as_deref(), Some("0 8-18 * * 1-5"));
            assert!(!rec.local.enabled);
            assert_eq!(
                rec.local.storage_path.as_deref(),
                Some(storage_root.to_str().unwrap())
            );
            assert_eq!(rec.local.quota_gb, Some(12));
        }
        assert!(recording_path.is_none(), "local disabled must not record");
        assert!(
            !storage_root.exists(),
            "disabled local recording must not create storage"
        );
    }

    #[tokio::test]
    async fn set_config_leaves_unspecified_fields_and_ignores_wrong_types() {
        let mut cam = local_camera("cam-partial");
        cam.recording.schedule = Some("always".to_owned());
        cam.recording.local.quota_gb = Some(5);
        let state = isolated_state(vec![cam.clone()]).await;

        let result = call(
            &state,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "local_quota_gb": "ten",
            }),
        )
        .await;
        let saved = state.read().await.config.cameras[0].recording.clone();
        state.write().await.stop_all();

        assert_eq!(result.unwrap(), json!({ "ok": true }));
        assert_eq!(saved.schedule.as_deref(), Some("always"));
        assert_eq!(saved.local.quota_gb, Some(5));
    }

    /// A legacy UI/client may still send the retired `s3_*` RPC params (from
    /// a config screen that has not refreshed). The handler must ignore them
    /// silently rather than erroring, and must not resurrect any S3 state —
    /// there is no `recording.s3` field left to populate.
    #[tokio::test]
    async fn set_config_ignores_legacy_s3_rpc_params_without_error() {
        let cam = local_camera("cam-legacy-s3-params");
        let state = isolated_state(vec![cam.clone()]).await;

        let result = call(
            &state,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "local_quota_gb": 8,
                "s3_enabled": true,
                "s3_endpoint": "https://s3.example.invalid",
                "s3_bucket": "legacy-bucket",
                "s3_prefix": "site-a",
                "s3_access_key": "AKIA-LEGACY",
                "s3_secret_key": "legacy-secret",
                "s3_quota_gb": 34,
            }),
        )
        .await;
        let (saved, config_path) = {
            let st = state.read().await;
            (
                st.config.cameras[0].recording.clone(),
                st.config_path.clone(),
            )
        };
        state.write().await.stop_all();

        assert_eq!(result.unwrap(), json!({ "ok": true }));
        assert_eq!(saved.local.quota_gb, Some(8));
        let persisted_raw = std::fs::read_to_string(&config_path).unwrap();
        for secret in ["legacy-bucket", "AKIA-LEGACY", "legacy-secret"] {
            assert!(
                !persisted_raw.contains(secret),
                "persisted config must not contain legacy S3 credential: {secret}"
            );
        }
    }

    #[tokio::test]
    async fn set_config_enabling_local_creates_dated_camera_directory() {
        let cam = local_camera("cam-enable-local");
        let state = isolated_state(vec![cam.clone()]).await;
        let storage_root = test_config_path("enable-storage");

        let result = call(
            &state,
            "recording.set_config",
            json!({
                "id": &cam.id,
                "local_enabled": true,
                "local_storage_path": storage_root,
            }),
        )
        .await;
        let (runtime_path, status) = {
            let st = state.read().await;
            (
                st.recording.recording_storage_path(&cam.id).cloned(),
                st.recording.status_all().get(&cam.id).cloned(),
            )
        };
        state.write().await.stop_all();

        assert_eq!(result.unwrap(), json!({ "ok": true }));
        let expected = storage_root
            .join(&cam.id)
            .join(crate::app_state::current_local_date_stamp());
        assert!(expected.is_dir(), "expected {expected:?} to be created");
        assert_eq!(runtime_path.as_deref(), Some(expected.as_path()));
        assert_eq!(status.as_deref(), Some("starting"));
    }

    #[tokio::test]
    async fn set_config_save_failure_reports_internal_error_and_keeps_config() {
        let cam = local_camera("cam-save-failure");
        let state = isolated_state(vec![cam.clone()]).await;
        let blocker = test_config_path("config-parent-file");
        std::fs::write(&blocker, b"file, not a directory").unwrap();
        state.write().await.config_path = blocker.join("config.json");

        let (code, message) = call_err(
            &state,
            "recording.set_config",
            json!({ "id": &cam.id, "schedule": "never-saved" }),
        )
        .await;

        let st = state.read().await;
        assert_eq!(code, INTERNAL_ERROR);
        assert!(!message.is_empty());
        assert_eq!(st.config.cameras[0], cam, "config must be unchanged");
        assert!(st.supervisor_states().is_empty(), "no restart on failure");
    }

    #[tokio::test]
    async fn status_all_reports_idle_for_every_configured_camera() {
        let empty = isolated_state(vec![]).await;
        let empty_status = call(&empty, "recording.status_all", json!({}))
            .await
            .unwrap();

        let state = isolated_state(vec![local_camera("cam-s1"), local_camera("cam-s2")]).await;
        let status = call(&state, "recording.status_all", json!(null))
            .await
            .unwrap();

        assert_eq!(empty_status, json!({}));
        assert_eq!(status, json!({ "cam-s1": "idle", "cam-s2": "idle" }));
    }

    #[tokio::test]
    async fn start_then_stop_toggles_recording_state_for_local_camera() {
        let mut cam = local_camera("cam-start-stop");
        let storage_root = test_config_path("start-storage");
        cam.recording.local.storage_path = Some(storage_root.to_string_lossy().into_owned());
        let state = isolated_state(vec![cam.clone(), local_camera("cam-bystander")]).await;

        let started = call(&state, "recording.start", json!({ "id": &cam.id })).await;
        let (after_start, storage) = {
            let st = state.read().await;
            (
                st.recording.status_all(),
                st.recording.recording_storage_path(&cam.id).cloned(),
            )
        };

        let stopped = call(&state, "recording.stop", json!({ "id": &cam.id })).await;
        let (after_stop, storage_after_stop) = {
            let st = state.read().await;
            (
                st.recording.status_all(),
                st.recording.recording_storage_path(&cam.id).cloned(),
            )
        };
        state.write().await.stop_all();

        assert_eq!(started.unwrap(), json!({ "ok": true }));
        assert_eq!(stopped.unwrap(), json!({ "ok": true }));
        assert_eq!(
            after_start.get(&cam.id).map(String::as_str),
            Some("starting")
        );
        assert_eq!(after_start.get("cam-bystander"), None);
        let storage = storage.expect("start must record");
        assert!(storage.is_dir());
        assert_eq!(
            storage,
            storage_root
                .join(&cam.id)
                .join(crate::app_state::current_local_date_stamp())
        );
        assert_eq!(after_stop.get(&cam.id).map(String::as_str), Some("idle"));
        assert!(storage_after_stop.is_none());
    }

    #[tokio::test]
    async fn start_uses_global_storage_when_camera_has_no_local_path() {
        let cam = local_camera("cam-global-start");
        let state = isolated_state(vec![cam.clone()]).await;
        let global_root =
            std::path::PathBuf::from(&state.read().await.config.recording.storage_path);

        let result = call(&state, "recording.start", json!({ "id": &cam.id })).await;
        let storage = state
            .read()
            .await
            .recording
            .recording_storage_path(&cam.id)
            .cloned();
        state.write().await.stop_all();

        assert_eq!(result.unwrap(), json!({ "ok": true }));
        let storage = storage.expect("start must record");
        assert!(storage.starts_with(global_root.join(&cam.id)));
        assert!(storage.is_dir());
    }

    #[tokio::test]
    async fn start_fails_when_storage_directory_cannot_be_created() {
        let mut cam = local_camera("cam-start-blocked");
        let blocker = test_config_path("start-blocker");
        std::fs::write(&blocker, b"file, not a directory").unwrap();
        cam.recording.local.storage_path = Some(blocker.to_string_lossy().into_owned());
        let state = isolated_state(vec![cam.clone()]).await;

        let (code, message) = call_err(&state, "recording.start", json!({ "id": &cam.id })).await;

        let st = state.read().await;
        assert_eq!(code, INTERNAL_ERROR);
        assert!(
            message.starts_with("Cannot create storage dir"),
            "unexpected message: {message}"
        );
        assert!(st.recording.recording_storage_path(&cam.id).is_none());
        assert!(st.supervisor_states().is_empty());
    }

    #[tokio::test]
    async fn stop_without_active_recording_succeeds_and_stays_idle() {
        let cam = local_camera("cam-stop-idle");
        let state = isolated_state(vec![cam.clone()]).await;

        let result = call(&state, "recording.stop", json!({ "id": &cam.id })).await;
        let status = call(&state, "recording.status_all", json!({}))
            .await
            .unwrap();
        state.write().await.stop_all();

        assert_eq!(result.unwrap(), json!({ "ok": true }));
        assert_eq!(status[&cam.id], "idle");
    }

    #[tokio::test]
    async fn stop_indexes_finished_segments_from_the_recording_directory() {
        let mut cam = local_camera("cam-stop-scan");
        let storage_root = test_config_path("scan-storage");
        cam.recording.local.storage_path = Some(storage_root.to_string_lossy().into_owned());
        cam.recording.local.quota_gb = Some(100);
        let state = isolated_state(vec![cam.clone()]).await;

        call(&state, "recording.start", json!({ "id": &cam.id }))
            .await
            .unwrap();
        let storage = state
            .read()
            .await
            .recording
            .recording_storage_path(&cam.id)
            .cloned()
            .expect("start must record");
        let segment = storage.join(format!("{}_finished.mkv", cam.id));
        std::fs::write(&segment, [0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0]).unwrap();
        std::fs::write(storage.join("other-cam_1.mkv"), [0x1a, 0x45, 0xdf, 0xa3]).unwrap();

        call(&state, "recording.stop", json!({ "id": &cam.id }))
            .await
            .unwrap();
        let expected_path = segment.to_string_lossy().into_owned();
        let indexed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let rows = call(&state, "recording.list_segments", json!({ "id": &cam.id }))
                    .await
                    .unwrap();
                if !rows.as_array().unwrap().is_empty() {
                    break rows;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        state.write().await.stop_all();

        let rows = indexed.expect("stop should index the finished segment");
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["path"], expected_path.as_str());
        assert_eq!(rows[0]["camera_id"], cam.id.as_str());
        assert_eq!(rows[0]["size_bytes"], 8);
    }
}
