//! Tickets: the portal's signed permission for one recorder to back up its
//! cameras on one ingest for a short time.
//!
//! The ingest has no database: the ticket carries everything it needs (the
//! recorder's key, the cameras, their plans' limits and backup windows), and
//! it trusts the portal's signing keys only. A ticket is
//! `base64url(claims JSON) "." base64url(signature)`, the signature covering
//! [`TICKET_DOMAIN`] followed by the JSON bytes exactly as sent, so neither
//! side has to reproduce the other's JSON.
//!
//! The recorder proves it holds the ticket's key and binds it to this
//! connection by signing [`hello_payload`]: the ticket and the TLS exporter
//! ([`EXPORTER_LABEL`](crate::EXPORTER_LABEL)), so a ticket and signature
//! seen on one connection are useless on another.
use crate::limits::Limits;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Ticket format version.
pub const TICKET_VERSION: u32 = 1;
/// What the portal's signature covers, before the claims JSON.
pub const TICKET_DOMAIN: &[u8] = b"nw-backup/1 ticket\0";
const HELLO_DOMAIN: &[u8] = b"nw-backup/1 hello\0";
const RENEW_DOMAIN: &[u8] = b"nw-backup/1 renew\0";
/// Clock difference tolerated between portal, ingest and recorder.
pub const CLOCK_SKEW_MS: i64 = 60_000;
/// Longest ticket an ingest accepts: plans change, so tickets are renewed.
pub const MAX_LIFETIME_MS: i64 = 24 * 3_600_000;
/// Largest ticket text accepted.
pub const MAX_TICKET_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    /// [`TICKET_VERSION`].
    pub v: u32,
    /// Unique per ticket, for logs.
    pub id: String,
    pub org_id: String,
    pub node_id: String,
    /// The recorder's Ed25519 public key (hex), from its enrolment.
    pub node_key: String,
    /// The ingest this ticket is for (its name, as configured there).
    pub audience: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub cameras: Vec<CameraGrant>,
}

/// One camera's paid backup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraGrant {
    pub camera_id: String,
    /// How long the cloud keeps its footage in all (seconds, hot plus
    /// archive): the recorder gives up units older than this.
    pub retention_s: u32,
    /// Whether the plan includes the camera's sound.
    #[serde(default)]
    pub sound: bool,
    pub limits: Limits,
    /// How long footage stays on the ingest's own disk, where it plays at
    /// once (0: all of `retention_s`, as tickets before 2026-11-08 meant).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hot_s: u32,
    /// How often the ingest moves what is older than `hot_s` to object
    /// storage (0: hourly).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub move_every_s: u32,
    /// Which of the camera's streams the plan is for (`main` or `sub`), sent
    /// as the camera sends it: never re-encoded. Absent (tickets before
    /// 2026-11-08): the main stream while it fits `limits`, else the sub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    /// The customer's X25519 public key (hex): stored footage is sealed to it,
    /// and only the customer can open it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal_key: Option<String>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// The streams a grant may name.
pub const GRANT_STREAMS: [&str; 2] = ["main", "sub"];

impl CameraGrant {
    /// The stream the plan is for, when it names one the protocol knows
    /// (`None`: choose by `limits`, as before streams were named).
    pub fn named_stream(&self) -> Option<&str> {
        self.stream
            .as_deref()
            .filter(|stream| GRANT_STREAMS.contains(stream))
    }

    /// The customer's public key, when the grant has a well-formed one.
    pub fn seal_key(&self) -> Option<[u8; 32]> {
        unhex(self.seal_key.as_deref()?)
    }

    /// Seconds footage stays on the ingest's disk.
    pub fn hot_seconds(&self) -> u32 {
        if self.hot_s == 0 {
            self.retention_s
        } else {
            self.hot_s.min(self.retention_s)
        }
    }

    /// Seconds between the ingest's moves to object storage.
    pub fn move_every_seconds(&self) -> u32 {
        if self.move_every_s == 0 {
            3600
        } else {
            self.move_every_s.clamp(60, 86_400)
        }
    }
}

