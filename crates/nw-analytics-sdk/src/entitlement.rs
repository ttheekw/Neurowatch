//! A hosted plugin's check of the portal's ticket: signed by a key the root
//! vouches for, for this plugin and instance, and presented by the recorder
//! it names on this very connection. A self-hosted plugin never uses this.
pub use nw_service_protocol::{Grants, ServiceTicket, Trust};

use nw_analytics_protocol::messages::Entitlement;
use nw_service_protocol::{entitlement_payload, verify_hex};
use std::sync::{Arc, RwLock};

/// Who this daemon is, and what it trusts (kept current by the daemon).
#[derive(Clone)]
pub struct EntitlementGate {
    pub trust: Arc<RwLock<Trust>>,
    /// The plugin's manifest id.
    pub service: String,
    /// This instance's name in the catalog.
    pub audience: String,
}

impl EntitlementGate {
    /// The ticket's claims, or why it is refused. `current`: the session's
    /// ticket when this is a renewal, which must be for the same recorder.
    pub fn check(
        &self,
        message: &Entitlement,
        channel_binding: &str,
        current: Option<&ServiceTicket>,
        now_ms: i64,
    ) -> Result<ServiceTicket, String> {
        let trust = self
            .trust
            .read()
            .map_err(|_| "The plugin's key list is unavailable".to_owned())?;
        let ticket = trust
            .verify_ticket(&message.ticket, &self.service, &self.audience, now_ms)
            .map_err(|error| format!("The service ticket was refused: {error}"))?;
        let payload = entitlement_payload(&message.ticket, channel_binding.as_bytes());
        let key = ticket
            .node_key()
            .ok_or("The ticket names no recorder key")?;
        if !verify_hex(&key, &payload, &message.signature) {
            return Err("The recorder did not prove it holds this ticket".into());
        }
        if current
            .is_some_and(|held| held.org_id != ticket.org_id || held.node_id != ticket.node_id)
        {
            return Err("A renewal must be for the same recorder".into());
        }
        Ok(ticket)
    }

    /// Whether a recorder holding `ticket` has been revoked since it connected.
    pub fn revoked(&self, ticket: &ServiceTicket) -> bool {
        self.trust
            .read()
            .map_or(true, |trust| trust.is_revoked(&ticket.node_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use nw_service_protocol::{
        hex, seal, sign_hex, KeyList, ListedKey, KEY_LIST_DOMAIN, PURPOSE_TICKETS,
        SERVICE_TICKET_DOMAIN,
    };

    const NOW: i64 = 1_790_000_000_000;

    fn gate() -> (EntitlementGate, SigningKey) {
        let root = SigningKey::from_bytes(&[1; 32]);
        let tickets = SigningKey::from_bytes(&[2; 32]);
        let mut trust = Trust::new(root.verifying_key());
        let list = KeyList {
            v: 1,
            sequence: 1,
            issued_at_ms: NOW,
            expires_at_ms: NOW + 86_400_000,
            keys: vec![ListedKey {
                kid: "t".into(),
                purpose: PURPOSE_TICKETS.into(),
                public_key: hex(tickets.verifying_key().as_bytes()),
            }],
        };
        trust
            .accept_key_list(&seal(KEY_LIST_DOMAIN, &list, &root), NOW)
            .unwrap();
        (
            EntitlementGate {
                trust: Arc::new(RwLock::new(trust)),
                service: "nw.counter".into(),
                audience: "counter-1".into(),
            },
            tickets,
        )
    }

    fn present(tickets: &SigningKey, node: &SigningKey, channel: &str) -> Entitlement {
        let ticket = seal(
            SERVICE_TICKET_DOMAIN,
            &ServiceTicket {
                v: 1,
                id: "t".into(),
                kid: "t".into(),
                service: "nw.counter".into(),
                audience: "counter-1".into(),
                org_id: "org".into(),
                node_id: "node".into(),
                node_key: hex(node.verifying_key().as_bytes()),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 900_000,
                grants: Grants {
                    cameras: vec!["cam-1".into()],
                    options: serde_json::Value::Null,
                },
            },
            tickets,
        );
        let signature = sign_hex(node, &entitlement_payload(&ticket, channel.as_bytes()));
        Entitlement { ticket, signature }
    }

    #[test]
    fn the_recorder_named_on_this_connection_is_accepted() {
        let (gate, tickets) = gate();
        let node = SigningKey::from_bytes(&[4; 32]);
        let good = present(&tickets, &node, "channel");
        assert_eq!(
            gate.check(&good, "channel", None, NOW)
                .unwrap()
                .grants
                .cameras,
            ["cam-1"]
        );
    }

    #[test]
    fn a_ticket_copied_to_another_connection_is_refused() {
        let (gate, tickets) = gate();
        let node = SigningKey::from_bytes(&[4; 32]);
        let good = present(&tickets, &node, "channel");
        assert!(gate.check(&good, "another", None, NOW).is_err());
    }

    #[test]
    fn someone_elses_ticket_is_refused() {
        let (gate, tickets) = gate();
        let node = SigningKey::from_bytes(&[4; 32]);
        let thief = SigningKey::from_bytes(&[5; 32]);
        let mut stolen = present(&tickets, &node, "channel");
        stolen.signature = sign_hex(
            &thief,
            &entitlement_payload(&stolen.ticket, "channel".as_bytes()),
        );
        assert!(gate.check(&stolen, "channel", None, NOW).is_err());
    }
}
