//! Sealed web-portal requests: camera addresses and passwords that only the
//! browser and this node can read.
//!
//! The browser encrypts a request to this node's X25519 key, which is the
//! node's Ed25519 identity key mapped to Montgomery form (the cloud derives
//! the public half from the identity key it already holds). The request
//! carries a one-time id, an expiry and a fresh browser key; the node seals
//! its answer to that key. The cloud relays ciphertext only.
//!
//! Each direction is ephemeral X25519 → HKDF-SHA256 → AES-256-GCM, bound to
//! the node and the method (request) or the request id (answer).

use std::collections::HashMap;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use curve25519_dalek::montgomery::MontgomeryPoint;
use ed25519_dalek::SigningKey;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

pub const SEALED_VERSION: u64 = 1;
const REQUEST_INFO: &[u8] = b"neurowatch sealed request v1";
const REPLY_INFO: &[u8] = b"neurowatch sealed reply v1";
/// Longest a sealed request may be valid for.
pub const MAX_REQUEST_LIFETIME_SECS: i64 = 300;
const MAX_SEALED_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("the sealed request is malformed")]
    Malformed,
    #[error("the sealed request could not be opened")]
    Unopenable,
    #[error("the sealed request has expired")]
    Expired,
    #[error("the sealed request was already used")]
    Replayed,
}

/// An opened request: its parameters and where to seal the answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub request_id: String,
    pub params: Value,
    pub reply_key: [u8; 32],
}

/// This node's X25519 public key, as the browser seals to it.
pub fn node_public_key(key: &SigningKey) -> [u8; 32] {
    key.verifying_key().to_montgomery().to_bytes()
}

fn hkdf(shared: &[u8; 32], salt: &[u8], info: &[u8]) -> Result<[u8; 32], SealError> {
    // RFC 5869 with one output block.
    let mut extract =
        <Hmac<Sha256> as Mac>::new_from_slice(salt).map_err(|_| SealError::Unopenable)?;
    extract.update(shared);
    let prk = extract.finalize().into_bytes();
    let mut expand =
        <Hmac<Sha256> as Mac>::new_from_slice(&prk).map_err(|_| SealError::Unopenable)?;
    expand.update(info);
    expand.update(&[1]);
    Ok(expand.finalize().into_bytes().into())
}

fn x25519(scalar: [u8; 32], point: [u8; 32]) -> Result<[u8; 32], SealError> {
    let shared = MontgomeryPoint(point).mul_clamped(scalar).to_bytes();
    // A low-order point gives an all-zero secret; refuse it.
    if shared.iter().all(|b| *b == 0) {
        return Err(SealError::Unopenable);
    }
    Ok(shared)
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64<const N: usize>(value: &Value) -> Result<[u8; N], SealError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value.as_str().ok_or(SealError::Malformed)?)
        .map_err(|_| SealError::Malformed)?;
    bytes.try_into().map_err(|_| SealError::Malformed)
}

fn request_aad(node_id: &str, method: &str) -> Vec<u8> {
    format!("neurowatch-sealed-request-v1\0{node_id}\0{method}").into_bytes()
}

fn reply_aad(node_id: &str, request_id: &str) -> Vec<u8> {
    format!("neurowatch-sealed-reply-v1\0{node_id}\0{request_id}").into_bytes()
}

/// Encrypt `plaintext` to `recipient` with the ephemeral scalar `ephemeral`.
fn seal_with(
    ephemeral: [u8; 32],
    nonce: [u8; 12],
    recipient: [u8; 32],
    info: &[u8],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Value, SealError> {
    let epk = MontgomeryPoint::mul_base_clamped(ephemeral).to_bytes();
    let shared = x25519(ephemeral, recipient)?;
    let salt = [epk.as_slice(), recipient.as_slice()].concat();
    let key = hkdf(&shared, &salt, info)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| SealError::Unopenable)?;
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| SealError::Unopenable)?;
    Ok(json!({"v": SEALED_VERSION, "epk": b64(&epk), "nonce": b64(&nonce), "ct": b64(&ct)}))
}

