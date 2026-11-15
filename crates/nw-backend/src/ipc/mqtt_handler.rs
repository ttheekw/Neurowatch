//! IPC handler for `mqtt.*` ([`crate::mqtt`]).
//!
//! Methods (administrators, like every unlisted method):
//!   mqtt.get    — {} → {config (password as `KEPT_SECRET`), status}
//!   mqtt.set    — MqttConfig → the same as `mqtt.get`; a password sent as
//!                 `KEPT_SECRET` keeps the stored one; reconnects at once
//!   mqtt.status — {} → MqttStatus {state off|connecting|connected|error, detail?, broker?}
//!   mqtt.test   — MqttConfig (unsaved) → {ok, error?} after trying to connect

use std::sync::Arc;

use nw_core::ipc::RpcError;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::mqtt::{MqttConfig, MqttService};

pub struct MqttHandler {
    state: Arc<RwLock<AppState>>,
}

impl MqttHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self { state }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "mqtt.get" => Ok(self.current().await),
            "mqtt.status" => {
                let mqtt = Arc::clone(&self.state.read().await.mqtt);
                serde_json::to_value(mqtt.status()).map_err(|e| RpcError::internal(e.to_string()))
            }
            "mqtt.set" => self.set(params).await,
            "mqtt.test" => {
                let config = {
                    let st = self.state.read().await;
                    st.config
                        .mqtt
                        .replaced_by(parse(params)?)
                        .map_err(RpcError::invalid_params)?
                };
                if config.host.is_empty() {
                    return Err(RpcError::invalid_params("Enter the MQTT broker's address"));
                }
                Ok(match MqttService::test(&config).await {
                    Ok(()) => json!({ "ok": true }),
                    Err(error) => json!({ "ok": false, "error": error }),
                })
            }
            _ => Err(RpcError::method_not_found()),
        }
    }

    async fn current(&self) -> Value {
        let st = self.state.read().await;
        json!({ "config": st.config.mqtt.masked(), "status": st.mqtt.status() })
    }

    async fn set(&self, params: &Value) -> Result<Value, RpcError> {
        let changes = parse(params)?;
        let mqtt = {
            let mut st = self.state.write().await;
            let next = st
                .config
                .mqtt
                .replaced_by(changes)
                .map_err(RpcError::invalid_params)?;
            let previous = std::mem::replace(&mut st.config.mqtt, next);
            if let Err(error) = st.save_config() {
                st.config.mqtt = previous;
                return Err(RpcError::internal(error.to_string()));
            }
            tracing::info!(
                enabled = st.config.mqtt.enabled,
                discovery = st.config.mqtt.discovery,
                "MQTT settings changed"
            );
            Arc::clone(&st.mqtt)
        };
        mqtt.reconfigured();
        Ok(self.current().await)
    }
}

fn parse(params: &Value) -> Result<MqttConfig, RpcError> {
    let raw = params.get("config").unwrap_or(params);
    serde_json::from_value(raw.clone())
        .map_err(|e| RpcError::invalid_params(format!("Invalid MQTT settings: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::mqtt::config::KEPT_SECRET;
    use crate::pipeline::hwaccel::HwAccelKind;

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn handler() -> (MqttHandler, Arc<RwLock<AppState>>, Cleanup) {
        let path = std::env::temp_dir().join(format!("nw-mqtt-{}.json", uuid::Uuid::new_v4()));
        let state = Arc::new(RwLock::new(AppState::with_database(
            AppConfig::default(),
            path.clone(),
            HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )));
        (MqttHandler::new(Arc::clone(&state)), state, Cleanup(path))
    }

    #[tokio::test]
    async fn settings_are_saved_checked_and_returned_without_the_password() {
        let (handler, state, _dir) = handler();
        let settings = json!({"enabled": true, "host": "broker.local", "username": "nw", "password": "s3cret"});
        let saved = handler.dispatch("mqtt.set", &settings).await.unwrap();
        assert_eq!(saved["config"]["password"], KEPT_SECRET);
        assert_eq!(saved["config"]["topic_prefix"], "neurowatch");
        assert_eq!(state.read().await.config.mqtt.password, "s3cret");
        // Sent back as read, the password is kept.
        let read = handler.dispatch("mqtt.get", &json!({})).await.unwrap();
        handler.dispatch("mqtt.set", &read["config"]).await.unwrap();
        assert_eq!(state.read().await.config.mqtt.password, "s3cret");
        let bad = json!({"enabled": true, "host": ""});
        assert!(handler.dispatch("mqtt.set", &bad).await.is_err());
        assert!(state.read().await.config.mqtt.enabled);
    }

    #[tokio::test]
    async fn a_test_reports_an_unreachable_broker() {
        let (handler, _state, _dir) = handler();
        let reply = handler
            .dispatch(
                "mqtt.test",
                &json!({"enabled": true, "host": "127.0.0.1", "port": 1}),
            )
            .await
            .unwrap();
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("Could not reach"));
    }
}
