//! JSON-RPC surface for the native viewer runtime.
//!
//! Methods (all `viewer.*`):
//!   - `viewer.capabilities`       → `{ available, preferred_sink, sinks, reason? }`
//!   - `viewer.attach { camera_id, window_handle }` → `{ session_id }`
//!   - `viewer.set_window { session_id, window_handle }` → `{ ok: true }`
//!   - `viewer.detach { session_id }` → `{ ok: true }`
//!
//! The IPC boundary never carries camera URLs or credentials.  The `camera_id`
//! is looked up server-side and the resulting URL never appears in a response
//! or error message — see the `viewer.attach` sanitization test in
//! `ipc/mod.rs`.

use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::viewer::{ViewerError, ViewerManager, WindowHandle};

pub struct ViewerHandler {
    state: Arc<RwLock<AppState>>,
    manager: Arc<ViewerManager>,
}

impl ViewerHandler {
    pub fn new(state: Arc<RwLock<AppState>>, manager: Arc<ViewerManager>) -> Self {
        Self { state, manager }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "viewer.capabilities" => self.capabilities().await,
            "viewer.attach" => self.attach(params).await,
            "viewer.set_window" => self.set_window(params).await,
            "viewer.detach" => self.detach(params).await,
            _ => Err(RpcError::method_not_found()),
        }
    }

    async fn capabilities(&self) -> Result<Value, RpcError> {
        serde_json::to_value(self.manager.capabilities())
            .map_err(|e| RpcError::internal(e.to_string()))
    }

    async fn attach(&self, params: &Value) -> Result<Value, RpcError> {
        let camera_id = params["camera_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'camera_id'"))?
            .to_owned();
        let handle = extract_window_handle(params)?;

        let cam = {
            let st = self.state.read().await;
            st.config
                .cameras
                .iter()
                .find(|c| c.id == camera_id)
                .cloned()
        };
        let mut cam = cam
            .ok_or(ViewerError::UnknownCamera {
                camera_id: camera_id.clone(),
            })
            .map_err(viewer_err_to_rpc)?;
        // Optional `stream` ("main", "sub" or a stream id) picks what the
        // overlay decodes; without it the main URL is used, as before.
        if let Some(selector) = params["stream"].as_str() {
            cam.url = cam
                .stream_url(selector)
                .ok_or_else(|| RpcError::invalid_params("Unknown stream"))?;
        }

        let session_id = self
            .manager
            .attach(&cam, handle)
            .map_err(viewer_err_to_rpc)?;
        // Optional `client_id`: the desktop UI whose view lease this session
        // ends with (see `view.update`).
        if let Some(client_id) = params["client_id"].as_str() {
            self.manager.set_owner(&session_id, client_id);
        }
        Ok(json!({ "session_id": session_id }))
    }

    async fn set_window(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?
            .to_owned();
        let handle = extract_window_handle(params)?;
        self.manager
            .set_window(&session_id, handle)
            .map_err(viewer_err_to_rpc)?;
        Ok(json!({ "ok": true }))
    }

    async fn detach(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?;
        self.manager.detach(session_id).map_err(viewer_err_to_rpc)?;
        Ok(json!({ "ok": true }))
    }
}

fn extract_window_handle(params: &Value) -> Result<WindowHandle, RpcError> {
    let raw = params["window_handle"]
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params("Missing or invalid 'window_handle'"))?;
    if raw == 0 {
        return Err(RpcError::invalid_params("'window_handle' must be non-zero"));
    }
    Ok(WindowHandle::Raw(raw))
}

