//! Signed session identities, bound to fresh nonces and the TLS connection.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginIdentity {
    pub public_key: String,
    pub challenge: String,
    pub signature: String,
    #[serde(default)]
    pub channel_binding: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Authenticate {
    pub public_key: String,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub public_key: String,
    pub permissions: Vec<String>,
}
