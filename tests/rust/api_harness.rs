//! Shared harness for the hosted control-plane tests.
//!
//! Every test here talks to a **real** `nw-api` process-level service: the
//! crate's own `Server` bound to an ephemeral port on 127.0.0.1, backed by a
//! temporary SQLite file, driven over real HTTP with `reqwest`. Nothing is
//! stubbed and nothing is in-memory, so a test that passes is evidence the
//! shipped binary behaves the same way.
//!
//! The node side is reproduced faithfully: the signing rules below are copied
//! from `crates/nw-backend/src/cloud/mod.rs`, so a change that breaks the real
//! node breaks these tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{json, Value};

pub const ENROLLMENT_AUDIENCE: &str = "neurowatch-cloud-enrollment";
pub const NODE_AUDIENCE: &str = "neurowatch-cloud-node";
pub const NODE_CHALLENGE_AUDIENCE: &str = "neurowatch-cloud-node-challenge";
pub const CHECK_IN_OPERATION: &str = "metadata_check_in";
pub const INVENTORY_OPERATION: &str = "inventory_snapshot";
pub const UNLINK_OPERATION: &str = "unlink";

pub const ADMIN_TOKEN: &str = "test-admin-token-0123456789abcdefghijklmnop";

// -- media-session test configuration ---------------------------------------
//
// The media edge does not exist yet (see `docs/adr/0002-sfu-and-turn-selection.md`).
// These are the *contract* values a deployment would supply; nothing here
// contacts a real SFU or TURN relay, and no test in this repository claims a
// browser can play video.

/// Deterministic ed25519 seed for the grant signing key. Test-only.
pub const GRANT_SIGNING_SEED: [u8; 32] = [7_u8; 32];
pub const SFU_TOKEN: &str = "test-sfu-edge-token-0123456789abcdefghij";
pub const TURN_SECRET: &str = "test-turn-static-secret-0123456789abcdef";
pub const SFU_REGION: &str = "sfu-test-1";
pub const SFU_SIGNALING_BASE: &str = "https://sfu.example.test/whep";
pub const LIVEKIT_API_KEY: &str = "test-livekit-key";
pub const LIVEKIT_API_SECRET: &str = "test-livekit-secret-012345678901234567890123456789";
pub const TURN_URLS: &str = "turns:turn.example.test:5349?transport=tcp";

pub fn grant_signing_key() -> String {
    URL_SAFE_NO_PAD.encode(GRANT_SIGNING_SEED)
}

/// How a test wants the media surface configured.
#[derive(Clone, Debug)]
pub struct MediaSetup {
    pub configured: bool,
    pub grant_ttl_seconds: i64,
}

impl Default for MediaSetup {
    fn default() -> Self {
        Self {
            configured: true,
            grant_ttl_seconds: 30,
        }
    }
}

impl MediaSetup {
    /// A deployment with no media edge configured at all.
    pub fn unconfigured() -> Self {
        Self {
            configured: false,
            grant_ttl_seconds: 30,
        }
    }

    pub fn with_grant_ttl(mut self, seconds: i64) -> Self {
        self.grant_ttl_seconds = seconds;
        self
    }
}

/// A stand-in for the external identity provider that does not exist yet.
///
/// This lives in the test harness on purpose: `nw-api` ships **no** verifier,
/// so there is no built-in way for a caller to become a principal. A test has
/// to inject one through the same public trait a real provider would implement.
#[derive(Default)]
pub struct StaticPrincipals {
    principals: std::collections::HashMap<String, nw_api::identity::Principal>,
}

impl StaticPrincipals {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, credential: &str, principal: nw_api::identity::Principal) -> Self {
        self.principals.insert(credential.to_owned(), principal);
        self
    }

    /// A viewer that may watch `sub` but not `main`.
    ///
    /// `principal_id` is deliberately unrelated to the credential: a principal
    /// identifier is audited, and a credential must never be.
    pub fn viewer(principal_id: &str, tenant_id: &str) -> nw_api::identity::Principal {
        nw_api::identity::Principal {
            principal_id: principal_id.to_owned(),
            tenant_id: tenant_id.to_owned(),
            permissions: vec![nw_api::identity::PERMISSION_VIEW_LIVE.to_owned()],
        }
    }

    /// An operator that may watch both representations.
    pub fn operator(principal_id: &str, tenant_id: &str) -> nw_api::identity::Principal {
        nw_api::identity::Principal {
            principal_id: principal_id.to_owned(),
            tenant_id: tenant_id.to_owned(),
            permissions: vec![
                nw_api::identity::PERMISSION_VIEW_LIVE.to_owned(),
                nw_api::identity::PERMISSION_VIEW_LIVE_MAIN.to_owned(),
            ],
        }
    }
}

