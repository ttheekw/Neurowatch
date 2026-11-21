//! What NeuroWatch's portal, recorders and hosted services agree on: signed
//! service tickets, the root-signed key list and the revocations. No I/O.
//!
//! Every token is `base64url(JSON claims) "." base64url(Ed25519 signature)`,
//! the signature covering a domain prefix and the JSON bytes exactly as sent,
//! so no side has to reproduce another's JSON.
//!
//! A service pins one thing, the offline root's public key ([`Trust::new`]). The
//! [key list](KeyList) signed by the root says which signing keys to accept
//! for each purpose, so a hacked portal cannot introduce a key of its own. A
//! list's `sequence` only grows, so a dropped key cannot be replayed back in.
//! [Revocations](Revocations) are signed online with a key the list names:
//! they can only take access away.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const SERVICE_TICKET_DOMAIN: &[u8] = b"nw-service/1 ticket\0";
pub const KEY_LIST_DOMAIN: &[u8] = b"nw-service/1 key-list\0";
pub const REVOCATIONS_DOMAIN: &[u8] = b"nw-service/1 revocations\0";
const ENTITLEMENT_DOMAIN: &[u8] = b"nw-service/1 entitlement\0";
const INSTANCE_PROOF_DOMAIN: &[u8] = b"nw-service/1 instance\0";
const REPORT_DOMAIN: &[u8] = b"nw-service/1 report\0";

pub const PURPOSE_TICKETS: &str = "tickets";
pub const PURPOSE_LINKS: &str = "links";
pub const PURPOSE_REVOCATIONS: &str = "revocations";

pub const VERSION: u32 = 1;
/// Clock difference tolerated between portal, service and recorder.
pub const CLOCK_SKEW_MS: i64 = 60_000;
/// Longest service ticket accepted: they are renewed every few minutes.
pub const MAX_TICKET_LIFETIME_MS: i64 = 3_600_000;
/// Largest token text accepted.
pub const MAX_TOKEN_BYTES: usize = 256 * 1024;
const MAX_LISTED_KEYS: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrustError {
    #[error("the token is malformed")]
    Malformed,
    #[error("the signature is not a trusted key's")]
    Signature,
    #[error("unsupported version {0}")]
    Version(u32),
    #[error("it has expired")]
    Expired,
    #[error("it is not valid yet")]
    NotYetValid,
    #[error("it is valid for too long")]
    TooLong,
    #[error("it is for another instance")]
    Audience,
    #[error("it is for another service")]
    Service,
    #[error("it names no valid recorder key")]
    NodeKey,
    #[error("this recorder has been revoked")]
    Revoked,
    #[error("an older list than the one held")]
    Older,
    #[error("the list names a malformed key")]
    BadKey,
}

/// One signing key the root vouches for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedKey {
    pub kid: String,
    /// [`PURPOSE_TICKETS`], [`PURPOSE_LINKS`] or [`PURPOSE_REVOCATIONS`].
    pub purpose: String,
    /// Hex.
    pub public_key: String,
}

/// Signed by the offline root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyList {
    pub v: u32,
    pub sequence: u64,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub keys: Vec<ListedKey>,
}

/// Recorders that lost their link recently, signed with a revocations key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revocations {
    pub v: u32,
    #[serde(default)]
    pub kid: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    /// Hex public keys.
    pub node_keys: Vec<String>,
}

/// What a ticket allows on a service.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Grants {
    /// The recorder's own ids of the cameras this ticket is for, and no others.
    pub cameras: Vec<String>,
    #[serde(default)]
    pub options: Value,
}

impl Grants {
    /// Whether this ticket covers the camera a recorder wants to attach.
    pub fn allows(&self, camera_id: &str) -> bool {
        self.cameras.iter().any(|camera| camera == camera_id)
    }
}

/// The portal's permission for one recorder to use one service instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceTicket {
    pub v: u32,
    pub id: String,
    #[serde(default)]
    pub kid: String,
    pub service: String,
    /// The instance this ticket is for.
    pub audience: String,
    pub org_id: String,
    pub node_id: String,
    /// The recorder's Ed25519 public key (hex).
    pub node_key: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub grants: Grants,
}

impl ServiceTicket {
    pub fn node_key(&self) -> Option<VerifyingKey> {
        VerifyingKey::from_bytes(&unhex::<32>(&self.node_key)?).ok()
    }
}