fn open_with(
    scalar: [u8; 32],
    own_public: [u8; 32],
    envelope: &Value,
    info: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, SealError> {
    if envelope["v"].as_u64() != Some(SEALED_VERSION) {
        return Err(SealError::Malformed);
    }
    if envelope.to_string().len() > MAX_SEALED_BYTES {
        return Err(SealError::Malformed);
    }
    let epk: [u8; 32] = unb64(&envelope["epk"])?;
    let nonce: [u8; 12] = unb64(&envelope["nonce"])?;
    let ct = URL_SAFE_NO_PAD
        .decode(envelope["ct"].as_str().ok_or(SealError::Malformed)?)
        .map_err(|_| SealError::Malformed)?;
    let shared = x25519(scalar, epk)?;
    let salt = [epk.as_slice(), own_public.as_slice()].concat();
    let key = hkdf(&shared, &salt, info)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| SealError::Unopenable)?;
    cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad })
        .map_err(|_| SealError::Unopenable)
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let _ = getrandom::getrandom(&mut bytes);
    bytes
}

/// Request ids already opened, until they expire.
#[derive(Default, Debug)]
pub struct ReplayGuard {
    seen: HashMap<String, i64>,
}

impl ReplayGuard {
    fn admit(&mut self, id: &str, expires: i64, now: i64) -> bool {
        self.seen.retain(|_, exp| *exp > now);
        if self.seen.contains_key(id) || self.seen.len() >= 4096 {
            return false;
        }
        self.seen.insert(id.to_owned(), expires);
        true
    }
}

