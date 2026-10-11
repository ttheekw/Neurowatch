//! IPC handler for `storage.*`: the drives a recording location can be put on.
//!
//! The desktop Settings dialog and the web portal (headless nodes, through
//! `remote_config`) both pick a drive from this scan; recordings then go to
//! `<drive>/nw-storage`. `storage.select` never takes a path.

use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::{expand_user_path, AppState};
use crate::storage_drives;

pub struct StorageHandler {
    state: Arc<RwLock<AppState>>,
}

impl StorageHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self { state }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "storage.list" => self.list(params).await,
            "storage.select" => self.select(params).await,
            _ => Err(RpcError::method_not_found()),
        }
    }

    /// Mounted drives, the current recording location and the folder a
    /// chosen drive records to. Unless `all`, drives smaller than
    /// `recommended_min_bytes` are left out (except the current one).
    async fn list(&self, params: &Value) -> Result<Value, RpcError> {
        let all = params["all"].as_bool().unwrap_or(false);
        let current = self
            .state
            .read()
            .await
            .config
            .recording
            .storage_path
            .clone();
        let current_path = expand_user_path(&current);
        let drives = tokio::task::spawn_blocking(move || storage_drives::list(all, &current_path))
            .await
            .map_err(|e| RpcError::internal(e.to_string()))?
            .map_err(RpcError::internal)?;
        Ok(json!({
            "drives": drives,
            "storage_path": current,
            "recommended_min_bytes": storage_drives::RECOMMENDED_MIN_BYTES,
            "folder": storage_drives::STORAGE_FOLDER,
        }))
    }

    /// Record to `<drive>/nw-storage` from now on. Recordings in the old
    /// location continue in the new one; existing files stay where they are.
    async fn select(&self, params: &Value) -> Result<Value, RpcError> {
        let mount_point = params["mount_point"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'mount_point'"))?
            .to_owned();
        let folder = tokio::task::spawn_blocking(move || storage_drives::prepare(&mount_point))
            .await
            .map_err(|e| RpcError::internal(e.to_string()))?
            .map_err(RpcError::internal)?;
        let storage_path = folder.to_string_lossy().into_owned();
        self.state
            .write()
            .await
            .update_config_settings(&json!({ "recording": { "storage_path": storage_path } }))
            .map_err(|e| RpcError::internal(e.to_string()))?;
        tracing::info!(storage_path = %storage_path, "Recording location changed to a drive");
        Ok(json!({ "storage_path": storage_path }))
    }
}
