//! Config `plugins`: the plugin daemons this node connects to, and which
//! cameras are bound to each.
use nw_analytics_protocol::{address::Address, messages::StreamChoice};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// Largest binding configuration document, serialized.
const MAX_BINDING_CONFIG_BYTES: usize = 64 * 1024;
const MAX_SERVERS: usize = 32;
const MAX_BINDINGS: usize = 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    pub servers: Vec<PluginServer>,
    pub bindings: Vec<PluginBinding>,
}

/// A plugin daemon, by where it listens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginServer {
    /// The node's name for it (bindings refer to it).
    pub id: String,
    /// `unix:/path.sock` or `tcp:127.0.0.1:port` (`nw_analytics_protocol::address`).
    pub address: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<nw_analytics_protocol::auth::Approval>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub credential_revision: u64,
    /// Verified manifest id, learned when the daemon is inspected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Provided by NeuroWatch: set from the portal's signed reply alone, and
    /// not editable here (`cloud::services`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<Managed>,
}

/// Which hosted service a managed plugin server is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Managed {
    pub service: String,
    pub instance: String,
    /// The name customers know it by, from the portal's catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl PluginServer {
    /// The name people see: a hosted plugin's catalog name, else the
    /// plugin's own (`manifest_name`, when it has connected), else the id.
    pub fn shown_name(&self, manifest_name: Option<&str>) -> String {
        self.managed
            .as_ref()
            .and_then(|managed| managed.label.as_deref())
            .or(manifest_name)
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.id)
            .to_owned()
    }
}

/// A camera bound to a plugin, with that binding's configuration document
/// (owned by the node, interpreted by the plugin).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginBinding {
    pub server: String,
    pub camera_id: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default = "empty_object")]
    pub config: Value,
    /// Which of the camera's streams the plugin gets (`None`: its own choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamChoice>,
    /// The plugin's drawing layers shown on the live wall (`None`: the ones
    /// its manifest shows by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_layers: Option<Vec<String>>,
    /// The plugin always decodes this camera itself: the recorder never
    /// offers it decoded pictures (a desktop's or an always-on decoder's).
    /// Off: automatic, it uses the recorder's pictures whenever there are any.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own_decoder: bool,
    /// A hosted plugin no longer covers this camera: off until it does again,
    /// its settings kept. Set only by `cloud::services`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub withdrawn: bool,
}

