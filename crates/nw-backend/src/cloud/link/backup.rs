//! Cloud Backup's ticket: the portal says which of this recorder's cameras
//! have a backup plan, and signs a ticket for an ingest to take them
//! (`docs/plans/2026-11-07-cloud-backup.md`).
//!
//! `POST v1/nodes/{node}/backup-tickets` with the usual signed proof
//! (operation `backup_ticket`). The reply is `{"backup": null}` when no
//! camera has a plan, else `{"backup": {"ticket", "ingest": {"name",
//! "quic"?, "tcp"?, "ca_pem"?}}}`; the ticket's claims list the cameras, their
//! windows and limits. `ca_pem` (a development ingest's private CA) is
//! honoured by debug builds only, as the cloud address is.
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use serde_json::{json, Value};
use time::Duration as TimeDuration;

use super::{
    fmt_time, sign_value, without_signature, CloudError, CloudNode, LinkPhase, Result,
    NODE_AUDIENCE,
};

pub const BACKUP_TICKET_OPERATION: &str = "backup_ticket";
/// Largest ticket accepted (as the ingest's limit).
const MAX_TICKET_BYTES: usize = 64 * 1024;

/// Where to back up, and the ticket that lets this recorder.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BackupAssignment {
    pub ticket: String,
    pub ingest: BackupIngest,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BackupIngest {
    /// Its TLS name.
    pub name: String,
    /// `host:port` for QUIC and for TLS over TCP; at least one.
    #[serde(default)]
    pub quic: Option<String>,
    #[serde(default)]
    pub tcp: Option<String>,
    /// A development ingest's CA (PEM): debug builds only.
    #[serde(default)]
    pub ca_pem: Option<String>,
    /// Its catalog key (hex), which it must prove it holds.
    #[serde(default)]
    pub public_key: Option<String>,
}

impl BackupAssignment {
    /// A `service-tickets` entry for the backup ingest.
    pub fn from_service(entry: super::services::ServiceEntry) -> Self {
        Self {
            ticket: entry.ticket,
            ingest: BackupIngest {
                name: entry.instance,
                quic: entry.endpoint.quic,
                tcp: entry.endpoint.tcp,
                ca_pem: entry.ca_pem,
                public_key: entry.public_key,
            },
        }
    }
}

/// The portal's reply: `None` when no camera has a backup plan.
pub fn parse_backup_reply(reply: &Value) -> Result<Option<BackupAssignment>> {
    let invalid = |what: &str| CloudError::Network(format!("invalid backup ticket reply: {what}"));
    let backup = reply
        .get("backup")
        .ok_or_else(|| invalid("no backup field"))?;
    if backup.is_null() {
        return Ok(None);
    }
    let mut assignment: BackupAssignment =
        serde_json::from_value(backup.clone()).map_err(|e| invalid(&e.to_string()))?;
    if assignment.ticket.is_empty() || assignment.ticket.len() > MAX_TICKET_BYTES {
        return Err(invalid("ticket"));
    }
    let name = &assignment.ingest.name;
    if !valid_name(name) {
        return Err(invalid("ingest name"));
    }
    for address in [&assignment.ingest.quic, &assignment.ingest.tcp]
        .into_iter()
        .flatten()
    {
        if !host_port(address) {
            return Err(invalid("ingest address"));
        }
    }
    if assignment.ingest.quic.is_none() && assignment.ingest.tcp.is_none() {
        return Err(invalid("no ingest address"));
    }
    if !cfg!(debug_assertions) {
        assignment.ingest.ca_pem = None;
    }
    Ok(Some(assignment))
}

/// A service instance's name, which is also its TLS name.
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// `host:port` (a name or IPv4, or `[IPv6]`), port 1–65535.
pub(super) fn host_port(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    let port_ok = port.parse::<u16>().is_ok_and(|p| p > 0);
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    port_ok
        && !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':')
}

impl CloudNode {
    /// This recorder's backup assignment, or `None` when no camera has a plan.
    pub async fn backup_assignment(&self) -> Result<Option<BackupAssignment>> {
        let outcome = self.request_backup().await;
        self.note(&outcome);
        outcome
    }

    async fn request_backup(&self) -> Result<Option<BackupAssignment>> {
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
            .node_challenge(&client, &endpoint, &key, &status, BACKUP_TICKET_OPERATION)
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
        let url = endpoint.join(&format!("v1/nodes/{node_id}/backup-tickets"))?;
        let response = self
            .control_post_json(&endpoint, &client, url, &proof)
            .await?;
        self.mark_contact();
        parse_backup_reply(&response)
    }

    /// The recorder's identity key: tickets name its public half, and the
    /// recorder signs each backup connection with it.
    pub fn identity_key(&self) -> Option<SigningKey> {
        self.lock().ok().map(|i| i.key.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(ingest: Value) -> Value {
        json!({"backup": {"ticket": "claims.signature", "ingest": ingest}})
    }

    #[test]
    fn no_plan_is_none() {
        assert_eq!(parse_backup_reply(&json!({"backup": null})).unwrap(), None);
        assert!(
            parse_backup_reply(&json!({})).is_err(),
            "the field must be there"
        );
    }

    #[test]
    fn an_assignment_is_read_and_checked() {
        let got = parse_backup_reply(&reply(json!({"name": "ingest-1.neurowatch.app", "quic": "ingest-1.neurowatch.app:443", "tcp": "203.0.113.9:443"})))
            .unwrap()
            .unwrap();
        assert_eq!(got.ticket, "claims.signature");
        assert_eq!(
            got.ingest.quic.as_deref(),
            Some("ingest-1.neurowatch.app:443")
        );
        assert_eq!(got.ingest.ca_pem, None);
        assert!(
            parse_backup_reply(&reply(json!({"name": "x", "quic": "[::1]:4443"})))
                .unwrap()
                .is_some()
        );
        for bad in [
            json!({"name": "x"}),
            json!({"name": "", "quic": "x:1"}),
            json!({"name": "a b", "quic": "x:1"}),
            json!({"name": "x", "quic": "x"}),
            json!({"name": "x", "quic": "x:0"}),
            json!({"name": "x", "tcp": "x:99999"}),
            json!({"name": "x", "tcp": "/etc/passwd:1"}),
        ] {
            assert!(parse_backup_reply(&reply(bad.clone())).is_err(), "{bad}");
        }
        let long = json!({"backup": {"ticket": "x".repeat(MAX_TICKET_BYTES + 1), "ingest": {"name": "x", "quic": "x:1"}}});
        assert!(parse_backup_reply(&long).is_err());
    }

    #[test]
    fn a_development_ca_is_kept_only_in_debug_builds() {
        let got = parse_backup_reply(&reply(json!({"name": "localhost", "quic": "127.0.0.1:4443", "ca_pem": "-----BEGIN CERTIFICATE-----"})))
            .unwrap()
            .unwrap();
        assert_eq!(got.ingest.ca_pem.is_some(), cfg!(debug_assertions));
    }
}