/// What this service trusts now: the pinned root, the newest key list it has
/// seen and the newest revocations.
#[derive(Debug, Clone)]
pub struct Trust {
    root: VerifyingKey,
    list: Option<KeyList>,
    signed_list: Option<String>,
    revoked: HashSet<String>,
    revocations_issued_ms: i64,
}

impl Trust {
    pub fn new(root: VerifyingKey) -> Self {
        Self {
            root,
            list: None,
            signed_list: None,
            revoked: HashSet::new(),
            revocations_issued_ms: i64::MIN,
        }
    }

    /// Take a key list if the root signed it and it is not older than the
    /// one held. `Ok(true)`: it is new.
    pub fn accept_key_list(&mut self, text: &str, now_ms: i64) -> Result<bool, TrustError> {
        let claims = open(text, KEY_LIST_DOMAIN, std::slice::from_ref(&self.root))?;
        let list: KeyList = serde_json::from_slice(&claims).map_err(|_| TrustError::Malformed)?;
        if list.v != VERSION {
            return Err(TrustError::Version(list.v));
        }
        if now_ms.saturating_sub(CLOCK_SKEW_MS) >= list.expires_at_ms {
            return Err(TrustError::Expired);
        }
        if list.keys.is_empty() || list.keys.len() > MAX_LISTED_KEYS {
            return Err(TrustError::BadKey);
        }
        if list
            .keys
            .iter()
            .any(|key| verifying_key(&key.public_key).is_none())
        {
            return Err(TrustError::BadKey);
        }
        if let Some(held) = &self.list {
            if list.sequence < held.sequence {
                return Err(TrustError::Older);
            }
            if list.sequence == held.sequence {
                return Ok(false);
            }
        }
        self.list = Some(list);
        self.signed_list = Some(text.trim().to_owned());
        Ok(true)
    }

    /// The list as signed, to keep on disk across restarts (it is checked
    /// against the root again when read back).
    pub fn signed_key_list(&self) -> Option<&str> {
        self.signed_list.as_deref()
    }

    pub fn sequence(&self) -> Option<u64> {
        self.list.as_ref().map(|list| list.sequence)
    }

    /// The keys accepted for `purpose` now: none once the list has expired.
    pub fn keys(&self, purpose: &str, now_ms: i64) -> Vec<VerifyingKey> {
        match &self.list {
            Some(list) if now_ms.saturating_sub(CLOCK_SKEW_MS) < list.expires_at_ms => list
                .keys
                .iter()
                .filter(|key| key.purpose == purpose)
                .filter_map(|key| verifying_key(&key.public_key))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Take a newer revocation list. An older one is ignored, and the last
    /// one stays in force if the portal cannot be reached.
    pub fn accept_revocations(&mut self, text: &str, now_ms: i64) -> Result<(), TrustError> {
        let keys = self.keys(PURPOSE_REVOCATIONS, now_ms);
        let claims = open(text, REVOCATIONS_DOMAIN, &keys)?;
        let list: Revocations =
            serde_json::from_slice(&claims).map_err(|_| TrustError::Malformed)?;
        if list.v != VERSION {
            return Err(TrustError::Version(list.v));
        }
        if now_ms.saturating_sub(CLOCK_SKEW_MS) >= list.expires_at_ms {
            return Err(TrustError::Expired);
        }
        if list.issued_at_ms > self.revocations_issued_ms {
            self.revocations_issued_ms = list.issued_at_ms;
            self.revoked = list
                .node_keys
                .iter()
                .map(|key| key.to_ascii_lowercase())
                .collect();
        }
        Ok(())
    }

    pub fn is_revoked(&self, node_key: &str) -> bool {
        self.revoked.contains(&node_key.to_ascii_lowercase())
    }

    /// Check a service ticket: signed by a listed tickets key, for this
    /// service and instance, current, naming a recorder not revoked.
    pub fn verify_ticket(
        &self,
        text: &str,
        service: &str,
        audience: &str,
        now_ms: i64,
    ) -> Result<ServiceTicket, TrustError> {
        let keys = self.keys(PURPOSE_TICKETS, now_ms);
        let claims = open(text, SERVICE_TICKET_DOMAIN, &keys)?;
        let ticket: ServiceTicket =
            serde_json::from_slice(&claims).map_err(|_| TrustError::Malformed)?;
        if ticket.v != VERSION {
            return Err(TrustError::Version(ticket.v));
        }
        if ticket.service != service {
            return Err(TrustError::Service);
        }
        if ticket.audience != audience {
            return Err(TrustError::Audience);
        }
        if ticket.expires_at_ms.saturating_sub(ticket.issued_at_ms) > MAX_TICKET_LIFETIME_MS {
            return Err(TrustError::TooLong);
        }
        if now_ms.saturating_add(CLOCK_SKEW_MS) < ticket.issued_at_ms {
            return Err(TrustError::NotYetValid);
        }
        if now_ms.saturating_sub(CLOCK_SKEW_MS) >= ticket.expires_at_ms {
            return Err(TrustError::Expired);
        }
        if ticket.node_key().is_none() {
            return Err(TrustError::NodeKey);
        }
        if self.is_revoked(&ticket.node_key) {
            return Err(TrustError::Revoked);
        }
        Ok(ticket)
    }
}

/// A token's claims, once one of `keys` is shown to have signed it.
fn open(text: &str, domain: &[u8], keys: &[VerifyingKey]) -> Result<Vec<u8>, TrustError> {
    let text = text.trim();
    if text.len() > MAX_TOKEN_BYTES {
        return Err(TrustError::Malformed);
    }
    let (claims, signature) = text.split_once('.').ok_or(TrustError::Malformed)?;
    let claims = URL_SAFE_NO_PAD
        .decode(claims)
        .map_err(|_| TrustError::Malformed)?;
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(TrustError::Malformed)?;
    let signature = Signature::from_bytes(&signature);
    let mut signed = domain.to_vec();
    signed.extend_from_slice(&claims);
    if keys
        .iter()
        .any(|key| key.verify_strict(&signed, &signature).is_ok())
    {
        Ok(claims)
    } else {
        Err(TrustError::Signature)
    }
}

/// Sign a token (the portal does this; here for tests and tools).
pub fn seal(domain: &[u8], claims: &impl Serialize, key: &SigningKey) -> String {
    let claims = serde_json::to_vec(claims).unwrap_or_default();
    let mut signed = domain.to_vec();
    signed.extend_from_slice(&claims);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&claims),
        URL_SAFE_NO_PAD.encode(key.sign(&signed).to_bytes())
    )
}

