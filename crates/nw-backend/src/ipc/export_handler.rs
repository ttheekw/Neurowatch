//! IPC handler for `export.*` methods: clip export ([`crate::export`]).
//!
//! Methods:
//!   export.start  — {camera_id, start_ms, end_ms, path?, overwrite?} → job status
//!   export.status — {job_id} → job status
//!   export.cancel — {job_id} → {cancelled}
//!   export.list   — {} → [job status], oldest first
//!
//! `path` is where the MP4 goes (absolute; `.mp4` is added when missing);
//! left out, the clip goes to the default export folder. A job status is
//! [`crate::export::ExportStatus`].

use std::path::PathBuf;
use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::export::{self, ExportError, ExportRequest};
use crate::playback::handler::PlaybackHandler;

pub struct ExportHandler {
    state: Arc<RwLock<AppState>>,
    /// Finds (and indexes) the recordings in a range.
    playback: PlaybackHandler,
}

impl ExportHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self {
            playback: PlaybackHandler::new(Arc::clone(&state)),
            state,
        }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "export.start" => self.start(params).await,
            "export.status" => self.status(params).await,
            "export.cancel" => self.cancel(params).await,
            "export.list" => self.list().await,
            _ => Err(RpcError::method_not_found()),
        }
    }

    /// A job's stored camera is authoritative; caller-supplied camera ids
    /// cannot authorize another camera's status or cancellation.
    pub(crate) async fn authorize_job(
        &self,
        session: &crate::accounts::Session,
        method: &str,
        params: &Value,
    ) -> Result<(), RpcError> {
        if session.cameras.is_empty() || !matches!(method, "export.status" | "export.cancel") {
            return Ok(());
        }
        let id = job_id(params)?;
        let exports = Arc::clone(&self.state.read().await.exports);
        if exports
            .status(id)
            .is_some_and(|job| !session.sees(&job.camera_id))
        {
            return Err(RpcError {
                code: super::handler::NOT_ALLOWED as i32,
                message: "You may not access this camera's exports".into(),
                data: None,
            });
        }
        Ok(())
    }

    async fn start(&self, params: &Value) -> Result<Value, RpcError> {
        let camera_id = params["camera_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'camera_id'"))?;
        let start_ms = params["start_ms"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'start_ms'"))?;
        let end_ms = params["end_ms"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'end_ms'"))?;
        let output = match &params["path"] {
            Value::Null => None,
            Value::String(path) if path.trim().is_empty() => None,
            Value::String(path) => Some(PathBuf::from(path)),
            _ => return Err(RpcError::invalid_params("'path' must be a string")),
        };
        let overwrite = params["overwrite"].as_bool().unwrap_or(false);
        let timelapse: Option<export::TimelapseOptions> = params
            .get("timelapse")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|e| RpcError::invalid_params(format!("Invalid timelapse options: {e}")))?;
        if let Some(options) = timelapse {
            options.validate(start_ms, end_ms).map_err(rpc_error)?;
        } else {
            export::validate_range(start_ms, end_ms).map_err(rpc_error)?;
        }

        let (camera_name, exports) = {
            let st = self.state.read().await;
            let camera = st
                .config
                .cameras
                .iter()
                .find(|c| c.id == camera_id)
                .ok_or_else(|| RpcError::invalid_params(format!("Unknown camera '{camera_id}'")))?;
            (camera.name.clone(), Arc::clone(&st.exports))
        };
        let segments = self
            .playback
            .segments_between(camera_id, start_ms, end_ms)
            .await
            .map_err(RpcError::internal)?;
        let request = ExportRequest {
            camera_id: camera_id.to_owned(),
            camera_name,
            start_ms,
            end_ms,
            output,
            overwrite,
        };
        // Checking the destination touches the disk.
        let status = tokio::task::spawn_blocking(move || match timelapse {
            Some(options) => exports.start_timelapse(&request, segments, options),
            None => exports.start(&request, segments),
        })
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?
        .map_err(rpc_error)?;
        serde_json::to_value(status).map_err(|e| RpcError::internal(e.to_string()))
    }

    async fn status(&self, params: &Value) -> Result<Value, RpcError> {
        let job_id = job_id(params)?;
        let exports = Arc::clone(&self.state.read().await.exports);
        let status = exports
            .status(job_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown export '{job_id}'")))?;
        serde_json::to_value(status).map_err(|e| RpcError::internal(e.to_string()))
    }

    async fn cancel(&self, params: &Value) -> Result<Value, RpcError> {
        let job_id = job_id(params)?;
        let exports = Arc::clone(&self.state.read().await.exports);
        Ok(json!({ "cancelled": exports.cancel(job_id) }))
    }

    async fn list(&self) -> Result<Value, RpcError> {
        let exports = Arc::clone(&self.state.read().await.exports);
        serde_json::to_value(exports.list()).map_err(|e| RpcError::internal(e.to_string()))
    }
}

