//! IPC handler for `alerts.*` ([`crate::alerts`]).
//!
//! Methods:
//!   alerts.get     — {} → {channels, rules} (secrets as `KEPT_SECRET`)
//!   alerts.set     — {channels, rules} → the saved settings, masked; a secret
//!                    sent as `KEPT_SECRET` keeps the stored one
//!   alerts.test    — {channel_id} or {channel} → {ok, error?} after sending a test alert
//!   alerts.history — {limit?} → {deliveries: [Delivery]}, newest first
//!   alerts.desktop — {after?} → {seq, alerts: [DesktopAlert]}; without `after` only `seq`

use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::alerts::{AlertChannel, AlertsConfig};
use crate::app_state::AppState;

pub struct AlertsHandler {
    state: Arc<RwLock<AppState>>,
}

impl AlertsHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self { state }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "alerts.get" => {
                let masked = self.state.read().await.config.alerts.masked();
                serde_json::to_value(masked).map_err(|e| RpcError::internal(e.to_string()))
            }
            "alerts.set" => self.set(params).await,
            "alerts.test" => self.test(params).await,
            "alerts.history" => {
                let limit = params["limit"].as_u64().unwrap_or(50).clamp(1, 100) as usize;
                let alerts = Arc::clone(&self.state.read().await.alerts);
                Ok(json!({ "deliveries": alerts.history(limit) }))
            }
            "alerts.desktop" => {
                let after = match &params["after"] {
                    Value::Null => None,
                    value => Some(
                        value
                            .as_u64()
                            .ok_or_else(|| RpcError::invalid_params("'after' must be a number"))?,
                    ),
                };
                let alerts = Arc::clone(&self.state.read().await.alerts);
                let (seq, list) = alerts.desktop_since(after);
                Ok(json!({ "seq": seq, "alerts": list }))
            }
            _ => Err(RpcError::method_not_found()),
        }
    }

    async fn set(&self, params: &Value) -> Result<Value, RpcError> {
        let changes: AlertsConfig = serde_json::from_value(json!({
            "channels": params.get("channels").cloned().unwrap_or(json!([])),
            "rules": params.get("rules").cloned().unwrap_or(json!([])),
        }))
        .map_err(|e| RpcError::invalid_params(format!("Invalid alert settings: {e}")))?;
        let mut st = self.state.write().await;
        let next = st
            .config
            .alerts
            .replaced_by(changes)
            .map_err(RpcError::invalid_params)?;
        let previous = std::mem::replace(&mut st.config.alerts, next);
        if let Err(error) = st.save_config() {
            st.config.alerts = previous;
            return Err(RpcError::internal(error.to_string()));
        }
        tracing::info!(
            channels = st.config.alerts.channels.len(),
            rules = st.config.alerts.rules.len(),
            "Alert settings changed"
        );
        serde_json::to_value(st.config.alerts.masked())
            .map_err(|e| RpcError::internal(e.to_string()))
    }

    async fn test(&self, params: &Value) -> Result<Value, RpcError> {
        let (channel, site, headless, alerts) = {
            let st = self.state.read().await;
            let stored = &st.config.alerts;
            let channel = match (params.get("channel"), params["channel_id"].as_str()) {
                // An unsaved channel; its kept secrets come from the stored one.
                (Some(raw), _) => {
                    let channel: AlertChannel = serde_json::from_value(raw.clone())
                        .map_err(|e| RpcError::invalid_params(format!("Invalid channel: {e}")))?;
                    let probe = AlertsConfig {
                        channels: vec![channel],
                        rules: Vec::new(),
                    };
                    stored
                        .replaced_by(probe)
                        .map_err(RpcError::invalid_params)?
                        .channels
                        .remove(0)
                }
                (None, Some(id)) => stored
                    .channels
                    .iter()
                    .find(|c| c.id == id)
                    .cloned()
                    .ok_or_else(|| RpcError::invalid_params("Channel not found"))?,
                (None, None) => return Err(RpcError::invalid_params("Missing 'channel_id'")),
            };
            (
                channel,
                st.config.site_name.clone(),
                st.headless(),
                Arc::clone(&st.alerts),
            )
        };
        Ok(match alerts.test(&channel, &site, headless).await {
            Ok(()) => json!({ "ok": true }),
            Err(error) => json!({ "ok": false, "error": error }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::KEPT_SECRET;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;

    fn handler() -> (AlertsHandler, Arc<RwLock<AppState>>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("nw-alerts-{}.json", uuid::Uuid::new_v4()));
        let state = Arc::new(RwLock::new(AppState::with_database(
            AppConfig::default(),
            path.clone(),
            HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )));
        (AlertsHandler::new(Arc::clone(&state)), state, path)
    }

    #[tokio::test]
    async fn settings_are_saved_validated_and_returned_without_secrets() {
        let (handler, state, path) = handler();
        let settings = json!({
            "channels": [{"id": "bot", "name": "Family", "kind": "telegram",
                          "bot_token": "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZ", "chat_id": "-100"},
                         {"id": "desk", "name": "Desktop", "kind": "desktop"}],
            "rules": [{"id": "r", "name": "Motion", "kinds": ["motion"], "channels": ["bot", "desk"]}]
        });
        let saved = handler.dispatch("alerts.set", &settings).await.unwrap();
        assert_eq!(saved["channels"][0]["bot_token"], KEPT_SECRET);
        let read = handler.dispatch("alerts.get", &json!({})).await.unwrap();
        assert_eq!(read, saved);
        // Sending the masked settings back keeps the token.
        handler.dispatch("alerts.set", &read).await.unwrap();
        let stored = state.read().await.config.alerts.clone();
        assert!(matches!(&stored.channels[0].target,
            crate::alerts::ChannelTarget::Telegram { bot_token, .. } if bot_token.starts_with("123456:")));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"alerts\""));
        // Invalid settings change nothing.
        let mut bad = read.clone();
        bad["rules"][0]["channels"] = json!(["nope"]);
        assert!(handler.dispatch("alerts.set", &bad).await.is_err());
        assert_eq!(state.read().await.config.alerts, stored);
        // A desktop test lands in the desktop list.
        let (seq_before, _) = state.read().await.alerts.desktop_since(None);
        let tested = handler
            .dispatch("alerts.test", &json!({"channel_id": "desk"}))
            .await
            .unwrap();
        assert_eq!(tested, json!({"ok": true}));
        let desktop = handler
            .dispatch("alerts.desktop", &json!({"after": seq_before}))
            .await
            .unwrap();
        assert_eq!(desktop["alerts"].as_array().map(Vec::len), Some(1));
        let history = handler
            .dispatch("alerts.history", &json!({}))
            .await
            .unwrap();
        assert_eq!(history["deliveries"][0]["test"], true);
        let _ = std::fs::remove_file(&path);
    }
}