/// A ticket's claims without checking its signature: what a recorder reads
/// from the ticket it was given (the service checks it).
pub fn read_ticket(text: &str) -> Option<ServiceTicket> {
    let (claims, _) = text.split_once('.')?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()
}

fn bound(domain: &[u8], ticket: &str, channel_binding: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(domain.len() + 8 + ticket.len() + channel_binding.len());
    payload.extend_from_slice(domain);
    payload.extend_from_slice(&(ticket.len() as u32).to_le_bytes());
    payload.extend_from_slice(ticket.as_bytes());
    payload.extend_from_slice(&(channel_binding.len() as u32).to_le_bytes());
    payload.extend_from_slice(channel_binding);
    payload
}

/// What a recorder signs to present a ticket on this connection, so a ticket
/// copied from one connection is useless on another.
pub fn entitlement_payload(ticket: &str, channel_binding: &[u8]) -> Vec<u8> {
    bound(ENTITLEMENT_DOMAIN, ticket, channel_binding)
}

/// What an instance signs to show a recorder it holds its catalog key.
pub fn instance_proof_payload(ticket: &str, channel_binding: &[u8]) -> Vec<u8> {
    bound(INSTANCE_PROOF_DOMAIN, ticket, channel_binding)
}

/// What an instance signs to report to the portal (headers
/// `X-NW-Service-Instance`, `-Time` and `-Signature`).
pub fn report_payload(
    instance: &str,
    time_ms: i64,
    method: &str,
    path: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut payload = REPORT_DOMAIN.to_vec();
    payload.extend_from_slice(format!("{instance}\n{time_ms}\n{method}\n{path}\n").as_bytes());
    payload.extend_from_slice(body);
    payload
}

/// A signature, hex.
pub fn sign_hex(key: &SigningKey, payload: &[u8]) -> String {
    hex(&key.sign(payload).to_bytes())
}

/// Whether `signature` (hex) is `key`'s of `payload`.
pub fn verify_hex(key: &VerifyingKey, payload: &[u8], signature: &str) -> bool {
    unhex::<64>(signature).is_some_and(|signature| {
        key.verify_strict(payload, &Signature::from_bytes(&signature))
            .is_ok()
    })
}