impl Ticket {
    pub fn camera(&self, camera_id: &str) -> Option<&CameraGrant> {
        self.cameras
            .iter()
            .find(|grant| grant.camera_id == camera_id)
    }

    /// The recorder's key this ticket names.
    pub fn node_key(&self) -> Option<VerifyingKey> {
        let bytes: [u8; 32] = unhex(&self.node_key)?;
        VerifyingKey::from_bytes(&bytes).ok()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TicketError {
    #[error("the ticket is malformed")]
    Malformed,
    #[error("the ticket's signature is not the portal's")]
    Signature,
    #[error("unsupported ticket version {0}")]
    Version(u32),
    #[error("the ticket is for another ingest")]
    Audience,
    #[error("the ticket is not valid yet")]
    NotYetValid,
    #[error("the ticket has expired")]
    Expired,
    #[error("the ticket is valid for too long")]
    TooLong,
    #[error("the ticket names no valid recorder key")]
    NodeKey,
}

/// Sign a ticket (the portal does this; here for tests and tools).
pub fn issue(ticket: &Ticket, portal_key: &SigningKey) -> String {
    let claims = serde_json::to_vec(ticket).unwrap_or_default();
    let mut signed = TICKET_DOMAIN.to_vec();
    signed.extend_from_slice(&claims);
    let signature = portal_key.sign(&signed);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&claims),
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

/// A ticket's claims without checking its signature: what the recorder reads
/// from the ticket it was given (the ingest checks it).
pub fn read_claims(text: &str) -> Option<Ticket> {
    let (claims, _) = text.split_once('.')?;
    let claims = URL_SAFE_NO_PAD.decode(claims).ok()?;
    serde_json::from_slice(&claims).ok()
}

/// Check a ticket: signed by one of `portal_keys`, for `audience`, current at
/// `now_ms`, naming a usable recorder key.
pub fn verify(
    text: &str,
    portal_keys: &[VerifyingKey],
    audience: &str,
    now_ms: i64,
) -> Result<Ticket, TicketError> {
    if text.len() > MAX_TICKET_BYTES {
        return Err(TicketError::Malformed);
    }
    let (claims, signature) = text.split_once('.').ok_or(TicketError::Malformed)?;
    let claims = URL_SAFE_NO_PAD
        .decode(claims)
        .map_err(|_| TicketError::Malformed)?;
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(TicketError::Malformed)?;
    let signature = Signature::from_bytes(&signature);
    let mut signed = TICKET_DOMAIN.to_vec();
    signed.extend_from_slice(&claims);
    if !portal_keys
        .iter()
        .any(|key| key.verify_strict(&signed, &signature).is_ok())
    {
        return Err(TicketError::Signature);
    }
    let ticket: Ticket = serde_json::from_slice(&claims).map_err(|_| TicketError::Malformed)?;
    if ticket.v != TICKET_VERSION {
        return Err(TicketError::Version(ticket.v));
    }
    if ticket.audience != audience {
        return Err(TicketError::Audience);
    }
    if ticket.expires_at_ms - ticket.issued_at_ms > MAX_LIFETIME_MS {
        return Err(TicketError::TooLong);
    }
    if now_ms + CLOCK_SKEW_MS < ticket.issued_at_ms {
        return Err(TicketError::NotYetValid);
    }
    if now_ms - CLOCK_SKEW_MS >= ticket.expires_at_ms {
        return Err(TicketError::Expired);
    }
    if ticket.node_key().is_none() {
        return Err(TicketError::NodeKey);
    }
    Ok(ticket)
}

/// What a portal's file token signs, before its claims JSON.
pub const FILE_TOKEN_DOMAIN: &[u8] = b"nw-backup/1 file\0";
/// Longest file token an ingest accepts.
pub const MAX_FILE_TOKEN_MS: i64 = 3_600_000;

/// The portal's permission for a browser to read hot footage from one
/// ingest's disk: every file under `prefix` (one camera's), until `exp_ms`.
/// Same form as a ticket: `base64url(JSON).base64url(signature)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileToken {
    pub v: u32,
    /// The ingest (its name).
    pub aud: String,
    /// Object keys starting with this may be read (`backup/<org>/<camera>/`).
    pub prefix: String,
    /// When set, this one object only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub exp_ms: i64,
}

pub fn issue_file_token(token: &FileToken, portal_key: &SigningKey) -> String {
    let claims = serde_json::to_vec(token).unwrap_or_default();
    let mut signed = FILE_TOKEN_DOMAIN.to_vec();
    signed.extend_from_slice(&claims);
    let signature = portal_key.sign(&signed);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&claims),
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

/// Whether `text` lets a browser read `key` from `audience` at `now_ms`.
pub fn verify_file_token(
    text: &str,
    portal_keys: &[VerifyingKey],
    audience: &str,
    key: &str,
    now_ms: i64,
) -> bool {
    let Some((claims, signature)) = text.split_once('.') else {
        return false;
    };
    let (Ok(claims), Ok(signature)) = (
        URL_SAFE_NO_PAD.decode(claims),
        URL_SAFE_NO_PAD.decode(signature),
    ) else {
        return false;
    };
    let Ok(signature) = <[u8; 64]>::try_from(signature) else {
        return false;
    };
    let signature = Signature::from_bytes(&signature);
    let mut signed = FILE_TOKEN_DOMAIN.to_vec();
    signed.extend_from_slice(&claims);
    if !portal_keys
        .iter()
        .any(|k| k.verify_strict(&signed, &signature).is_ok())
    {
        return false;
    }
    let Ok(token) = serde_json::from_slice::<FileToken>(&claims) else {
        return false;
    };
    token.v == 1
        && token.aud == audience
        && !token.prefix.is_empty()
        && token.prefix.ends_with('/')
        && key.starts_with(&token.prefix)
        && token.file.as_deref().map_or(true, |file| file == key)
        && !key.contains("..")
        && now_ms < token.exp_ms
        && token.exp_ms - now_ms <= MAX_FILE_TOKEN_MS + CLOCK_SKEW_MS
}

/// What the recorder signs in `Hello`.
pub fn hello_payload(ticket: &str, channel_binding: &[u8]) -> Vec<u8> {
    bound(HELLO_DOMAIN, ticket, channel_binding)
}

/// What the recorder signs in `Renew`.
pub fn renew_payload(ticket: &str, channel_binding: &[u8]) -> Vec<u8> {
    bound(RENEW_DOMAIN, ticket, channel_binding)
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

/// The recorder's signature of a payload, hex.
pub fn sign(node_key: &SigningKey, payload: &[u8]) -> String {
    hex(&node_key.sign(payload).to_bytes())
}

/// Whether `signature` (hex) is the ticket's recorder signing `payload`.
pub fn verify_signature(ticket: &Ticket, payload: &[u8], signature: &str) -> bool {
    let (Some(key), Some(signature)) = (ticket.node_key(), unhex::<64>(signature)) else {
        return false;
    };
    key.verify_strict(payload, &Signature::from_bytes(&signature))
        .is_ok()
}

/// Whether a renewed ticket is for the same recorder as the session's.
pub fn same_recorder(current: &Ticket, renewed: &Ticket) -> bool {
    current.org_id == renewed.org_id
        && current.node_id == renewed.node_id
        && current.node_key.eq_ignore_ascii_case(&renewed.node_key)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
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

    const NOW: i64 = 1_762_500_000_000;

    fn keys() -> (SigningKey, SigningKey) {
        (
            SigningKey::from_bytes(&[1; 32]),
            SigningKey::from_bytes(&[2; 32]),
        )
    }

    fn ticket(node: &SigningKey) -> Ticket {
        Ticket {
            v: TICKET_VERSION,
            id: "t-1".into(),
            org_id: "org-1".into(),
            node_id: "node-1".into(),
            node_key: hex(node.verifying_key().as_bytes()),
            audience: "ingest-1.neurowatch.app".into(),
            issued_at_ms: NOW,
            expires_at_ms: NOW + 3_600_000,
            cameras: vec![CameraGrant {
                stream: None,
                seal_key: None,
                camera_id: "cam-1".into(),
                retention_s: 3600,
                sound: false,
                limits: Limits::P1080_10,
                hot_s: 0,
                move_every_s: 0,
            }],
        }
    }

    #[test]
    fn a_signed_ticket_verifies_and_reads_back() {
        let (portal, node) = keys();
        let text = issue(&ticket(&node), &portal);
        let got = verify(
            &text,
            &[portal.verifying_key()],
            "ingest-1.neurowatch.app",
            NOW,
        )
        .unwrap();
        assert_eq!(got, ticket(&node));
        assert_eq!(got.camera("cam-1").unwrap().retention_s, 3600);
        assert_eq!(
            read_claims(&text),
            Some(ticket(&node)),
            "the recorder reads its own"
        );
        assert_eq!(read_claims("nonsense"), None);
        assert!(got.camera("cam-2").is_none());
        // Any of several keys (rotation).
        let other = SigningKey::from_bytes(&[9; 32]).verifying_key();
        assert!(verify(
            &text,
            &[other, portal.verifying_key()],
            "ingest-1.neurowatch.app",
            NOW
        )
        .is_ok());
    }

    #[test]
    fn bad_tickets_are_refused_with_the_reason() {
        let (portal, node) = keys();
        let keys = [portal.verifying_key()];
        let aud = "ingest-1.neurowatch.app";
        let text = issue(&ticket(&node), &portal);
        assert_eq!(
            verify(&text, &keys, "ingest-2", NOW),
            Err(TicketError::Audience)
        );
        assert_eq!(
            verify(&text, &keys, aud, NOW + 3_600_000 + CLOCK_SKEW_MS),
            Err(TicketError::Expired)
        );
        assert!(
            verify(&text, &keys, aud, NOW + 3_600_000).is_ok(),
            "within the skew"
        );
        assert_eq!(
            verify(&text, &keys, aud, NOW - CLOCK_SKEW_MS - 1),
            Err(TicketError::NotYetValid)
        );
        assert_eq!(
            verify(&text, &[node.verifying_key()], aud, NOW),
            Err(TicketError::Signature)
        );

        // A changed claim breaks the signature.
        let (claims, signature) = text.split_once('.').unwrap();
        let mut json = URL_SAFE_NO_PAD.decode(claims).unwrap();
        let at = json.windows(5).position(|w| w == b"cam-1").unwrap();
        json[at + 4] = b'2';
        let forged = format!("{}.{signature}", URL_SAFE_NO_PAD.encode(json));
        assert_eq!(
            verify(&forged, &keys, aud, NOW),
            Err(TicketError::Signature)
        );

        for malformed in ["", "abc", "a.b", &format!("{claims}.")] {
            assert_eq!(
                verify(malformed, &keys, aud, NOW),
                Err(TicketError::Malformed),
                "{malformed}"
            );
        }

        let mut long = ticket(&node);
        long.expires_at_ms = long.issued_at_ms + MAX_LIFETIME_MS + 1;
        assert_eq!(
            verify(&issue(&long, &portal), &keys, aud, NOW),
            Err(TicketError::TooLong)
        );
        let mut keyless = ticket(&node);
        keyless.node_key = "00".into();
        assert_eq!(
            verify(&issue(&keyless, &portal), &keys, aud, NOW),
            Err(TicketError::NodeKey)
        );
        let mut future = ticket(&node);
        future.v = 2;
        assert_eq!(
            verify(&issue(&future, &portal), &keys, aud, NOW),
            Err(TicketError::Version(2))
        );
    }

    #[test]
    fn the_hello_signature_binds_ticket_key_and_connection() {
        let (portal, node) = keys();
        let text = issue(&ticket(&node), &portal);
        let ticket = verify(
            &text,
            &[portal.verifying_key()],
            "ingest-1.neurowatch.app",
            NOW,
        )
        .unwrap();
        let binding = [7u8; 32];
        let signature = sign(&node, &hello_payload(&text, &binding));
        assert!(verify_signature(
            &ticket,
            &hello_payload(&text, &binding),
            &signature
        ));
        // Another connection, a renewal payload, or another key: refused.
        assert!(!verify_signature(
            &ticket,
            &hello_payload(&text, &[8u8; 32]),
            &signature
        ));
        assert!(!verify_signature(
            &ticket,
            &renew_payload(&text, &binding),
            &signature
        ));
        let thief = sign(&portal, &hello_payload(&text, &binding));
        assert!(!verify_signature(
            &ticket,
            &hello_payload(&text, &binding),
            &thief
        ));
        assert!(!verify_signature(
            &ticket,
            &hello_payload(&text, &binding),
            "zz"
        ));
    }

    #[test]
    fn renewal_must_name_the_same_recorder() {
        let (_, node) = keys();
        let current = ticket(&node);
        let mut renewed = ticket(&node);
        renewed.id = "t-2".into();
        renewed.node_key = renewed.node_key.to_uppercase();
        assert!(same_recorder(&current, &renewed));
        renewed.node_id = "node-2".into();
        assert!(!same_recorder(&current, &renewed));
    }

    /// A ticket the web portal signed (Python, `tests/test_backup_api.py`
    /// writes the same file): both sides agree on the signed bytes.
    #[test]
    fn the_portal_s_tickets_verify() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/portal-ticket-v1.json")).unwrap();
        let key: [u8; 32] = unhex(fixture["portal_public_key"].as_str().unwrap()).unwrap();
        let key = VerifyingKey::from_bytes(&key).unwrap();
        let text = fixture["ticket"].as_str().unwrap();
        let audience = fixture["audience"].as_str().unwrap();
        let now = fixture["now_ms"].as_i64().unwrap();
        let ticket = verify(text, &[key], audience, now).unwrap();
        let claims: Ticket = serde_json::from_value(fixture["claims"].clone()).unwrap();
        assert_eq!(ticket, claims);
        assert_eq!(ticket.cameras[0].limits, Limits::P1080_10);
        assert!(ticket.cameras[0].sound);
        // The portal's link to hot footage verifies on the ingest it names.
        let file_token = fixture["file_token"].as_str().unwrap();
        let file_key = fixture["file_key"].as_str().unwrap();
        assert!(verify_file_token(
            file_token,
            &[key],
            audience,
            file_key,
            now
        ));
        assert!(!verify_file_token(
            file_token,
            &[key],
            "ingest-2",
            file_key,
            now
        ));
    }

    #[test]
    fn grants_say_how_long_footage_stays_hot() {
        let grant = |retention_s, hot_s, move_every_s| CameraGrant {
            stream: None,
            seal_key: None,
            camera_id: "c".into(),
            retention_s,
            sound: false,
            limits: Limits::P1080_10,
            hot_s,
            move_every_s,
        };
        assert_eq!(
            grant(3600, 0, 0).hot_seconds(),
            3600,
            "older tickets: all of it hot"
        );
        assert_eq!(grant(8 * 86_400, 86_400, 0).hot_seconds(), 86_400);
        assert_eq!(grant(3600, 0, 0).move_every_seconds(), 3600);
        assert_eq!(
            grant(3600, 0, 5).move_every_seconds(),
            60,
            "at most once a minute"
        );
        let json = serde_json::to_value(grant(3600, 0, 0)).unwrap();
        assert!(
            json.get("hot_s").is_none(),
            "unset fields are left out: {json}"
        );
    }

    #[test]
    fn a_file_token_naming_a_file_opens_only_that_file() {
        let (portal, _) = keys();
        let keys = [portal.verifying_key()];
        let token = issue_file_token(
            &FileToken {
                v: 1,
                aud: "ingest-1".into(),
                prefix: "backup/org/cam-1/".into(),
                file: Some("backup/org/cam-1/a.mp4".into()),
                exp_ms: NOW + 180_000,
            },
            &portal,
        );
        assert!(verify_file_token(
            &token,
            &keys,
            "ingest-1",
            "backup/org/cam-1/a.mp4",
            NOW
        ));
        assert!(!verify_file_token(
            &token,
            &keys,
            "ingest-1",
            "backup/org/cam-1/b.mp4",
            NOW
        ));
    }

    #[test]
    fn file_tokens_cover_one_prefix_on_one_ingest_for_a_while() {
        let (portal, other) = keys();
        let token = FileToken {
            v: 1,
            aud: "ingest-1".into(),
            prefix: "backup/org/cam-1/".into(),
            file: None,
            exp_ms: NOW + 600_000,
        };
        let text = issue_file_token(&token, &portal);
        let keys = [portal.verifying_key()];
        let key = "backup/org/cam-1/2026-11-08/120000Z-1-p0-0.mp4";
        assert!(verify_file_token(&text, &keys, "ingest-1", key, NOW));
        assert!(
            !verify_file_token(&text, &keys, "ingest-2", key, NOW),
            "another ingest"
        );
        assert!(
            !verify_file_token(&text, &keys, "ingest-1", "backup/org/cam-2/x.mp4", NOW),
            "another camera"
        );
        assert!(!verify_file_token(
            &text,
            &keys,
            "ingest-1",
            "backup/org/cam-1/../cam-2/x.mp4",
            NOW
        ));
        assert!(
            !verify_file_token(&text, &keys, "ingest-1", key, NOW + 600_000),
            "expired"
        );
        assert!(
            !verify_file_token(&text, &[other.verifying_key()], "ingest-1", key, NOW),
            "not the portal's"
        );
        assert!(!verify_file_token("junk", &keys, "ingest-1", key, NOW));
        let long = FileToken {
            exp_ms: NOW + 2 * MAX_FILE_TOKEN_MS,
            ..token.clone()
        };
        assert!(
            !verify_file_token(
                &issue_file_token(&long, &portal),
                &keys,
                "ingest-1",
                key,
                NOW
            ),
            "too long-lived"
        );
        let bare = FileToken {
            prefix: "backup/".into(),
            ..token
        };
        assert!(verify_file_token(
            &issue_file_token(&bare, &portal),
            &keys,
            "ingest-1",
            key,
            NOW
        ));
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex::<3>("000fff"), Some([0, 15, 255]));
        assert_eq!(unhex::<3>("000FFF"), Some([0, 15, 255]));
        assert_eq!(unhex::<3>("000ff"), None);
        assert_eq!(unhex::<2>("zz00"), None);
        assert_eq!(unhex::<2>("é0"), None);
    }

    #[test]
    fn a_grant_names_its_stream_and_older_grants_stay_byte_identical() {
        let mut grant: CameraGrant = serde_json::from_value(serde_json::json!({
            "camera_id": "cam-1", "retention_s": 3600, "sound": false,
            "limits": {"max_long_side": 8192, "max_short_side": 8192, "max_fps": 60.0, "max_bitrate_bps": 40_000_000}
        }))
        .unwrap();
        assert_eq!(grant.named_stream(), None, "choose by limits, as before");
        assert!(!serde_json::to_string(&grant).unwrap().contains("stream"));
        grant.stream = Some("sub".into());
        assert_eq!(grant.named_stream(), Some("sub"));
        assert!(serde_json::to_string(&grant)
            .unwrap()
            .contains(r#""stream":"sub""#));
        grant.stream = Some("other".into());
        assert_eq!(grant.named_stream(), None, "an unknown stream is not taken");
    }
}
