//! The cloud side of the node link protocol (`cloud-v1`).
//!
//! Every constant, path, field name and signing rule here mirrors the node
//! client in `crates/nw-backend/src/cloud/mod.rs`. This is deliberately one
//! protocol described twice, not two protocols: the canonical form below is
//! `serde_json`'s object serialisation, whose keys are sorted, which is what
//! the node signs (see the node's `fixtures/*.json` vectors).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

use crate::error::{ApiError, ApiResult};

pub const PROTOCOL_VERSION: u8 = 1;
pub const ENROLLMENT_AUDIENCE: &str = "neurowatch-cloud-enrollment";
pub const NODE_AUDIENCE: &str = "neurowatch-cloud-node";
pub const NODE_CHALLENGE_AUDIENCE: &str = "neurowatch-cloud-node-challenge";

pub const CHECK_IN_OPERATION: &str = "metadata_check_in";
pub const INVENTORY_OPERATION: &str = "inventory_snapshot";
pub const PUBLISH_DIRECTIVES_OPERATION: &str = "publish_directives";
pub const UNLINK_OPERATION: &str = "unlink";

/// Operations a node may ask for a challenge for.
pub const OPERATIONS: [&str; 4] = [
    CHECK_IN_OPERATION,
    INVENTORY_OPERATION,
    PUBLISH_DIRECTIVES_OPERATION,
    UNLINK_OPERATION,
];

/// How long an issued challenge is usable.
pub const CHALLENGE_TTL_SECONDS: i64 = 60;
/// Longest accepted signed-request window. The node signs 60-second windows.
pub const MAX_REQUEST_LIFETIME_SECONDS: i64 = 300;
/// Tolerance for a node whose clock runs ahead of the cloud's.
pub const MAX_CLOCK_SKEW_SECONDS: i64 = 60;

/// Coarse camera states the node is allowed to report (`cloud_status` in the
/// node's inventory module). Anything else is refused rather than stored.
pub const CAMERA_STATUSES: [&str; 6] = [
    "streaming",
    "connecting",
    "reconnecting",
    "error",
    "offline",
    "stopped",
];

/// The exact bytes a signature covers: `serde_json`'s sorted-key form.
pub fn canonical_json(value: &Value) -> ApiResult<String> {
    serde_json::to_string(value).map_err(|error| ApiError::Internal(error.to_string()))
}

/// The signed payload of a node request is the whole body without `signature`.
pub fn without_signature(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("signature");
    }
    value
}