/// Open a sealed request for `method`, sent to this node.
pub fn open_request(
    key: &SigningKey,
    node_id: &str,
    method: &str,
    envelope: &Value,
    now_unix: i64,
    replay: &mut ReplayGuard,
) -> Result<Opened, SealError> {
    let plaintext = open_with(
        key.to_scalar_bytes(),
        node_public_key(key),
        envelope,
        REQUEST_INFO,
        &request_aad(node_id, method),
    )?;
    let inner: Value = serde_json::from_slice(&plaintext).map_err(|_| SealError::Malformed)?;
    let request_id = inner["id"]
        .as_str()
        .filter(|id| (16..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or(SealError::Malformed)?
        .to_owned();
    let expires = inner["exp"].as_i64().ok_or(SealError::Malformed)?;
    if expires <= now_unix || expires > now_unix + MAX_REQUEST_LIFETIME_SECS {
        return Err(SealError::Expired);
    }
    let reply_key: [u8; 32] = unb64(&inner["reply_key"])?;
    let params = inner["params"].clone();
    if !params.is_object() {
        return Err(SealError::Malformed);
    }
    if !replay.admit(&request_id, expires, now_unix) {
        return Err(SealError::Replayed);
    }
    Ok(Opened {
        request_id,
        params,
        reply_key,
    })
}

/// Seal an answer for the browser that sent `opened`.
pub fn seal_reply(
    node_id: &str,
    opened: &Opened,
    outcome: &std::result::Result<Value, String>,
) -> Result<Value, SealError> {
    seal_reply_with(random(), random(), node_id, opened, outcome)
}

fn reply_plaintext(outcome: &std::result::Result<Value, String>) -> Vec<u8> {
    let body = match outcome {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    body.to_string().into_bytes()
}

fn seal_reply_with(
    ephemeral: [u8; 32],
    nonce: [u8; 12],
    node_id: &str,
    opened: &Opened,
    outcome: &std::result::Result<Value, String>,
) -> Result<Value, SealError> {
    seal_with(
        ephemeral,
        nonce,
        opened.reply_key,
        REPLY_INFO,
        &reply_aad(node_id, &opened.request_id),
        &reply_plaintext(outcome),
    )
}

/// Seal a request the way the browser does (tests and fixtures only).
#[cfg(test)]
pub fn seal_request_with(
    ephemeral: [u8; 32],
    nonce: [u8; 12],
    node_public: [u8; 32],
    node_id: &str,
    method: &str,
    inner: &Value,
) -> Value {
    seal_with(
        ephemeral,
        nonce,
        node_public,
        REQUEST_INFO,
        &request_aad(node_id, method),
        inner.to_string().as_bytes(),
    )
    .expect("seal")
}

/// Open an answer the way the browser does (tests only).
#[cfg(test)]
pub fn open_reply(
    browser_scalar: [u8; 32],
    node_id: &str,
    request_id: &str,
    envelope: &Value,
) -> Result<Value, SealError> {
    let public = MontgomeryPoint::mul_base_clamped(browser_scalar).to_bytes();
    let plaintext = open_with(
        browser_scalar,
        public,
        envelope,
        REPLY_INFO,
        &reply_aad(node_id, request_id),
    )?;
    serde_json::from_slice(&plaintext).map_err(|_| SealError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
    const NOW: i64 = 1_790_000_000;

    fn browser() -> ([u8; 32], [u8; 32]) {
        let scalar = [5u8; 32];
        (scalar, MontgomeryPoint::mul_base_clamped(scalar).to_bytes())
    }

    fn sealed(key: &SigningKey, method: &str, id: &str, exp: i64) -> Value {
        let (_, reply_key) = browser();
        seal_request_with(
            [3u8; 32],
            [4u8; 12],
            node_public_key(key),
            NODE,
            method,
            &json!({"id": id, "exp": exp, "reply_key": b64(&reply_key), "params": {"url": "rtsp://10.0.0.5/live", "password_b64": "c2VjcmV0"}}),
        )
    }

    #[test]
    fn the_node_key_is_the_identity_key_in_montgomery_form() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        // The same public key any X25519 implementation gets from the scalar.
        assert_eq!(
            node_public_key(&key),
            MontgomeryPoint::mul_base_clamped(key.to_scalar_bytes()).to_bytes()
        );
    }

    #[test]
    fn a_sealed_request_opens_once_and_its_answer_opens_for_the_browser_only() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let envelope = sealed(&key, "camera.add", "aaaaaaaaaaaaaaaa", NOW + 60);
        let mut replay = ReplayGuard::default();
        let opened =
            open_request(&key, NODE, "camera.add", &envelope, NOW, &mut replay).expect("opens");
        assert_eq!(opened.params["password_b64"], "c2VjcmV0");
        // The relay must not be able to submit it twice.
        assert_eq!(
            open_request(&key, NODE, "camera.add", &envelope, NOW, &mut replay),
            Err(SealError::Replayed)
        );
        let answer = seal_reply(NODE, &opened, &Ok(json!({"id": "cam-1"}))).expect("sealed");
        assert!(!answer.to_string().contains("cam-1"));
        let (scalar, _) = browser();
        assert_eq!(
            open_reply(scalar, NODE, "aaaaaaaaaaaaaaaa", &answer).expect("opens"),
            json!({"ok": true, "result": {"id": "cam-1"}})
        );
        assert!(open_reply([6u8; 32], NODE, "aaaaaaaaaaaaaaaa", &answer).is_err());
        assert!(open_reply(scalar, NODE, "bbbbbbbbbbbbbbbb", &answer).is_err());
    }

    #[test]
    fn a_request_is_bound_to_its_method_node_lifetime_and_bytes() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut replay = ReplayGuard::default();
        let envelope = sealed(&key, "camera.add", "cccccccccccccccc", NOW + 60);
        // Replayed as another method, or to another node.
        assert_eq!(
            open_request(&key, NODE, "camera.update", &envelope, NOW, &mut replay),
            Err(SealError::Unopenable)
        );
        assert_eq!(
            open_request(
                &key,
                "other-node",
                "camera.add",
                &envelope,
                NOW,
                &mut replay
            ),
            Err(SealError::Unopenable)
        );
        assert_eq!(
            open_request(
                &SigningKey::from_bytes(&[8u8; 32]),
                NODE,
                "camera.add",
                &envelope,
                NOW,
                &mut replay
            ),
            Err(SealError::Unopenable)
        );
        let mut tampered = envelope.clone();
        let mut ct = URL_SAFE_NO_PAD
            .decode(tampered["ct"].as_str().unwrap())
            .unwrap();
        ct[0] ^= 1;
        tampered["ct"] = Value::String(b64(&ct));
        assert_eq!(
            open_request(&key, NODE, "camera.add", &tampered, NOW, &mut replay),
            Err(SealError::Unopenable)
        );
        // Expired, or valid for too long.
        assert_eq!(
            open_request(
                &key,
                NODE,
                "camera.add",
                &sealed(&key, "camera.add", "dddddddddddddddd", NOW - 1),
                NOW,
                &mut replay
            ),
            Err(SealError::Expired)
        );
        assert_eq!(
            open_request(
                &key,
                NODE,
                "camera.add",
                &sealed(&key, "camera.add", "eeeeeeeeeeeeeeee", NOW + 3600),
                NOW,
                &mut replay
            ),
            Err(SealError::Expired)
        );
        assert_eq!(
            open_request(&key, NODE, "camera.add", &json!({"v": 2}), NOW, &mut replay),
            Err(SealError::Malformed)
        );
        // A low-order browser key yields no shared secret.
        let mut low = envelope;
        low["epk"] = Value::String(b64(&[0u8; 32]));
        assert_eq!(
            open_request(&key, NODE, "camera.add", &low, NOW, &mut replay),
            Err(SealError::Unopenable)
        );
    }

    /// Fixed-key vectors the web portal reproduces byte for byte
    /// (`frontend/tests/sealed.test.ts`, `backend/tests/test_sealing_key.py`).
    pub fn vectors() -> Value {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let (browser_scalar, reply_key) = browser();
        let inner = json!({"id": "0123456789abcdef0123456789abcdef", "exp": NOW + 120, "reply_key": b64(&reply_key), "params": {"name": "Gate", "url": "rtsp://10.0.0.5/live", "password_b64": "c2VjcmV0"}});
        let request = seal_request_with(
            [3u8; 32],
            [4u8; 12],
            node_public_key(&key),
            NODE,
            "camera.add",
            &inner,
        );
        let opened = Opened {
            request_id: "0123456789abcdef0123456789abcdef".into(),
            params: inner["params"].clone(),
            reply_key,
        };
        let reply = seal_reply_with(
            [11u8; 32],
            [12u8; 12],
            NODE,
            &opened,
            &Ok(json!({"id": "cam-1", "name": "Gate"})),
        )
        .expect("reply");
        json!({
            "node_id": NODE,
            "node_ed25519_public": b64(key.verifying_key().as_bytes()),
            "node_x25519_public": b64(&node_public_key(&key)),
            "method": "camera.add",
            "browser_ephemeral_scalar": b64(&[3u8; 32]),
            "request_nonce": b64(&[4u8; 12]),
            "request_inner": inner.to_string(),
            "request": request,
            "reply_scalar": b64(&browser_scalar),
            "reply": reply,
            "reply_plaintext": reply_plaintext(&Ok(json!({"id": "cam-1", "name": "Gate"}))).iter().map(|b| *b as char).collect::<String>(),
        })
    }

    #[test]
    fn sealed_vectors_match_the_shared_fixture() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let vectors = vectors();
        let opened = open_request(
            &key,
            NODE,
            "camera.add",
            &vectors["request"],
            NOW,
            &mut ReplayGuard::default(),
        )
        .expect("opens");
        assert_eq!(opened.params["name"], "Gate");
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/sealed-v1.json")).expect("fixture is JSON");
        assert_eq!(
            vectors, fixture,
            "regenerate with: cargo test -- --ignored write_sealed_fixture"
        );
    }

    #[test]
    #[ignore = "writes the shared fixture"]
    fn write_sealed_fixture() {
        let text = serde_json::to_string_pretty(&vectors()).expect("serialize") + "\n";
        std::fs::write("src/cloud/fixtures/sealed-v1.json", text).expect("write fixture");
    }
}
