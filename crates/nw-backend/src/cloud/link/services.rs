//! Hosted services: which of NeuroWatch's services this recorder should use.
//!
//! `POST v1/nodes/{node}/service-tickets` with the usual signed proof
//! (operation `service_tickets`). The portal chooses an instance and signs a
//! short-lived ticket for each service; a service absent from the reply is
//! one this recorder may no longer use.
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use time::Duration as TimeDuration;

use super::backup::{host_port, valid_name};
use super::{
    fmt_time, sign_value, without_signature, CloudError, CloudNode, LinkPhase, Result,
    NODE_AUDIENCE,
};

pub const SERVICE_TICKETS_OPERATION: &str = "service_tickets";
pub const BACKUP_SERVICE: &str = "backup_ingest";
const MAX_TICKET_BYTES: usize = 64 * 1024;
const MAX_SERVICES: usize = 32;
const MAX_CAMERAS: usize = 1024;
/// A reply this recent answers the backup loop and the services loop alike.
const FRESH: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ServiceEndpoint {
    #[serde(default)]
    pub quic: Option<String>,
    #[serde(default)]
    pub tcp: Option<String>,
    #[serde(default)]
    pub tls: Option<String>,
}

/// A service this recorder should use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServiceEntry {
    pub service: String,
    /// The instance's name, also its TLS name.
    pub instance: String,
    pub endpoint: ServiceEndpoint,
    /// The instance's own key (hex): what it must prove it holds.
    #[serde(default)]
    pub public_key: Option<String>,
    /// A development instance's CA (PEM): debug builds only.
    #[serde(default)]
    pub ca_pem: Option<String>,
    pub ticket: String,
    /// A hosted plugin's cameras: the ones its ticket names.
    #[serde(default)]
    pub cameras: Vec<String>,
    /// A hosted plugin's name in the portal's catalog, shown in place of its id.
    #[serde(default)]
    pub label: Option<String>,
}

pub type ServicesCache = std::sync::Mutex<Option<(Instant, Vec<ServiceEntry>)>>;

/// The portal's reply, with anything unusable refused.
pub fn parse_services_reply(reply: &Value) -> Result<Vec<ServiceEntry>> {
    let invalid =
        |what: &str| CloudError::Network(format!("invalid service tickets reply: {what}"));
    let services = reply
        .get("services")
        .ok_or_else(|| invalid("no services"))?;
    let mut entries: Vec<ServiceEntry> =
        serde_json::from_value(services.clone()).map_err(|e| invalid(&e.to_string()))?;
    if entries.len() > MAX_SERVICES {
        return Err(invalid("too many services"));
    }
    for entry in &mut entries {
        if entry.ticket.is_empty() || entry.ticket.len() > MAX_TICKET_BYTES {
            return Err(invalid("ticket"));
        }
        if entry.service.is_empty() || entry.service.len() > 64 || !valid_name(&entry.instance) {
            return Err(invalid("name"));
        }
        if entry.cameras.len() > MAX_CAMERAS || entry.cameras.iter().any(|id| id.len() > 64) {
            return Err(invalid("cameras"));
        }
        let addresses = [
            &entry.endpoint.quic,
            &entry.endpoint.tcp,
            &entry.endpoint.tls,
        ];
        if addresses.iter().all(|address| address.is_none())
            || addresses.into_iter().flatten().any(|a| !host_port(a))
        {
            return Err(invalid("address"));
        }
        if entry
            .public_key
            .as_deref()
            .is_some_and(|key| nw_service_protocol::verifying_key(key).is_none())
        {
            return Err(invalid("public key"));
        }
        if !cfg!(debug_assertions) {
            entry.ca_pem = None;
        }
    }
    Ok(entries)
}

impl CloudNode {
    /// What the portal says this recorder should use now.
    pub async fn hosted_services(&self) -> Result<Vec<ServiceEntry>> {
        let recent = self.services.lock().ok().and_then(|cached| {
            cached
                .as_ref()
                .filter(|(at, _)| at.elapsed() < FRESH)
                .map(|(_, e)| e.clone())
        });
        if let Some(entries) = recent {
            return Ok(entries);
        }
        let outcome = self.request_services().await;
        self.note(&outcome);
        let entries = outcome?;
        if let Ok(mut cached) = self.services.lock() {
            *cached = Some((Instant::now(), entries.clone()));
        }
        Ok(entries)
    }

    async fn request_services(&self) -> Result<Vec<ServiceEntry>> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, SERVICE_TICKETS_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut proof = json!({
            "protocol_version":1,"tenant_id":status.tenant_id,"node_id":node_id,
            "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
            "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
            "challenge":challenge,"signature":""
        });
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        let url = endpoint.join(&format!("v1/nodes/{node_id}/service-tickets"))?;
        let response = self
            .control_post_json(&endpoint, &client, url, &proof)
            .await?;
        self.mark_contact();
        parse_services_reply(&response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(entry: Value) -> Value {
        json!({"services": [entry]})
    }
    fn entry() -> Value {
        json!({"service": "nw.vcount", "instance": "vcount-1.example.test",
               "endpoint": {"quic": "vcount-1.example.test:443"}, "public_key": "ab".repeat(32), "ticket": "a.b"})
    }

    #[test]
    fn an_entry_is_read() {
        let mut key = [0u8; 32];
        key[0] = 1;
        let mut good = entry();
        good["public_key"] = json!(nw_service_protocol::hex(
            ed25519_dalek::SigningKey::from_bytes(&key)
                .verifying_key()
                .as_bytes()
        ));
        let got = parse_services_reply(&reply(good)).unwrap();
        assert_eq!(
            got[0].endpoint.quic.as_deref(),
            Some("vcount-1.example.test:443")
        );
        assert_eq!(
            parse_services_reply(&json!({"services": []})).unwrap(),
            vec![]
        );
        assert!(parse_services_reply(&json!({})).is_err());
    }

    #[test]
    fn anything_unusable_is_refused() {
        let mut key = [0u8; 32];
        key[0] = 1;
        let hex = nw_service_protocol::hex(
            ed25519_dalek::SigningKey::from_bytes(&key)
                .verifying_key()
                .as_bytes(),
        );
        for (field, bad) in [
            ("instance", json!("a b")),
            ("ticket", json!("")),
            ("endpoint", json!({})),
            ("endpoint", json!({"quic": "no-port"})),
            ("public_key", json!("zz")),
        ] {
            let mut candidate = entry();
            candidate["public_key"] = json!(hex);
            candidate[field] = bad.clone();
            assert!(
                parse_services_reply(&reply(candidate)).is_err(),
                "{field}: {bad}"
            );
        }
    }
}