impl nw_api::identity::PrincipalVerifier for StaticPrincipals {
    fn verify<'a>(&'a self, credential: &'a str) -> nw_api::identity::VerifyFuture<'a> {
        let found = self.principals.get(credential).cloned();
        Box::pin(async move { found.ok_or(nw_api::ApiError::Unauthenticated) })
    }

    fn describe(&self) -> &'static str {
        "test-static-principals"
    }
}

/// Decode the payload of a compact grant token without verifying it, so a test
/// can assert on what a browser would be able to read.
pub fn decode_grant_payload(token: &str) -> Value {
    let segments: Vec<&str> = token.split('.').collect();
    assert_eq!(segments.len(), 3, "compact token shape: {token}");
    assert_eq!(segments[0], "nwg1", "token version tag: {token}");
    let bytes = URL_SAFE_NO_PAD.decode(segments[1]).expect("payload base64");
    serde_json::from_slice(&bytes).expect("payload json")
}

/// A throwaway directory for one test's database.
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nw-api-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The exact bytes the node signs.
pub fn canonical(value: &Value) -> String {
    serde_json::to_string(value).expect("canonical json")
}

pub fn sign(key: &SigningKey, value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(key.sign(canonical(value).as_bytes()).to_bytes())
}

pub fn without_signature(value: &Value) -> Value {
    let mut value = value.clone();
    value
        .as_object_mut()
        .expect("object body")
        .remove("signature");
    value
}

/// Sign `body` in place, the way the node does: the signature covers the whole
/// body minus the `signature` field.
pub fn signed(key: &SigningKey, mut body: Value) -> Value {
    let signature = sign(key, &without_signature(&body));
    body["signature"] = Value::String(signature);
    body
}

pub fn rfc3339(offset_seconds: i64) -> String {
    let at = (time::OffsetDateTime::now_utc() + time::Duration::seconds(offset_seconds))
        .replace_nanosecond(0)
        .expect("whole seconds");
    at.format(&time::format_description::well_known::Rfc3339)
        .expect("rfc3339")
}