fn viewer_err_to_rpc(err: ViewerError) -> RpcError {
    // ViewerError::Display is guaranteed to be sanitized — safe to forward as-is.
    let message = err.to_string();
    tracing::warn!(error = %message, "viewer IPC error");
    match err {
        ViewerError::UnknownCamera { .. } | ViewerError::UnknownSession { .. } => {
            RpcError::invalid_params(message)
        }
        ViewerError::MissingWindowHandle => RpcError::invalid_params(message),
        ViewerError::Unavailable { .. } | ViewerError::Pipeline { .. } => {
            RpcError::internal(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::RwLock;

    use crate::app_state::AppState;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;
    use crate::viewer::capabilities::ViewerCapabilities;
    use crate::viewer::ViewerManager;

    use super::ViewerHandler;

    fn test_state() -> Arc<RwLock<AppState>> {
        let cfg_path = std::env::temp_dir().join(format!(
            "nw-viewer-handler-test-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        Arc::new(RwLock::new(AppState::new(
            AppConfig::default(),
            cfg_path,
            HwAccelKind::Software,
        )))
    }

    fn unavailable_handler(state: Arc<RwLock<AppState>>) -> ViewerHandler {
        let manager = Arc::new(ViewerManager::new(
            HwAccelKind::Software,
            ViewerCapabilities::unavailable("no_overlay_sink"),
        ));
        ViewerHandler::new(state, manager)
    }

    #[tokio::test]
    async fn capabilities_returns_available_flag() {
        let _ = gstreamer::init();
        let state = test_state();
        let handler = unavailable_handler(Arc::clone(&state));

        let result = handler
            .dispatch("viewer.capabilities", &json!({}))
            .await
            .expect("capabilities must return Ok");
        assert_eq!(result["available"], json!(false));
        assert_eq!(result["reason"], json!("no_overlay_sink"));
        state.write().await.stop_all();
    }

    #[tokio::test]
    async fn attach_requires_camera_id() {
        let _ = gstreamer::init();
        let state = test_state();
        let handler = unavailable_handler(Arc::clone(&state));

        let err = handler
            .dispatch("viewer.attach", &json!({ "window_handle": 1 }))
            .await
            .expect_err("missing camera_id must error");
        assert_eq!(err.code, -32602);
        state.write().await.stop_all();
    }

    #[tokio::test]
    async fn attach_requires_nonzero_window_handle() {
        let _ = gstreamer::init();
        let state = test_state();
        let handler = unavailable_handler(Arc::clone(&state));

        let err = handler
            .dispatch(
                "viewer.attach",
                &json!({ "camera_id": "cam-x", "window_handle": 0 }),
            )
            .await
            .expect_err("zero window_handle must error");
        assert_eq!(err.code, -32602);
        state.write().await.stop_all();
    }

    #[tokio::test]
    async fn attach_rejects_an_unknown_stream() {
        let _ = gstreamer::init();
        let state = test_state();
        let cam = nw_core::camera::CameraConfig::new("StreamCam", "rtsp://camera.example/s1");
        let cam_id = cam.id.clone();
        state.write().await.config.cameras.push(cam);
        let handler = unavailable_handler(Arc::clone(&state));

        let err = handler
            .dispatch(
                "viewer.attach",
                &json!({ "camera_id": cam_id, "window_handle": 1234, "stream": "9" }),
            )
            .await
            .expect_err("unknown stream must error");
        assert_eq!(err.code, -32602);
        state.write().await.stop_all();
    }

    #[tokio::test]
    async fn attach_unknown_camera_returns_invalid_params_without_leaking_url() {
        let _ = gstreamer::init();
        let state = test_state();
        // Configure a camera with credentials in the URL so that any URL leak
        // is unmistakable.
        {
            let mut st = state.write().await;
            st.config.cameras.push(nw_core::camera::CameraConfig::new(
                "SneakyCam",
                "rtsp://alice:s3cret@camera.example/live",
            ));
        }
        let handler = unavailable_handler(Arc::clone(&state));

        let err = handler
            .dispatch(
                "viewer.attach",
                &json!({
                    "camera_id": "not-a-real-camera",
                    "window_handle": 1234,
                }),
            )
            .await
            .expect_err("unknown camera must error");
        assert_eq!(err.code, -32602);
        for forbidden in ["alice", "s3cret", "camera.example", "rtsp://"] {
            assert!(
                !err.message.contains(forbidden),
                "IPC error leaked {forbidden:?}: {}",
                err.message
            );
        }
        state.write().await.stop_all();
    }

    #[tokio::test]
    async fn attach_when_unavailable_reports_unavailable_without_leaking_url() {
        let _ = gstreamer::init();
        let state = test_state();
        let camera_id = "cam-configured".to_owned();
        {
            let mut st = state.write().await;
            let mut cam = nw_core::camera::CameraConfig::new(
                "Cam",
                "rtsp://alice:s3cret@camera.example/live",
            );
            cam.id = camera_id.clone();
            st.config.cameras.push(cam);
        }
        let handler = unavailable_handler(Arc::clone(&state));

        let err = handler
            .dispatch(
                "viewer.attach",
                &json!({ "camera_id": camera_id, "window_handle": 4321 }),
            )
            .await
            .expect_err("unavailable viewer must error");
        assert_eq!(err.code, -32603);
        for forbidden in ["alice", "s3cret", "camera.example", "rtsp://"] {
            assert!(
                !err.message.contains(forbidden),
                "IPC error leaked {forbidden:?}: {}",
                err.message
            );
        }
        state.write().await.stop_all();
    }

    #[tokio::test]
    async fn detach_unknown_session_returns_invalid_params() {
        let _ = gstreamer::init();
        let state = test_state();
        let handler = unavailable_handler(Arc::clone(&state));
        let err = handler
            .dispatch("viewer.detach", &json!({ "session_id": "vw-nope" }))
            .await
            .expect_err("unknown session must error");
        assert_eq!(err.code, -32602);
        state.write().await.stop_all();
    }
}