fn job_id(params: &Value) -> Result<&str, RpcError> {
    params["job_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("Missing 'job_id'"))
}

fn rpc_error(error: ExportError) -> RpcError {
    if error.is_request_error() {
        RpcError::invalid_params(error.to_string())
    } else {
        RpcError::internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;

    fn handler() -> (ExportHandler, Arc<RwLock<AppState>>) {
        let mut config = AppConfig::default();
        let mut camera = nw_core::camera::CameraConfig::new("Front", "rtsp://example.invalid/s");
        camera.id = "cam".into();
        config.cameras.push(camera);
        config.recording.storage_path = std::env::temp_dir()
            .join(format!("nw-export-ipc-{}", uuid::Uuid::new_v4().simple()))
            .to_string_lossy()
            .into_owned();
        let mut app = AppState::new(
            config,
            std::env::temp_dir().join("nw-export-ipc.json"),
            HwAccelKind::Software,
        );
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::schema::apply_migrations(&conn).unwrap();
        app.db = Arc::new(std::sync::Mutex::new(conn));
        let state = Arc::new(RwLock::new(app));
        (ExportHandler::new(Arc::clone(&state)), state)
    }

    async fn error_of(handler: &ExportHandler, method: &str, params: Value) -> (i32, String) {
        let error = handler.dispatch(method, &params).await.unwrap_err();
        (error.code, error.message)
    }

    #[tokio::test]
    async fn requests_are_checked_before_anything_runs() {
        let (handler, _state) = handler();
        let cases = [
            ("export.start", json!({}), "Missing 'camera_id'"),
            (
                "export.start",
                json!({"camera_id": "cam", "start_ms": 5, "end_ms": 5}),
                "later than its start",
            ),
            (
                "export.start",
                json!({"camera_id": "nope", "start_ms": 0, "end_ms": 5}),
                "Unknown camera",
            ),
            (
                "export.start",
                json!({"camera_id": "cam", "start_ms": 0, "end_ms": 5, "path": 3}),
                "'path' must be a string",
            ),
            (
                "export.start",
                json!({"camera_id": "cam", "start_ms": 0, "end_ms": 5}),
                "No recordings",
            ),
            (
                "export.start",
                json!({"camera_id":"cam", "start_ms":0, "end_ms":1000,
                "timelapse":{"interval_sec":0,"fps":25}}),
                "Timelapse interval",
            ),
            (
                "export.start",
                json!({"camera_id":"nope", "start_ms":0, "end_ms":172800000,
                "timelapse":{"interval_sec":10,"fps":25}}),
                "Unknown camera",
            ),
            ("export.status", json!({}), "Missing 'job_id'"),
            ("export.status", json!({"job_id": "x"}), "Unknown export"),
        ];
        for (method, params, message) in cases {
            let (code, text) = error_of(&handler, method, params).await;
            assert_eq!(code, -32602, "{method}: {text}");
            assert!(text.contains(message), "{method}: {text}");
        }
        assert_eq!(
            handler
                .dispatch("export.cancel", &json!({"job_id": "x"}))
                .await
                .unwrap(),
            json!({"cancelled": false})
        );
        assert_eq!(
            handler.dispatch("export.list", &json!({})).await.unwrap(),
            json!([])
        );
        assert_eq!(
            handler
                .dispatch("export.nope", &json!({}))
                .await
                .unwrap_err()
                .code,
            -32601
        );
    }
}