pub fn verifying_key(text: &str) -> Option<VerifyingKey> {
    VerifyingKey::from_bytes(&unhex::<32>(text)?).ok()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.trim();
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000_000;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn listed(kid: &str, purpose: &str, signer: &SigningKey) -> ListedKey {
        ListedKey {
            kid: kid.into(),
            purpose: purpose.into(),
            public_key: hex(signer.verifying_key().as_bytes()),
        }
    }
    fn list(sequence: u64, keys: Vec<ListedKey>) -> KeyList {
        KeyList {
            v: 1,
            sequence,
            issued_at_ms: NOW,
            expires_at_ms: NOW + 86_400_000,
            keys,
        }
    }
    fn ticket(signer_key: &SigningKey) -> ServiceTicket {
        ServiceTicket {
            v: 1,
            id: "t".into(),
            kid: "k".into(),
            service: "nw.counter".into(),
            audience: "counter-1".into(),
            org_id: "org".into(),
            node_id: "node".into(),
            node_key: hex(signer_key.verifying_key().as_bytes()),
            issued_at_ms: NOW,
            expires_at_ms: NOW + 900_000,
            grants: Grants {
                cameras: vec!["cam-1".into(), "cam-2".into()],
                options: Value::Null,
            },
        }
    }
    fn trust() -> Trust {
        let mut trust = Trust::new(key(1).verifying_key());
        let signed = seal(
            KEY_LIST_DOMAIN,
            &list(
                5,
                vec![
                    listed("t", PURPOSE_TICKETS, &key(2)),
                    listed("r", PURPOSE_REVOCATIONS, &key(3)),
                ],
            ),
            &key(1),
        );
        assert_eq!(trust.accept_key_list(&signed, NOW), Ok(true));
        trust
    }

    #[test]
    fn only_the_root_can_name_keys() {
        let mut trust = Trust::new(key(1).verifying_key());
        let forged = seal(
            KEY_LIST_DOMAIN,
            &list(1, vec![listed("t", PURPOSE_TICKETS, &key(9))]),
            &key(9),
        );
        assert_eq!(
            trust.accept_key_list(&forged, NOW),
            Err(TrustError::Signature)
        );
        assert!(trust.keys(PURPOSE_TICKETS, NOW).is_empty());
    }

    #[test]
    fn an_older_list_cannot_bring_a_dropped_key_back() {
        let mut trust = trust();
        let old = seal(
            KEY_LIST_DOMAIN,
            &list(4, vec![listed("t", PURPOSE_TICKETS, &key(9))]),
            &key(1),
        );
        assert_eq!(trust.accept_key_list(&old, NOW), Err(TrustError::Older));
        assert_eq!(trust.sequence(), Some(5));
    }

    #[test]
    fn an_expired_list_trusts_nothing() {
        let trust = trust();
        assert_eq!(trust.keys(PURPOSE_TICKETS, NOW).len(), 1);
        assert!(trust.keys(PURPOSE_TICKETS, NOW + 2 * 86_400_000).is_empty());
    }

    #[test]
    fn a_ticket_is_checked_against_keys_of_its_purpose() {
        let trust = trust();
        let node = key(4);
        let good = seal(SERVICE_TICKET_DOMAIN, &ticket(&node), &key(2));
        let checked = trust.verify_ticket(&good, "nw.counter", "counter-1", NOW + 1000);
        assert_eq!(checked.map(|t| t.grants.cameras.len()), Ok(2));
        let revocation_key = seal(SERVICE_TICKET_DOMAIN, &ticket(&node), &key(3));
        assert_eq!(
            trust
                .verify_ticket(&revocation_key, "nw.counter", "counter-1", NOW)
                .unwrap_err(),
            TrustError::Signature,
            "a revocations key signs no tickets"
        );
        for (service, audience, now, error) in [
            ("other", "counter-1", NOW, TrustError::Service),
            ("nw.counter", "counter-2", NOW, TrustError::Audience),
            (
                "nw.counter",
                "counter-1",
                NOW + 3_600_000,
                TrustError::Expired,
            ),
            (
                "nw.counter",
                "counter-1",
                NOW - 3_600_000,
                TrustError::NotYetValid,
            ),
        ] {
            assert_eq!(
                trust
                    .verify_ticket(&good, service, audience, now)
                    .unwrap_err(),
                error
            );
        }
    }

    #[test]
    fn extreme_times_are_refused_without_overflow() {
        let trust = trust();
        let mut odd = ticket(&key(4));
        odd.issued_at_ms = i64::MIN;
        odd.expires_at_ms = i64::MAX;
        let odd = seal(SERVICE_TICKET_DOMAIN, &odd, &key(2));
        assert_eq!(
            trust
                .verify_ticket(&odd, "nw.counter", "counter-1", NOW)
                .unwrap_err(),
            TrustError::TooLong
        );
        let good = seal(SERVICE_TICKET_DOMAIN, &ticket(&key(4)), &key(2));
        for now in [i64::MIN, i64::MAX] {
            assert!(trust
                .verify_ticket(&odd, "nw.counter", "counter-1", now)
                .is_err());
            assert!(trust
                .verify_ticket(&good, "nw.counter", "counter-1", now)
                .is_err());
        }
    }

    #[test]
    fn a_long_ticket_is_refused() {
        let trust = trust();
        let mut long = ticket(&key(4));
        long.expires_at_ms = NOW + MAX_TICKET_LIFETIME_MS + 1;
        let signed = seal(SERVICE_TICKET_DOMAIN, &long, &key(2));
        assert_eq!(
            trust
                .verify_ticket(&signed, "nw.counter", "counter-1", NOW)
                .unwrap_err(),
            TrustError::TooLong
        );
    }

    #[test]
    fn a_revoked_recorder_is_refused_and_older_revocations_are_ignored() {
        let mut trust = trust();
        let node = key(4);
        let revocations = |issued, node_keys| Revocations {
            v: 1,
            kid: "r".into(),
            issued_at_ms: issued,
            expires_at_ms: NOW + 600_000,
            node_keys,
        };
        let listed_node = vec![hex(node.verifying_key().as_bytes())];
        let signed = seal(REVOCATIONS_DOMAIN, &revocations(NOW, listed_node), &key(3));
        trust.accept_revocations(&signed, NOW).unwrap();
        let good = seal(SERVICE_TICKET_DOMAIN, &ticket(&node), &key(2));
        assert_eq!(
            trust
                .verify_ticket(&good, "nw.counter", "counter-1", NOW)
                .unwrap_err(),
            TrustError::Revoked
        );
        let older = seal(
            REVOCATIONS_DOMAIN,
            &revocations(NOW - 1000, vec![]),
            &key(3),
        );
        trust.accept_revocations(&older, NOW).unwrap();
        assert!(trust.is_revoked(&hex(node.verifying_key().as_bytes())));
        let by_ticket_key = seal(REVOCATIONS_DOMAIN, &revocations(NOW + 1, vec![]), &key(2));
        assert_eq!(
            trust.accept_revocations(&by_ticket_key, NOW),
            Err(TrustError::Signature)
        );
    }

    #[test]
    fn payloads_bind_the_connection() {
        let node = key(4);
        let payload = entitlement_payload("ticket", b"channel-a");
        let signature = sign_hex(&node, &payload);
        assert!(verify_hex(&node.verifying_key(), &payload, &signature));
        assert!(!verify_hex(
            &node.verifying_key(),
            &entitlement_payload("ticket", b"channel-b"),
            &signature
        ));
    }

    /// The bytes the portal produces (`backend/tests/test_services_contract.py`).
    #[test]
    fn the_portals_tokens_verify_here() {
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/service-tokens-v1.json")).unwrap();
        let text = |name: &str| fixture[name].as_str().unwrap().to_owned();
        let now = fixture["now_ms"].as_i64().unwrap();
        let mut trust = Trust::new(verifying_key(&text("root_public_key")).unwrap());
        assert_eq!(trust.accept_key_list(&text("key_list"), now), Ok(true));
        trust.accept_revocations(&text("revocations"), now).unwrap();
        assert!(trust.is_revoked(&text("revoked_node_key")));
        let ticket = trust
            .verify_ticket(&text("ticket"), "nw.counter", "counter-1.example.test", now)
            .unwrap();
        assert_eq!(ticket.grants.cameras, ["cam-1", "cam-2"]);
        assert!(ticket.grants.allows("cam-2") && !ticket.grants.allows("cam-3"));
        assert_eq!(ticket.kid, text("ticket_kid"));
        let instance = verifying_key(&text("instance_public_key")).unwrap();
        let payload = report_payload(
            &text("report_instance"),
            fixture["report_time_ms"].as_i64().unwrap(),
            "POST",
            &text("report_path"),
            text("report_body").as_bytes(),
        );
        assert!(verify_hex(&instance, &payload, &text("report_signature")));
    }
}