fn enabled() -> bool {
    true
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

impl PluginsConfig {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.bindings.is_empty()
    }

    /// Refuse a configuration the host could not run; `cameras` are the
    /// configured camera ids.
    pub fn validate<'a>(&self, cameras: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
        let cameras: HashSet<&str> = cameras.into_iter().collect();
        if self.servers.len() > MAX_SERVERS || self.bindings.len() > MAX_BINDINGS {
            return Err(format!(
                "At most {MAX_SERVERS} plugins and {MAX_BINDINGS} bindings"
            ));
        }
        let mut ids = HashSet::new();
        let mut addresses = HashSet::new();
        let mut plugins = HashSet::new();
        let mut keys = HashSet::new();
        for server in &self.servers {
            if !valid_id(&server.id) {
                return Err(format!(
                    "Plugin id {:?}: use 1–64 letters, digits, '.', '-' or '_'",
                    server.id
                ));
            }
            if !ids.insert(server.id.as_str()) {
                return Err(format!("Two plugins are named {:?}", server.id));
            }
            let address = Address::parse(&server.address)
                .map_err(|error| format!("Plugin {:?}: {error}", server.id))?;
            if !addresses.insert(super::discovery::address_key(&address.to_string())?) {
                return Err("This plugin address has already been added".into());
            }
            if server.managed.is_some()
                && !matches!(address, Address::Quic { .. } | Address::Tls { .. })
            {
                return Err("A hosted service must use an encrypted address".into());
            }
            if let Some(id) = &server.plugin_id {
                if !valid_id(id) {
                    return Err("Invalid plugin manifest id".into());
                }
                if !plugins.insert(id) {
                    return Err(format!("Plugin {id:?} has already been added"));
                }
            }
            if let Some(approval) = &server.approval {
                if !keys.insert(&approval.public_key) {
                    return Err("This plugin identity has already been added".into());
                }
                if nw_analytics_sdk::identity::unhex::<32>(&approval.public_key).is_none()
                    || approval.permissions.len() > 16
                    || approval.permissions.iter().any(|permission| {
                        ![
                            "video",
                            "audio",
                            "decoded_frames",
                            "events",
                            "overlays",
                            "web_ui",
                        ]
                        .contains(&permission.as_str())
                    })
                {
                    return Err("Invalid plugin approval".into());
                }
            }
        }
        let mut bound = HashSet::new();
        for binding in &self.bindings {
            if !ids.contains(binding.server.as_str()) {
                return Err(format!(
                    "A binding names plugin {:?}, which is not configured",
                    binding.server
                ));
            }
            if !cameras.contains(binding.camera_id.as_str()) {
                return Err(format!(
                    "A binding names camera {:?}, which does not exist",
                    binding.camera_id
                ));
            }
            if !bound.insert((binding.server.as_str(), binding.camera_id.as_str())) {
                return Err(format!(
                    "Camera {:?} is bound to plugin {:?} twice",
                    binding.camera_id, binding.server
                ));
            }
            if binding.wall_layers.as_ref().is_some_and(|layers| {
                layers.len() > 32 || layers.iter().any(|layer| !valid_id(layer))
            }) {
                return Err("A binding names at most 32 drawing layers".into());
            }
            if !binding.config.is_object() {
                return Err("A binding's configuration must be an object".into());
            }
            if serde_json::to_vec(&binding.config)
                .map_or(true, |b| b.len() > MAX_BINDING_CONFIG_BYTES)
            {
                return Err("A binding's configuration is limited to 64 KiB".into());
            }
        }
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(value: Value) -> PluginsConfig {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_minimal_configuration_takes_defaults() {
        let plugins = config(json!({
            "servers": [{"id": "motion", "address": "unix:/tmp/m.sock"}],
            "bindings": [{"server": "motion", "camera_id": "cam-1"}]
        }));
        assert!(plugins.servers[0].enabled);
        assert!(plugins.bindings[0].enabled);
        assert_eq!(plugins.bindings[0].config, json!({}));
        assert_eq!(plugins.bindings[0].stream, None);
        assert_eq!(plugins.bindings[0].wall_layers, None);
        let chosen = config(json!({
            "servers": [{"id": "motion", "address": "unix:/tmp/m.sock"}],
            "bindings": [{"server": "motion", "camera_id": "cam-1", "stream": "main",
                          "wall_layers": ["detections"]}]
        }));
        assert_eq!(chosen.bindings[0].stream, Some(StreamChoice::Main));
        assert!(chosen.validate(["cam-1"]).is_ok());
        assert!(plugins.validate(["cam-1"]).is_ok());
        assert!(PluginsConfig::default().is_empty());
    }

    #[test]
    fn invalid_configurations_are_refused() {
        let server = json!({"id": "motion", "address": "unix:/tmp/m.sock"});
        for bad in [
            json!({"servers": [{"id": "", "address": "unix:/x"}]}),
            json!({"servers": [{"id": "a b", "address": "unix:/x"}]}),
            json!({"servers": [server, server]}),
            json!({"servers": [{"id": "m", "address": "tcp:10.0.0.2:1"}]}),
            json!({"servers": [server], "bindings": [{"server": "other", "camera_id": "cam-1"}]}),
            json!({"servers": [server], "bindings": [{"server": "motion", "camera_id": "gone"}]}),
            json!({"servers": [server], "bindings": [{"server": "motion", "camera_id": "cam-1"},
                                                     {"server": "motion", "camera_id": "cam-1"}]}),
            json!({"servers": [server], "bindings": [{"server": "motion", "camera_id": "cam-1", "config": 3}]}),
            json!({"servers": [server], "bindings": [{"server": "motion", "camera_id": "cam-1", "wall_layers": ["a b"]}]}),
        ] {
            assert!(config(bad.clone()).validate(["cam-1"]).is_err(), "{bad}");
        }
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn one_plugin_cannot_be_saved_twice_by_address_manifest_or_key() {
        for servers in [
            json!([{"id":"a","address":"tls://EXAMPLE.com:443"},{"id":"b","address":"tls:example.com:443"}]),
            json!([{"id":"a","address":"unix:/a","plugin_id":"nw.motion"},{"id":"b","address":"unix:/b","plugin_id":"nw.motion"}]),
            json!([{"id":"a","address":"unix:/a","approval":{"public_key":"01".repeat(32),"permissions":[]}},{"id":"b","address":"unix:/b","approval":{"public_key":"01".repeat(32),"permissions":[]}}]),
        ] {
            let config: PluginsConfig = serde_json::from_value(json!({"servers":servers})).unwrap();
            assert!(config
                .validate([])
                .unwrap_err()
                .contains("already been added"));
        }
    }
}