/// A running control plane.
pub struct TestServer {
    pub base: String,
    pub database: PathBuf,
    pub admin_token: Option<String>,
    client: reqwest::Client,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl TestServer {
    /// Start a server on an ephemeral port against `database`, with no media
    /// surface and no identity provider.
    pub async fn start(database: &Path, admin_token: Option<&str>) -> Self {
        Self::start_with(database, admin_token, MediaSetup::unconfigured(), None).await
    }

    /// Start a server with an explicit media configuration and, optionally, an
    /// injected identity provider.
    pub async fn start_with(
        database: &Path,
        admin_token: Option<&str>,
        media: MediaSetup,
        verifier: Option<std::sync::Arc<dyn nw_api::identity::PrincipalVerifier>>,
    ) -> Self {
        let media_vars = move |name: &str| -> Option<String> {
            if !media.configured {
                return None;
            }
            match name {
                nw_api::config::GRANT_SIGNING_KEY_ENV => Some(grant_signing_key()),
                nw_api::config::GRANT_TTL_ENV => Some(media.grant_ttl_seconds.to_string()),
                nw_api::config::SFU_TOKEN_ENV => Some(SFU_TOKEN.into()),
                nw_api::config::LIVEKIT_API_KEY_ENV => Some(LIVEKIT_API_KEY.into()),
                nw_api::config::LIVEKIT_API_SECRET_ENV => Some(LIVEKIT_API_SECRET.into()),
                nw_api::config::SFU_REGION_ENV => Some(SFU_REGION.into()),
                nw_api::config::SFU_SIGNALING_BASE_ENV => Some(SFU_SIGNALING_BASE.into()),
                nw_api::config::TURN_URLS_ENV => Some(TURN_URLS.into()),
                nw_api::config::TURN_SECRET_ENV => Some(TURN_SECRET.into()),
                _ => None,
            }
        };
        let config = nw_api::ApiConfig::from_vars(|name| match name {
            nw_api::config::BIND_ENV => Some("127.0.0.1:0".into()),
            nw_api::config::DATABASE_ENV => Some(database.display().to_string()),
            nw_api::config::ADMIN_TOKEN_ENV => admin_token.map(str::to_owned),
            other => media_vars(other),
        })
        .expect("test configuration");
        let server = nw_api::Server::bind_with(&config, verifier)
            .await
            .expect("bind");
        let base = format!("http://{}", server.local_addr());
        let (shutdown, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = server
                .run(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            base,
            database: database.to_path_buf(),
            admin_token: admin_token.map(str::to_owned),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("client"),
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// POST JSON and return the status and the decoded body (`null` when the
    /// response has no body).
    pub async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.send(self.client.post(self.url(path)).json(body)).await
    }

    pub async fn post_admin(&self, path: &str, body: &Value) -> (u16, Value) {
        let token = self.admin_token.clone().unwrap_or_default();
        self.send(
            self.client
                .post(self.url(path))
                .bearer_auth(token)
                .json(body),
        )
        .await
    }

    pub async fn post_admin_with(&self, path: &str, token: &str, body: &Value) -> (u16, Value) {
        self.send(
            self.client
                .post(self.url(path))
                .bearer_auth(token)
                .json(body),
        )
        .await
    }

    pub async fn put_admin(&self, path: &str, body: &Value) -> (u16, Value) {
        let token = self.admin_token.clone().unwrap_or_default();
        self.send(
            self.client
                .put(self.url(path))
                .bearer_auth(token)
                .json(body),
        )
        .await
    }

    /// POST with an arbitrary bearer credential — how a browser principal, or
    /// the SFU edge, presents itself.
    pub async fn post_bearer(&self, path: &str, token: &str, body: &Value) -> (u16, Value) {
        self.send(
            self.client
                .post(self.url(path))
                .bearer_auth(token)
                .json(body),
        )
        .await
    }

    pub async fn delete_bearer(&self, path: &str, token: &str) -> (u16, Value) {
        self.send(self.client.delete(self.url(path)).bearer_auth(token))
            .await
    }

    pub async fn get(&self, path: &str) -> (u16, Value) {
        self.send(self.client.get(self.url(path))).await
    }

    pub async fn get_admin(&self, path: &str) -> (u16, Value) {
        let token = self.admin_token.clone().unwrap_or_default();
        self.send(self.client.get(self.url(path)).bearer_auth(token))
            .await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (u16, Value) {
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        let text = response.text().await.expect("body");
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        (status, body)
    }

    // -- control-plane conveniences -----------------------------------------

    pub async fn create_tenant(&self, name: &str) -> String {
        let (status, body) = self
            .post_admin("/admin/v1/tenants", &json!({"name": name}))
            .await;
        assert_eq!(status, 201, "create tenant: {body}");
        body["tenant_id"].as_str().expect("tenant_id").to_owned()
    }

    /// Create an enrollment and return the portal code, split as the node's
    /// `EnrollmentCode::parse` splits it.
    pub async fn create_enrollment(&self, tenant_id: &str, label: &str, ttl: i64) -> Enrollment {
        let (status, body) = self
            .post_admin(
                &format!("/admin/v1/tenants/{tenant_id}/node-enrollments"),
                &json!({"label": label, "ttl_seconds": ttl}),
            )
            .await;
        assert_eq!(status, 201, "create enrollment: {body}");
        let code = body["enrollment_code"]
            .as_str()
            .expect("enrollment_code")
            .to_owned();
        let parts: Vec<&str> = code.split('.').collect();
        assert_eq!(parts.len(), 3, "code shape: {code}");
        Enrollment {
            tenant_id: parts[0].to_owned(),
            enrollment_id: parts[1].to_owned(),
            transfer_proof: parts[2].to_owned(),
            code,
        }
    }

    /// Move an enrollment's expiry into the past, in the database the running
    /// server reads.
    ///
    /// A test that wants an expired enrollment must not *wait* for one: with a
    /// short lifetime the setup steps race the expiry they are arranging, and a
    /// claim-parameters call that lands late hands the test an error body, which
    /// then produces a malformed claim and a misleading `invalid_request`
    /// instead of the `expired` the test is actually about. Ageing the row is
    /// deterministic and leaves every server-side check intact.
    pub fn expire_enrollment(&self, enrollment: &Enrollment) {
        let connection = rusqlite::Connection::open(&self.database).expect("open control database");
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .expect("busy timeout");
        let changed = connection
            .execute(
                "UPDATE node_enrollments SET expires_at = ?1 WHERE tenant_id = ?2 AND id = ?3",
                rusqlite::params![rfc3339(-1), enrollment.tenant_id, enrollment.enrollment_id],
            )
            .expect("age the enrollment");
        assert_eq!(changed, 1, "expected exactly one enrollment row to age");
    }

    /// Read the database file's raw bytes, for "this must never be stored"
    /// assertions.
    pub fn database_bytes(&self) -> Vec<u8> {
        let mut bytes = std::fs::read(&self.database).unwrap_or_default();
        for suffix in ["-wal", "-shm"] {
            let mut path = self.database.clone().into_os_string();
            path.push(suffix);
            bytes.extend(std::fs::read(PathBuf::from(path)).unwrap_or_default());
        }
        bytes
    }
}

pub struct Enrollment {
    pub code: String,
    pub tenant_id: String,
    pub enrollment_id: String,
    pub transfer_proof: String,
}

/// A node with its own key pair, doing exactly what the shipped node client
/// does.
pub struct TestNode {
    pub key: SigningKey,
    pub tenant_id: String,
    pub node_id: String,
    pub auth_epoch: u64,
}

impl TestNode {
    pub fn new(tenant_id: &str) -> Self {
        let mut seed = [0_u8; 32];
        getrandom::getrandom(&mut seed).expect("seed");
        Self {
            key: SigningKey::from_bytes(&seed),
            tenant_id: tenant_id.to_owned(),
            node_id: uuid::Uuid::new_v4().to_string(),
            auth_epoch: 1,
        }
    }

    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.verifying_key().as_bytes())
    }

    pub fn claim_body(&self, enrollment: &Enrollment, parameters: &Value, label: &str) -> Value {
        let payload = json!({
            "protocol_version": 1,
            "enrollment_id": enrollment.enrollment_id,
            "tenant_id": enrollment.tenant_id,
            "node_id": self.node_id,
            "audience": ENROLLMENT_AUDIENCE,
            "expires_at": parameters["expires_at"],
            "challenge": parameters["challenge"],
        });
        let signature = sign(&self.key, &payload);
        json!({
            "protocol_version": 1,
            "enrollment_id": enrollment.enrollment_id,
            "tenant_id": enrollment.tenant_id,
            "node_id": self.node_id,
            "audience": ENROLLMENT_AUDIENCE,
            "expires_at": parameters["expires_at"],
            "challenge": parameters["challenge"],
            "transfer_proof": enrollment.transfer_proof,
            "public_key": self.public_key(),
            "signature": signature,
            "display_label": label,
        })
    }

    pub fn confirm_body(
        &self,
        enrollment: &Enrollment,
        parameters: &Value,
        claim_id: &str,
    ) -> Value {
        let payload = json!({
            "protocol_version": 1,
            "enrollment_id": enrollment.enrollment_id,
            "tenant_id": enrollment.tenant_id,
            "node_id": self.node_id,
            "audience": ENROLLMENT_AUDIENCE,
            "expires_at": parameters["expires_at"],
            "challenge": parameters["challenge"],
            "claim_id": claim_id,
        });
        let signature = sign(&self.key, &payload);
        json!({
            "protocol_version": 1,
            "enrollment_id": enrollment.enrollment_id,
            "tenant_id": enrollment.tenant_id,
            "node_id": self.node_id,
            "audience": ENROLLMENT_AUDIENCE,
            "expires_at": parameters["expires_at"],
            "challenge": parameters["challenge"],
            "claim_id": claim_id,
            "signature": signature,
        })
    }

    pub fn challenge_body(&self, operation: &str) -> Value {
        signed(
            &self.key,
            json!({
                "protocol_version": 1,
                "tenant_id": self.tenant_id,
                "node_id": self.node_id,
                "audience": NODE_CHALLENGE_AUDIENCE,
                "operation": operation,
                "auth_epoch": self.auth_epoch,
                "issued_at": rfc3339(0),
                "expires_at": rfc3339(60),
                "nonce": URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes()),
                "signature": "",
            }),
        )
    }

    pub fn proof_body(&self, challenge: &str) -> Value {
        signed(
            &self.key,
            json!({
                "protocol_version": 1,
                "tenant_id": self.tenant_id,
                "node_id": self.node_id,
                "audience": NODE_AUDIENCE,
                "auth_epoch": self.auth_epoch,
                "issued_at": rfc3339(0),
                "expires_at": rfc3339(60),
                "challenge": challenge,
                "signature": "",
            }),
        )
    }

    pub fn directive_proof_body(
        &self,
        challenge: &str,
        acknowledged_directive_ids: Vec<String>,
    ) -> Value {
        signed(
            &self.key,
            json!({
                "protocol_version": 1,
                "tenant_id": self.tenant_id,
                "node_id": self.node_id,
                "audience": NODE_AUDIENCE,
                "auth_epoch": self.auth_epoch,
                "issued_at": rfc3339(0),
                "expires_at": rfc3339(60),
                "challenge": challenge,
                "acknowledged_directive_ids": acknowledged_directive_ids,
                "signature": "",
            }),
        )
    }

    pub fn inventory_body(&self, challenge: &str, cameras: Value) -> Value {
        signed(
            &self.key,
            json!({
                "protocol_version": 1,
                "tenant_id": self.tenant_id,
                "node_id": self.node_id,
                "audience": NODE_AUDIENCE,
                "auth_epoch": self.auth_epoch,
                "issued_at": rfc3339(0),
                "expires_at": rfc3339(60),
                "challenge": challenge,
                "cameras": cameras,
                "signature": "",
            }),
        )
    }

    /// Ask for a challenge for `operation` and return it.
    pub async fn challenge(&self, server: &TestServer, operation: &str) -> String {
        let (status, body) = server
            .post("/v1/node-auth/challenges", &self.challenge_body(operation))
            .await;
        assert!(
            (200..300).contains(&status),
            "challenge for {operation}: {status} {body}"
        );
        body["challenge"].as_str().expect("challenge").to_owned()
    }

    pub async fn check_in(&self, server: &TestServer) -> (u16, Value) {
        let challenge = self.challenge(server, CHECK_IN_OPERATION).await;
        server
            .post(
                &format!("/v1/nodes/{}/metadata-check-ins", self.node_id),
                &self.proof_body(&challenge),
            )
            .await
    }

    pub async fn report_inventory(&self, server: &TestServer, cameras: Value) -> (u16, Value) {
        let challenge = self.challenge(server, INVENTORY_OPERATION).await;
        server
            .post(
                &format!("/v1/nodes/{}/inventory-snapshots", self.node_id),
                &self.inventory_body(&challenge, cameras),
            )
            .await
    }
}

/// Run the whole enrollment handshake and return the linked node.
pub async fn link_node(server: &TestServer, tenant_id: &str, label: &str) -> TestNode {
    let enrollment = server.create_enrollment(tenant_id, label, 900).await;
    let node = TestNode::new(tenant_id);
    let (status, parameters) = server
        .post(
            &format!(
                "/v1/node-enrollments/{}/claim-parameters",
                enrollment.enrollment_id
            ),
            &json!({
                "tenant_id": enrollment.tenant_id,
                "transfer_proof": enrollment.transfer_proof,
            }),
        )
        .await;
    assert_eq!(status, 200, "claim-parameters: {parameters}");
    let (status, claim) = server
        .post(
            &format!("/v1/node-enrollments/{}/claim", enrollment.enrollment_id),
            &node.claim_body(&enrollment, &parameters, label),
        )
        .await;
    assert_eq!(status, 200, "claim: {claim}");
    let claim_id = claim["claim_id"].as_str().expect("claim_id");
    let (status, confirmation) = server
        .post(
            &format!("/v1/node-enrollments/{}/confirm", enrollment.enrollment_id),
            &node.confirm_body(&enrollment, &parameters, claim_id),
        )
        .await;
    assert_eq!(status, 200, "confirm: {confirmation}");
    assert_eq!(confirmation["node_id"], node.node_id);
    node
}