/// Verify an ed25519 signature over the canonical form of `payload`.
pub fn verify_signature(public_key: &str, signature: &str, payload: &Value) -> bool {
    let Ok(canonical) = canonical_json(payload) else {
        return false;
    };
    let Some(key) = decode_public_key(public_key) else {
        return false;
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    let Ok(bytes): Result<[u8; 64], _> = bytes.try_into() else {
        return false;
    };
    key.verify(canonical.as_bytes(), &Signature::from_bytes(&bytes))
        .is_ok()
}

pub fn decode_public_key(public_key: &str) -> Option<VerifyingKey> {
    let bytes = URL_SAFE_NO_PAD.decode(public_key).ok()?;
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Hash of a one-time secret. Only this ever reaches the database.
pub fn proof_hash(proof: &str) -> String {
    let digest = Sha256::digest(proof.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Constant-time comparison of two hex digests.
pub fn hash_matches(left: &str, right: &str) -> bool {
    use subtle::ConstantTimeEq as _;
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

/// `n` random bytes as url-safe base64 without padding.
pub fn random_token(bytes: usize) -> ApiResult<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::getrandom(&mut buffer).map_err(|error| ApiError::Internal(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// A canonical-form UUID string, as the node's parser requires.
pub fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

// ---------------------------------------------------------------------------
// Enrollment
// ---------------------------------------------------------------------------

/// `POST /v1/node-enrollments/{enrollment_id}/claim-parameters`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimParametersRequest {
    pub tenant_id: String,
    pub transfer_proof: String,
}

#[derive(Debug, Serialize)]
pub struct ClaimParametersResponse {
    pub protocol_version: u8,
    pub audience: &'static str,
    pub enrollment_id: String,
    pub tenant_id: String,
    pub expires_at: String,
    pub challenge: String,
}

/// `POST /v1/node-enrollments/{enrollment_id}/claim`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    pub protocol_version: u8,
    pub enrollment_id: String,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub expires_at: String,
    pub challenge: String,
    pub transfer_proof: String,
    pub public_key: String,
    pub signature: String,
    pub display_label: String,
}

impl ClaimRequest {
    /// Exactly the subset the node signs; `transfer_proof`, `public_key` and
    /// `display_label` are deliberately outside it.
    pub fn signed_payload(&self) -> Value {
        serde_json::json!({
            "protocol_version": self.protocol_version,
            "enrollment_id": self.enrollment_id,
            "tenant_id": self.tenant_id,
            "node_id": self.node_id,
            "audience": self.audience,
            "expires_at": self.expires_at,
            "challenge": self.challenge,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct ClaimResponse {
    pub claim_id: String,
}

/// `POST /v1/node-enrollments/{enrollment_id}/confirm`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    pub protocol_version: u8,
    pub enrollment_id: String,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub expires_at: String,
    pub challenge: String,
    pub claim_id: String,
    pub signature: String,
}

impl ConfirmRequest {
    pub fn signed_payload(&self) -> Value {
        serde_json::json!({
            "protocol_version": self.protocol_version,
            "enrollment_id": self.enrollment_id,
            "tenant_id": self.tenant_id,
            "node_id": self.node_id,
            "audience": self.audience,
            "expires_at": self.expires_at,
            "challenge": self.challenge,
            "claim_id": self.claim_id,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct ConfirmResponse {
    pub status: &'static str,
    pub node_id: String,
    pub auth_epoch: u64,
}

// ---------------------------------------------------------------------------
// Authenticated node requests
// ---------------------------------------------------------------------------

/// `POST /v1/node-auth/challenges`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeRequest {
    pub protocol_version: u8,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub operation: String,
    pub auth_epoch: u64,
    pub issued_at: String,
    pub expires_at: String,
    pub nonce: String,
    pub signature: String,
}

/// The node deserialises this with `deny_unknown_fields`: these five fields,
/// no more.
#[derive(Debug, Serialize)]
pub struct ChallengeResponse {
    pub protocol_version: u8,
    pub audience: &'static str,
    pub operation: String,
    pub challenge: String,
    pub expires_at: String,
}

/// The body of a check-in and of an unlink indication.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeProof {
    pub protocol_version: u8,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub auth_epoch: u64,
    pub issued_at: String,
    pub expires_at: String,
    pub challenge: String,
    pub signature: String,
}

/// A directive pull also confirms exact directive generations consumed by the
/// node. The acknowledgement list is covered by the node signature.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishDirectivesRequest {
    pub protocol_version: u8,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub auth_epoch: u64,
    pub issued_at: String,
    pub expires_at: String,
    pub challenge: String,
    pub acknowledged_directive_ids: Vec<String>,
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct CheckInResponse {
    pub node_id: String,
    pub auth_epoch: u64,
    pub status: &'static str,
    pub checked_in_at: String,
}

/// `POST /v1/nodes/{node_id}/inventory-snapshots`
///
/// `deny_unknown_fields` here and on [`CameraEntry`] is the allowlist: a node
/// that adds an RTSP URL, a credential or a path to a snapshot has its whole
/// snapshot refused, so such a field cannot reach the database by being
/// ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventorySnapshot {
    pub protocol_version: u8,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub auth_epoch: u64,
    pub issued_at: String,
    pub expires_at: String,
    pub challenge: String,
    pub cameras: Vec<CameraEntry>,
    pub signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraEntry {
    pub camera_id: String,
    pub name: String,
    pub status: String,
    pub recording: bool,
}

#[derive(Debug, Serialize)]
pub struct InventoryResponse {
    pub node_id: String,
    pub accepted: bool,
    pub camera_count: usize,
}

// ---------------------------------------------------------------------------
// Validation shared by the handlers
// ---------------------------------------------------------------------------

/// A signed window that passed validation, with both timestamps rewritten into
/// the canonical whole-second `Z` form.
///
/// The node always writes that form, but `+00:00`, a lowercase `z` and
/// fractional seconds are all the same instants and the parser accepts them.
/// Stored timestamps are compared as text — snapshot ordering and the nonce
/// reaper both do it in SQL — so only the canonical spelling may reach the
/// database; otherwise two spellings of one instant order by punctuation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedWindow {
    pub issued_at: String,
    pub expires_at: String,
}

/// Reject a signed window that is malformed, already over, too long, or too far
/// in the future for a plausible clock difference.
pub fn validate_window(
    issued_at: &str,
    expires_at: &str,
    now: OffsetDateTime,
) -> ApiResult<SignedWindow> {
    let issued = crate::clock::parse(issued_at)
        .ok_or_else(|| ApiError::BadRequest("issued_at must be UTC RFC 3339".into()))?;
    let expires = crate::clock::parse(expires_at)
        .ok_or_else(|| ApiError::BadRequest("expires_at must be UTC RFC 3339".into()))?;
    if expires <= issued {
        return Err(ApiError::BadRequest(
            "expires_at must be after issued_at".into(),
        ));
    }
    if (expires - issued).whole_seconds() > MAX_REQUEST_LIFETIME_SECONDS {
        return Err(ApiError::BadRequest("signed window is too long".into()));
    }
    if expires <= now {
        return Err(ApiError::Expired("the signed request has expired".into()));
    }
    if (issued - now).whole_seconds() > MAX_CLOCK_SKEW_SECONDS {
        return Err(ApiError::BadRequest("issued_at is in the future".into()));
    }
    Ok(SignedWindow {
        issued_at: crate::clock::format(issued),
        expires_at: crate::clock::format(expires),
    })
}

/// The same rules the node applies before sending a label, applied again on
/// arrival: a label is operator-facing text and nothing else.
pub fn validate_display_label(label: &str) -> ApiResult<()> {
    const FORBIDDEN: [&str; 12] = [
        "://",
        "camera",
        "rtsp",
        "credential",
        "password",
        "secret",
        "private key",
        " lan",
        " ipc",
        "storage",
        "bucket",
        "endpoint",
    ];
    let lowered = label.to_ascii_lowercase();
    let acceptable = !label.is_empty()
        && label.len() <= 64
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._()#&'-".contains(c))
        && !FORBIDDEN.iter().any(|word| lowered.contains(word));
    if acceptable {
        Ok(())
    } else {
        Err(ApiError::BadRequest("invalid display label".into()))
    }
}

/// Camera metadata the hosted side is willing to store. Rejects anything that
/// looks like an address, a URL or a credential rather than sanitising it: the
/// node already sanitises, so a violation means the two sides disagree and the
/// snapshot should be refused loudly.
pub fn validate_camera(camera: &CameraEntry) -> ApiResult<()> {
    let bad = |reason: &str| Err(ApiError::BadRequest(format!("invalid camera: {reason}")));
    if camera.camera_id.is_empty()
        || camera.camera_id.len() > 64
        || !camera
            .camera_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return bad("camera_id must be an opaque identifier");
    }
    if camera.name.is_empty() || camera.name.chars().count() > 64 {
        return bad("name length");
    }
    if camera.name.contains("://")
        || camera.name.contains('@')
        || camera.name.chars().any(char::is_control)
        || contains_ipv4(&camera.name)
    {
        return bad("name must not carry an address, URL or credential");
    }
    if !CAMERA_STATUSES.contains(&camera.status.as_str()) {
        return bad("status is not a known camera state");
    }
    Ok(())
}

/// True if the text contains four dot-separated groups of 1–3 digits; mirrors
/// the node-side check.
fn contains_ipv4(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .any(|token| {
            let groups: Vec<&str> = token.split('.').collect();
            groups.len() >= 4
                && groups
                    .windows(4)
                    .any(|window| window.iter().all(|group| (1..=3).contains(&group.len())))
        })
}
