//! Plugin pages for the portal, scoped to the authenticated viewer and the plugin's bindings.
use crate::app_state::AppState;
use base64::{engine::general_purpose::STANDARD, Engine};
use nw_analytics_protocol::{
    messages::CameraInfo,
    web::{self, UserContext},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Populated by the IPC accounts gate, or by authenticated cloud request metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewer {
    pub user: String,
    pub role: String,
    #[serde(default)]
    pub cameras: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}
impl Viewer {
    pub fn owner() -> Self {
        Self {
            user: "local-owner".into(),
            role: "admin".into(),
            cameras: vec![],
            session: None,
        }
    }
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs().min(i64::MAX as u64) as i64)
}

pub async fn dispatch(
    state: &Arc<RwLock<AppState>>,
    method: &str,
    params: &Value,
    viewer: &Viewer,
) -> Result<Value, String> {
    if viewer.user.len() > 128 || !["admin", "operator", "viewer"].contains(&viewer.role.as_str()) {
        return Err("Invalid viewer".into());
    }
    let (config, host, path) = {
        let state = state.read().await;
        (
            state.config.clone(),
            Arc::clone(&state.plugins),
            state.config_path.clone(),
        )
    };
    let statuses = host.status(&config.plugins.servers);
    let cameras_for = |server: &str| -> Vec<CameraInfo> {
        config
            .cameras
            .iter()
            .filter(|camera| viewer.cameras.is_empty() || viewer.cameras.contains(&camera.id))
            .filter(|camera| {
                config.plugins.bindings.iter().any(|binding| {
                    binding.enabled && binding.server == server && binding.camera_id == camera.id
                })
            })
            .map(|camera| CameraInfo {
                id: camera.id.clone(),
                name: camera.name.chars().take(128).collect(),
            })
            .collect()
    };
    if method == "plugins.pages" {
        return Ok(
            json!({"viewer":{"user":viewer.user,"role":viewer.role},"plugins":statuses.iter().filter_map(|status|{
            let manifest=status.manifest.as_ref()?;let ui=manifest.web_ui.as_ref()?;
            let cameras=cameras_for(&status.id);
            if status.state!="connected" || !manifest.permissions.iter().any(|permission|permission=="web_ui") || (cameras.is_empty()&&viewer.role!="admin") || !nw_analytics_sdk::web::valid_path(&ui.entry){return None;}
            let name=config.plugins.servers.iter().find(|server|server.id==status.id).map_or_else(||manifest.name.clone(),|server|server.shown_name(Some(&manifest.name)));
            Some(json!({"id":status.id,"name":name,"title":ui.title,"entry":ui.entry,"topic":manifest.id,"cameras":cameras}))
        }).collect::<Vec<_>>() }),
        );
    }
    let server = params["server"].as_str().ok_or("'server' is required")?;
    let status = statuses
        .iter()
        .find(|status| status.id == server && status.state == "connected")
        .ok_or("Plugin is not connected")?;
    let manifest = status
        .manifest
        .as_ref()
        .ok_or("Plugin manifest unavailable")?;
    let key = manifest
        .identity
        .as_ref()
        .ok_or("Plugin identity unavailable")?;
    let cameras = cameras_for(server);
    if cameras.is_empty() && viewer.role != "admin" {
        return Err("No permitted camera is bound to this plugin".into());
    }
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let node = crate::cloud::link::ProtectedKeyStore::open(parent.join("cloud-node/node.key"))
        .and_then(|store| store.load_or_generate())
        .map_err(|error| error.to_string())?;
    let user = nw_analytics_sdk::identity::sign_user(
        &node,
        UserContext {
            user: viewer.user.clone(),
            role: viewer.role.clone(),
            cameras,
            plugin_key: key.public_key.clone(),
            expires_at: now() + 30,
            nonce: nw_analytics_sdk::identity::challenge().map_err(|error| error.to_string())?,
        },
    );
    match method {
        "plugins.web.request" => {
            let path = params["path"].as_str().unwrap_or("/");
            let method = params["method"].as_str().unwrap_or("GET");
            let body = params["body_b64"].as_str().unwrap_or("");
            let content_type = params["content_type"].as_str().unwrap_or("");
            if !nw_analytics_sdk::web::valid_path(path)
                || !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].contains(&method)
                || body.len() > web::CHUNK * 4 / 3 + 4
                || content_type.len() > 128
                || content_type.contains(['\r', '\n'])
            {
                return Err("Invalid plugin HTTP request".into());
            }
            if viewer.role == "viewer" && !["GET", "HEAD"].contains(&method) {
                return Err("This page is read-only for viewers".into());
            }
            let response = host
                .query(
                    server,
                    "web.request",
                    json!(web::Request {
                        path: path.into(),
                        method: method.into(),
                        content_type: content_type.into(),
                        body_b64: body.into(),
                        user
                    }),
                )
                .await?;
            let response: web::Response =
                serde_json::from_value(response).map_err(|_| "Invalid plugin HTTP response")?;
            if response.size > web::MAX_BODY
                || !(200..=599).contains(&response.status)
                || response.content_type.len() > 128
                || !response.content_type.is_ascii()
                || response.content_type.contains(['\r', '\n'])
                || nw_analytics_sdk::identity::unhex::<32>(&response.token).is_none()
            {
                return Err("Plugin response exceeds ingress limits".into());
            }
            Ok(json!(response))
        }
        "plugins.web.read" => {
            let token = params["token"].as_str().ok_or("'token' is required")?;
            let offset = params["offset"]
                .as_u64()
                .filter(|offset| *offset <= web::MAX_BODY as u64)
                .ok_or("Invalid page offset")? as usize;
            if nw_analytics_sdk::identity::unhex::<32>(token).is_none() {
                return Err("Invalid page capability".into());
            }
            let response = host
                .query(
                    server,
                    "web.read",
                    json!(web::Read {
                        token: token.into(),
                        offset,
                        user
                    }),
                )
                .await?;
            let response: web::Chunk =
                serde_json::from_value(response).map_err(|_| "Invalid page chunk")?;
            if response.offset != offset
                || response.data_b64.len() > web::CHUNK * 4 / 3 + 4
                || STANDARD
                    .decode(&response.data_b64)
                    .map_err(|_| "Invalid page data")?
                    .len()
                    > web::CHUNK
            {
                return Err("Invalid page chunk".into());
            }
            Ok(json!(response))
        }
        _ => Err("Unknown plugin web method".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<RwLock<AppState>> {
        Arc::new(RwLock::new(AppState::with_database(
            crate::config::AppConfig::default(),
            std::env::temp_dir().join(format!("nw-web-{}.json", uuid::Uuid::new_v4())),
            crate::pipeline::hwaccel::HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )))
    }

    fn viewer(role: &str) -> Viewer {
        Viewer {
            user: "u-1".into(),
            role: role.into(),
            cameras: vec![],
            session: None,
        }
    }

    #[tokio::test]
    async fn only_known_roles_and_short_names_are_served() {
        let state = state();
        let err = dispatch(&state, "plugins.pages", &json!({}), &viewer("root")).await;
        assert_eq!(err.unwrap_err(), "Invalid viewer");
        let mut long = viewer("viewer");
        long.user = "x".repeat(129);
        let err = dispatch(&state, "plugins.pages", &json!({}), &long).await;
        assert_eq!(err.unwrap_err(), "Invalid viewer");
        assert_eq!(Viewer::owner().role, "admin");
    }

    #[tokio::test]
    async fn pages_list_only_connected_plugins_with_a_page() {
        let state = state();
        let pages = dispatch(&state, "plugins.pages", &json!({}), &viewer("viewer"))
            .await
            .unwrap();
        assert_eq!(pages["viewer"], json!({"user": "u-1", "role": "viewer"}));
        assert_eq!(pages["plugins"], json!([]), "none is connected");
    }

    #[tokio::test]
    async fn a_request_names_a_connected_plugin() {
        let state = state();
        let missing = dispatch(&state, "plugins.web.request", &json!({}), &viewer("admin")).await;
        assert_eq!(missing.unwrap_err(), "'server' is required");
        let absent = dispatch(
            &state,
            "plugins.web.read",
            &json!({"server": "nw-motion-bundled"}),
            &viewer("admin"),
        )
        .await;
        assert_eq!(absent.unwrap_err(), "Plugin is not connected");
    }
}
