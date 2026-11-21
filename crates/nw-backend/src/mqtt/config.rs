//! MQTT settings (`config.json` `mqtt`): the broker, its login, the topics
//! and Home Assistant discovery.
//!
//! The password is stored like alert channel secrets: in the private config
//! file only. `mqtt.get` never returns it ([`MqttConfig::masked`]); a client
//! sends [`KEPT_SECRET`] back to keep the stored one.

use serde::{Deserialize, Serialize};

pub use crate::alerts::KEPT_SECRET;

pub const DEFAULT_PORT: u16 = 1883;
pub const DEFAULT_TLS_PORT: u16 = 8883;
pub const DEFAULT_PREFIX: &str = "neurowatch";
pub const DEFAULT_DISCOVERY_PREFIX: &str = "homeassistant";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MqttConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The broker's host name or address, without a scheme.
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Encrypt the connection (MQTT over TLS, checked against the public
    /// certificate authorities).
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Every topic starts with this, e.g. `neurowatch/front_door/person`.
    /// Give each recorder on the same broker its own.
    #[serde(default = "default_prefix")]
    pub topic_prefix: String,
    /// Announce cameras to Home Assistant (MQTT discovery).
    #[serde(default = "yes")]
    pub discovery: bool,
    #[serde(default = "default_discovery_prefix")]
    pub discovery_prefix: String,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_prefix() -> String {
    DEFAULT_PREFIX.into()
}
fn default_discovery_prefix() -> String {
    DEFAULT_DISCOVERY_PREFIX.into()
}
fn yes() -> bool {
    true
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: DEFAULT_PORT,
            tls: false,
            username: String::new(),
            password: String::new(),
            topic_prefix: default_prefix(),
            discovery: true,
            discovery_prefix: default_discovery_prefix(),
        }
    }
}

impl MqttConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The settings with the password replaced by [`KEPT_SECRET`].
    pub fn masked(&self) -> Self {
        let mut masked = self.clone();
        if !masked.password.is_empty() {
            masked.password = KEPT_SECRET.into();
        }
        masked
    }

    /// `changes` checked, with a password left as [`KEPT_SECRET`] taken from
    /// these (stored) settings.
    pub fn replaced_by(&self, mut changes: MqttConfig) -> Result<MqttConfig, String> {
        changes.host = changes.host.trim().to_owned();
        if changes.password == KEPT_SECRET {
            // The stored password goes only to the broker it was entered for
            // (security review 2026-11-20, finding 6).
            if !self.password.is_empty() && !changes.host.eq_ignore_ascii_case(self.host.trim()) {
                return Err("Enter the broker's password again for the new address".into());
            }
            changes.password = self.password.clone();
        }
        changes.topic_prefix = changes.topic_prefix.trim().to_owned();
        changes.discovery_prefix = changes.discovery_prefix.trim().to_owned();
        changes.validate()?;
        Ok(changes)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.host.is_empty() {
            return Err("Enter the MQTT broker's address".into());
        }
        if !self.host.is_empty() && !valid_host(&self.host) {
            return Err(
                "The broker's address is a host name or IP address, without mqtt:// or a port"
                    .into(),
            );
        }
        if self.port == 0 {
            return Err("The broker's port is 1–65535".into());
        }
        for (name, value) in [("user name", &self.username), ("password", &self.password)] {
            if value.chars().count() > 200 || value.chars().any(char::is_control) {
                return Err(format!(
                    "The MQTT {name} is at most 200 printable characters"
                ));
            }
        }
        valid_prefix(&self.topic_prefix).map_err(|e| format!("Topic prefix: {e}"))?;
        valid_prefix(&self.discovery_prefix)
            .map_err(|e| format!("Home Assistant discovery prefix: {e}"))?;
        Ok(())
    }
}

fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
        && !host.contains("://")
        // A port goes in its own field; an IPv6 address is written in brackets.
        && (host.starts_with('[') || !host.contains(':'))
}

/// 1–64 characters of letters, digits, `_`, `-` and inner `/`; no MQTT
/// wildcards, no empty levels.
fn valid_prefix(prefix: &str) -> Result<(), String> {
    if prefix.is_empty() || prefix.len() > 64 {
        return Err("use 1–64 characters".into());
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'))
    {
        return Err("use letters, digits, _, - and /".into());
    }
    if prefix.split('/').any(str::is_empty) {
        return Err("no empty level (// or a / at either end)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> MqttConfig {
        MqttConfig {
            enabled: true,
            host: "broker.local".into(),
            username: "nw".into(),
            password: "s3cret".into(),
            ..MqttConfig::default()
        }
    }

    #[test]
    fn a_kept_password_never_goes_to_another_broker() {
        // Security review 2026-11-20, finding 6.
        let stored = enabled();
        let moved = MqttConfig {
            host: "broker.attacker.example".into(),
            ..stored.masked()
        };
        let refused = stored.replaced_by(moved).unwrap_err();
        assert!(refused.contains("again for the new address"), "{refused}");
        let retyped = MqttConfig {
            host: "broker.attacker.example".into(),
            password: "typed".into(),
            ..stored.masked()
        };
        assert_eq!(stored.replaced_by(retyped).unwrap().password, "typed");
        let same = MqttConfig {
            host: " BROKER.local ".into(),
            ..stored.masked()
        };
        assert_eq!(stored.replaced_by(same).unwrap().password, "s3cret");
    }

    #[test]
    fn the_password_is_never_returned_and_kept_when_sent_back_masked() {
        let stored = enabled();
        let masked = stored.masked();
        assert_eq!(masked.password, KEPT_SECRET);
        let saved = stored.replaced_by(masked).unwrap();
        assert_eq!(saved.password, "s3cret");
        let changed = stored
            .replaced_by(MqttConfig {
                password: "new".into(),
                ..enabled()
            })
            .unwrap();
        assert_eq!(changed.password, "new");
        assert_eq!(MqttConfig::default().masked().password, "");
    }

    #[test]
    fn settings_are_checked() {
        let bad = |change: fn(&mut MqttConfig)| {
            let mut config = enabled();
            change(&mut config);
            config.validate().is_err()
        };
        assert!(enabled().validate().is_ok());
        assert!(bad(|c| c.host.clear()));
        assert!(bad(|c| c.host = "mqtt://broker".into()));
        assert!(bad(|c| c.host = "broker:1883".into()));
        assert!(bad(|c| c.port = 0));
        assert!(bad(|c| c.topic_prefix = "home/#".into()));
        assert!(bad(|c| c.topic_prefix = "/neurowatch".into()));
        assert!(bad(|c| c.discovery_prefix = String::new()));
        assert!(!bad(|c| c.topic_prefix = "site-a/neurowatch".into()));
        assert!(!bad(|c| c.host = "[fd00::1]".into()));
        // Switched off, nothing is required.
        assert!(MqttConfig::default().validate().is_ok());
    }

    #[test]
    fn a_stored_document_takes_defaults_for_what_it_leaves_out() {
        let config: MqttConfig =
            serde_json::from_value(serde_json::json!({"enabled": true, "host": "h"})).unwrap();
        assert_eq!(config.port, 1883);
        assert_eq!(config.topic_prefix, "neurowatch");
        assert!(config.discovery);
        assert_eq!(config.discovery_prefix, "homeassistant");
    }
}
