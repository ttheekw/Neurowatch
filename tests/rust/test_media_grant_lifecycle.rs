//! The access boundaries around a live media session.
//!
//! Every test here drives a real `nw-api` service over real HTTP. None of them
//! plays video, contacts an SFU or a TURN relay, or asserts that a browser can
//! see a picture — no such path exists yet. What they prove is narrower and
//! must hold before one is built: a grant is issued only to a validated
//! principal for a camera that principal's tenant owns, it is spendable exactly
//! once inside a short window, revoking its session kills it, the tenant's
//! persisted policy caps how many can be alive at once, and nothing about the
//! customer's network survives into the token, the response or the database.

mod api_harness;

use api_harness::*;
use base64::Engine as _;
use serde_json::json;

const VIEWER: &str = "viewer-credential";
const OPERATOR: &str = "operator-credential";
const OTHER_TENANT: &str = "other-tenant-credential";

/// One tenant with a linked node, one camera, and a media policy.
struct Fixture {
    server: TestServer,
    tenant_id: String,
    node_id: String,
    camera_id: String,
    dir: std::path::PathBuf,
}

async fn fixture(tag: &str, media: MediaSetup) -> Fixture {
    let dir = temp_dir(tag);
    // Two tenants: identities are minted before the server so the cross-tenant
    // principal has somewhere real to belong to.
    let bootstrap = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant_id = bootstrap.create_tenant("Acme").await;
    let other_tenant = bootstrap.create_tenant("Globex").await;
    let node = link_node(&bootstrap, &tenant_id, "Front office").await;
    let (status, body) = node
        .report_inventory(
            &bootstrap,
            json!([{"camera_id": "cam-lobby-01", "name": "Lobby", "status": "streaming", "recording": true}]),
        )
        .await;
    assert_eq!(status, 200, "seed inventory: {body}");
    bootstrap.stop().await;

    let principals = StaticPrincipals::new()
        .with(VIEWER, StaticPrincipals::viewer("user-viewer", &tenant_id))
        .with(
            OPERATOR,
            StaticPrincipals::operator("user-operator", &tenant_id),
        )
        .with(
            OTHER_TENANT,
            StaticPrincipals::operator("user-outsider", &other_tenant),
        );
    let server = TestServer::start_with(
        &dir.join("control.db"),
        Some(ADMIN_TOKEN),
        media,
        Some(std::sync::Arc::new(principals)),
    )
    .await;

    let (status, body) = server
        .put_admin(
            &format!("/admin/v1/tenants/{tenant_id}/media-policy"),
            &json!({
                "live_enabled": true,
                "allow_main_representation": true,
                "max_concurrent_sessions": 4,
                "max_bitrate_kbps": 2500,
            }),
        )
        .await;
    assert_eq!(status, 200, "set media policy: {body}");

    Fixture {
        server,
        tenant_id,
        node_id: node.node_id,
        camera_id: "cam-lobby-01".into(),
        dir,
    }
}

impl Fixture {
    async fn open_session(
        &self,
        credential: &str,
        representation: &str,
    ) -> (u16, serde_json::Value) {
        self.server
            .post_bearer(
                "/v1/media/live-sessions",
                credential,
                &json!({
                    "node_id": self.node_id,
                    "camera_id": self.camera_id,
                    "representation": representation,
                }),
            )
            .await
    }

    async fn consume(&self, grant: &str) -> (u16, serde_json::Value) {
        self.server
            .post_bearer(
                "/v1/media/grant-consumptions",
                SFU_TOKEN,
                &json!({"grant": grant}),
            )
            .await
    }

    async fn renew(&self, credential: &str, session_id: &str) -> (u16, serde_json::Value) {
        self.server
            .post_bearer(
                &format!("/v1/media/live-sessions/{session_id}/renewals"),
                credential,
                &json!({}),
            )
            .await
    }

    async fn audit(&self) -> Vec<serde_json::Value> {
        let (status, body) = self
            .server
            .get_admin(&format!(
                "/admin/v1/tenants/{}/media-audit-events",
                self.tenant_id
            ))
            .await;
        assert_eq!(status, 200, "read audit: {body}");
        body["events"].as_array().cloned().unwrap_or_default()
    }

    async fn finish(self) {
        self.server.stop().await;
        std::fs::remove_dir_all(self.dir).unwrap();
    }
}

fn reasons(events: &[serde_json::Value], event: &str) -> Vec<String> {
    events
        .iter()
        .filter(|row| row["event"] == event)
        .map(|row| {
            row["reason"]
                .as_str()
                .unwrap_or(row["outcome"].as_str().unwrap_or("?"))
                .to_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Issue and spend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_issues_a_grant_the_edge_can_spend_exactly_once() {
    let fixture = fixture("grant-once", MediaSetup::default()).await;

    let (status, handoff) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "open session: {handoff}");
    assert_eq!(handoff["protocol_version"], 1);
    let grant = handoff["grant"].as_str().expect("grant token").to_owned();
    let session_id = handoff["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    assert_eq!(handoff["sfu"]["region_id"], SFU_REGION);
    let livekit_token = handoff["livekit_access_token"]
        .as_str()
        .expect("LiveKit participant token");
    let claims: Vec<_> = livekit_token.split('.').collect();
    assert_eq!(claims.len(), 3);
    let claims: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claims[1])
            .expect("LiveKit claims"),
    )
    .expect("LiveKit claims JSON");
    assert_eq!(claims["video"]["room"], session_id);
    assert_eq!(claims["video"]["roomJoin"], true);
    assert_eq!(claims["video"]["canPublish"], false);
    assert_eq!(claims["video"]["canSubscribe"], true);
    assert_ne!(
        claims["sub"], ADMIN_TOKEN,
        "admin token is not browser identity"
    );
    assert!(
        handoff["sfu"]["ice_servers"][0]["credential"].is_string(),
        "an ephemeral TURN credential is part of the handoff: {handoff}"
    );
    assert_ne!(
        handoff["sfu"]["ice_servers"][0]["credential"], TURN_SECRET,
        "the long-lived TURN secret must never be handed out"
    );

    // The scope inside the token is exactly the one camera that was asked for.
    let payload = decode_grant_payload(&grant);
    assert_eq!(payload["scope"]["camera_id"], fixture.camera_id);
    assert_eq!(payload["scope"]["action"], "live");
    assert_eq!(payload["scope"]["representation"], "sub");
    assert_eq!(payload["scope"]["tenant_id"], fixture.tenant_id);
    assert_eq!(payload["audience"], SFU_REGION);

    // First spend succeeds and tells the edge what it may forward.
    let (status, spent) = fixture.consume(&grant).await;
    assert_eq!(status, 200, "first consumption: {spent}");
    assert_eq!(spent["session_id"], session_id);
    assert_eq!(spent["camera_id"], fixture.camera_id);
    assert_eq!(spent["representation"], "sub");

    // Replay is refused.
    let (status, replay) = fixture.consume(&grant).await;
    assert_eq!(status, 409, "replay: {replay}");
    assert_eq!(replay["error"], "grant_already_consumed");

    fixture.finish().await;
}

#[tokio::test]
async fn the_edge_boundary_rejects_a_forged_or_foreign_grant() {
    let fixture = fixture("grant-forged", MediaSetup::default()).await;
    let (_, handoff) = fixture.open_session(VIEWER, "sub").await;
    let grant = handoff["grant"].as_str().expect("grant").to_owned();

    // A tampered payload invalidates the signature.
    let mut segments: Vec<String> = grant.split('.').map(str::to_owned).collect();
    let mut payload = decode_grant_payload(&grant);
    payload["scope"]["camera_id"] = json!("cam-lobby-99");
    segments[1] = base64_url(&serde_json::to_vec(&payload).expect("json"));
    let forged = segments.join(".");
    let (status, body) = fixture.consume(&forged).await;
    assert_eq!(status, 403, "forged grant: {body}");
    assert_eq!(body["error"], "grant_signature_invalid");

    // The edge itself must authenticate: a browser credential is not an edge
    // credential, and neither is the administrator token.
    for credential in [VIEWER, ADMIN_TOKEN, "nonsense"] {
        let (status, body) = fixture
            .server
            .post_bearer(
                "/v1/media/grant-consumptions",
                credential,
                &json!({"grant": grant}),
            )
            .await;
        assert_eq!(status, 401, "{credential} as the edge: {body}");
    }

    fixture.finish().await;
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn signed_grant_payload(payload: &serde_json::Value) -> String {
    use ed25519_dalek::{Signer as _, SigningKey};

    let encoded = base64_url(&serde_json::to_vec(payload).expect("grant json"));
    let key = SigningKey::from_bytes(&GRANT_SIGNING_SEED);
    let signature = key.sign(format!("nwg1.{encoded}").as_bytes());
    format!("nwg1.{encoded}.{}", base64_url(&signature.to_bytes()))
}

fn database_connection(fixture: &Fixture) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(&fixture.server.database).expect("open test db");
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .expect("busy timeout");
    connection
}

/// Canonical whole-second UTC, the only spelling the store and the contract
/// accept. Stored expiries are compared as text, so a test that writes one must
/// write it the way the service would.
fn canonical_utc(offset_seconds: i64) -> (time::OffsetDateTime, String) {
    let at = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("whole seconds")
        + time::Duration::seconds(offset_seconds);
    let text = at
        .format(&time::format_description::well_known::Rfc3339)
        .expect("rfc3339");
    (at, text)
}

/// Move a live session's end, in the database the running server reads.
///
/// A test about the session boundary must not *wait* for one: sleeping through a
/// real hour is impossible and shortening the boundary at open time is not a
/// knob the service exposes. Ageing the row is deterministic and leaves every
/// server-side check intact.
fn set_session_end(fixture: &Fixture, session_id: &str, session_expires_at: &str) {
    let changed = database_connection(fixture)
        .execute(
            "UPDATE media_sessions SET session_expires_at=?2 WHERE session_id=?1",
            rusqlite::params![session_id, session_expires_at],
        )
        .expect("age the session");
    assert_eq!(changed, 1, "expected exactly one session row to age");
}

async fn set_policy(fixture: &Fixture, allow_main: bool, max_sessions: u32) {
    let (status, body) = fixture
        .server
        .put_admin(
            &format!("/admin/v1/tenants/{}/media-policy", fixture.tenant_id),
            &json!({
                "live_enabled": true,
                "allow_main_representation": allow_main,
                "max_concurrent_sessions": max_sessions,
                "max_bitrate_kbps": 2500,
            }),
        )
        .await;
    assert_eq!(status, 200, "set media policy: {body}");
}

/// The LiveKit participant token's claims, as a browser would read them.
fn livekit_claims(handoff: &serde_json::Value) -> serde_json::Value {
    let token = handoff["livekit_access_token"]
        .as_str()
        .expect("LiveKit participant token");
    let segments: Vec<&str> = token.split('.').collect();
    assert_eq!(segments.len(), 3, "compact JWT shape: {token}");
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(segments[1])
            .expect("LiveKit claims"),
    )
    .expect("LiveKit claims JSON")
}

// ---------------------------------------------------------------------------
// Identity boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_grant_is_issued_without_a_validated_principal() {
    // A deployment with the media edge configured but no identity provider
    // injected: the route exists and says so, rather than 404-ing or, worse,
    // falling back to some other credential.
    let dir = temp_dir("grant-no-identity");
    let server = TestServer::start_with(
        &dir.join("control.db"),
        Some(ADMIN_TOKEN),
        MediaSetup::default(),
        None,
    )
    .await;

    let body = json!({"node_id": "node", "camera_id": "cam", "representation": "sub"});
    let (status, answer) = server
        .post_bearer("/v1/media/live-sessions", "anything", &body)
        .await;
    assert_eq!(status, 503, "no identity provider: {answer}");
    assert_eq!(answer["error"], "identity_unavailable");

    // Including with the administrator credential, which is not a user.
    let (status, answer) = server
        .post_bearer("/v1/media/live-sessions", ADMIN_TOKEN, &body)
        .await;
    assert_eq!(status, 503, "admin token is not an identity: {answer}");

    // And with no credential at all.
    let (status, answer) = server.post("/v1/media/live-sessions", &body).await;
    assert_eq!(status, 503, "no credential: {answer}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_administrator_token_is_not_a_browser_identity() {
    let fixture = fixture("grant-admin-not-user", MediaSetup::default()).await;

    let (status, body) = fixture.open_session(ADMIN_TOKEN, "sub").await;
    assert_eq!(status, 401, "admin token as a viewer: {body}");

    let (status, body) = fixture.open_session("unknown-credential", "sub").await;
    assert_eq!(status, 401, "unknown credential: {body}");

    let (status, body) = fixture
        .server
        .post(
            "/v1/media/live-sessions",
            &json!({"node_id": fixture.node_id, "camera_id": fixture.camera_id, "representation": "sub"}),
        )
        .await;
    assert_eq!(status, 401, "no credential: {body}");

    fixture.finish().await;
}

#[tokio::test]
async fn an_identity_provider_cannot_be_configured_that_does_not_exist() {
    // There is no shipped verifier. A deployment that names one must fail to
    // start rather than silently run with no identity at all.
    let error = nw_api::ApiConfig::from_vars(|name| match name {
        nw_api::config::BIND_ENV => Some("127.0.0.1:0".into()),
        nw_api::config::DATABASE_ENV => Some("/tmp/nw-api-identity.db".into()),
        nw_api::config::IDENTITY_PROVIDER_ENV => Some("oidc".into()),
        _ => None,
    })
    .expect_err("an unimplemented identity provider must be refused");
    assert!(
        matches!(error, nw_api::ConfigError::Invalid { name, .. } if name == nw_api::config::IDENTITY_PROVIDER_ENV),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Tenant, camera and representation binding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_principal_cannot_open_a_session_on_another_tenants_camera() {
    let fixture = fixture("grant-cross-tenant", MediaSetup::default()).await;

    // Correct node id, correct camera id, wrong tenant: indistinguishable from
    // "no such camera".
    let (status, body) = fixture.open_session(OTHER_TENANT, "sub").await;
    assert_eq!(status, 404, "cross-tenant session: {body}");

    // A camera this tenant does not have is refused the same way.
    let (status, body) = fixture
        .server
        .post_bearer(
            "/v1/media/live-sessions",
            VIEWER,
            &json!({"node_id": fixture.node_id, "camera_id": "cam-lobby-99", "representation": "sub"}),
        )
        .await;
    assert_eq!(status, 404, "unknown camera: {body}");

    // The tenant's own principal is unaffected.
    let (status, body) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "own tenant: {body}");

    let denials = reasons(&fixture.audit().await, "grant_denied");
    assert!(
        denials.iter().any(|reason| reason == "unknown_camera"),
        "the denial is audited: {denials:?}"
    );

    fixture.finish().await;
}

#[tokio::test]
async fn the_main_representation_needs_both_policy_and_permission() {
    let fixture = fixture("grant-representation", MediaSetup::default()).await;

    // Policy allows `main`; the viewer's permissions do not.
    let (status, body) = fixture.open_session(VIEWER, "main").await;
    assert_eq!(status, 403, "viewer asking for main: {body}");
    assert_eq!(body["error"], "representation_not_permitted");

    // The operator may.
    let (status, body) = fixture.open_session(OPERATOR, "main").await;
    assert_eq!(status, 201, "operator asking for main: {body}");
    assert_eq!(
        decode_grant_payload(body["grant"].as_str().expect("grant"))["scope"]["representation"],
        "main"
    );

    // Turning it off in the persisted policy denies the operator too.
    let (status, body) = fixture
        .server
        .put_admin(
            &format!("/admin/v1/tenants/{}/media-policy", fixture.tenant_id),
            &json!({
                "live_enabled": true,
                "allow_main_representation": false,
                "max_concurrent_sessions": 4,
                "max_bitrate_kbps": 2500,
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = fixture.open_session(OPERATOR, "main").await;
    assert_eq!(status, 403, "main disabled by policy: {body}");

    // An action that has no media path is refused by name, not by parse error.
    let (status, body) = fixture
        .server
        .post_bearer(
            "/v1/media/live-sessions",
            OPERATOR,
            &json!({
                "node_id": fixture.node_id,
                "camera_id": fixture.camera_id,
                "representation": "sub",
                "action": "playback",
            }),
        )
        .await;
    assert_eq!(status, 501, "playback is not implemented: {body}");
    assert_eq!(body["error"], "action_not_supported");

    fixture.finish().await;
}

#[tokio::test]
async fn incompatible_simultaneous_representations_are_rejected_fail_closed() {
    let fixture = fixture("grant-representation-conflict", MediaSetup::default()).await;
    let (status, sub) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "sub session: {sub}");

    let (status, main) = fixture.open_session(OPERATOR, "main").await;
    assert_eq!(status, 403, "incompatible main session: {main}");
    assert_eq!(main["error"], "representation_not_permitted");

    let (status, another_sub) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(
        status, 201,
        "same-representation fan-out remains valid: {another_sub}"
    );
    fixture.finish().await;
}

#[tokio::test]
async fn a_tenant_without_a_persisted_policy_gets_nothing() {
    let dir = temp_dir("grant-no-policy");
    let bootstrap = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant_id = bootstrap.create_tenant("Acme").await;
    let node = link_node(&bootstrap, &tenant_id, "Front office").await;
    node.report_inventory(
        &bootstrap,
        json!([{"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": false}]),
    )
    .await;
    bootstrap.stop().await;

    let principals =
        StaticPrincipals::new().with(VIEWER, StaticPrincipals::viewer("user-viewer", &tenant_id));
    let server = TestServer::start_with(
        &dir.join("control.db"),
        Some(ADMIN_TOKEN),
        MediaSetup::default(),
        Some(std::sync::Arc::new(principals)),
    )
    .await;

    let (status, body) = server
        .post_bearer(
            "/v1/media/live-sessions",
            VIEWER,
            &json!({"node_id": node.node_id, "camera_id": "cam-1", "representation": "sub"}),
        )
        .await;
    assert_eq!(status, 403, "no policy means no live: {body}");
    assert_eq!(body["error"], "policy_missing");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

// ---------------------------------------------------------------------------
// Expiry, revocation and concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_expired_grant_cannot_be_spent() {
    let fixture = fixture("grant-expiry", MediaSetup::default().with_grant_ttl(1)).await;
    let (status, handoff) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{handoff}");
    let grant = handoff["grant"].as_str().expect("grant").to_owned();

    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let (status, body) = fixture.consume(&grant).await;
    assert_eq!(status, 410, "expired grant: {body}");
    assert_eq!(body["error"], "grant_expired");

    fixture.finish().await;
}

#[tokio::test]
async fn a_live_session_reissues_short_credentials_but_never_after_revocation() {
    let fixture = fixture("grant-renewal", MediaSetup::default().with_grant_ttl(1)).await;
    let (status, first) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{first}");
    let session_id = first["session_id"].as_str().unwrap().to_owned();
    let first_grant = first["grant"].as_str().unwrap().to_owned();

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let (status, renewed) = fixture.renew(VIEWER, &session_id).await;
    assert_eq!(status, 200, "renewal: {renewed}");
    assert_eq!(renewed["session_id"], session_id);
    assert_ne!(renewed["grant"], first_grant);
    assert_eq!(
        decode_grant_payload(renewed["grant"].as_str().unwrap())["scope"]["representation"],
        "sub"
    );

    let (status, _) = fixture
        .server
        .delete_bearer(&format!("/v1/media/live-sessions/{session_id}"), VIEWER)
        .await;
    assert_eq!(status, 204);
    let (status, body) = fixture.renew(VIEWER, &session_id).await;
    assert_eq!(status, 403, "revoked renewal: {body}");
    assert_eq!(body["error"], "session_revoked");

    fixture.finish().await;
}

#[tokio::test]
async fn renewal_is_time_bounded_by_the_session_and_frees_its_concurrency_slot() {
    let fixture = fixture("grant-renewal-session-expiry", MediaSetup::default()).await;
    let (status, first) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{first}");
    let session_id = first["session_id"].as_str().unwrap().to_owned();

    database_connection(&fixture)
        .execute(
            "UPDATE media_sessions SET session_expires_at='2000-01-01T00:00:00Z' WHERE session_id=?1",
            rusqlite::params![session_id],
        )
        .unwrap();
    let (status, body) = fixture.renew(VIEWER, &session_id).await;
    assert_eq!(status, 403, "expired session renewal: {body}");
    assert_eq!(body["error"], "session_revoked");

    let (status, replacement) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(
        status, 201,
        "an expired session must not consume concurrency: {replacement}"
    );
    fixture.finish().await;
}

/// Every expiring thing a renewal hands out is capped at the session's own end.
///
/// The grant TTL here (30s) is deliberately longer than what is left of the
/// session (20s), so the session bound — not the configured TTL — is what every
/// output must be derived from. Four independent credentials leave this route
/// (the viewer grant, the stored publisher grant, the TURN credential and the
/// LiveKit participant token) and a clamp that reached only some of them would
/// leave the others usable after the session ended.
#[tokio::test]
async fn renewed_viewer_publisher_turn_and_livekit_credentials_never_outlive_the_session() {
    let fixture = fixture(
        "renewal-all-credentials-clamped",
        MediaSetup::default().with_grant_ttl(30),
    )
    .await;
    let (status, first) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{first}");
    let session_id = first["session_id"].as_str().unwrap();
    let (session_end, session_end_text) = canonical_utc(20);
    set_session_end(&fixture, session_id, &session_end_text);

    let (status, renewed) = fixture.renew(VIEWER, session_id).await;
    assert_eq!(status, 200, "renewal: {renewed}");
    let viewer = decode_grant_payload(renewed["grant"].as_str().unwrap());
    assert_eq!(viewer["expires_at_utc"], session_end_text);
    assert_eq!(renewed["grant_expires_at_utc"], session_end_text);
    let stored_expiry: String = database_connection(&fixture)
        .query_row(
            "SELECT grant_expires_at FROM media_sessions WHERE session_id=?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_expiry, session_end_text,
        "publisher and viewer grants share the clamped expiry"
    );
    assert_eq!(
        renewed["sfu"]["ice_servers"][0]["username"]
            .as_str()
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        session_end.unix_timestamp(),
        "the TURN username carries the clamped expiry, not now + grant TTL"
    );
    let turn_ttl = renewed["sfu"]["ice_servers"][0]["credential_ttl_seconds"]
        .as_u64()
        .unwrap();
    assert!(
        turn_ttl <= 20,
        "the TURN credential outlives the session: {turn_ttl}s"
    );
    // A browser reads `max_duration_seconds` as "how long this may run", so a
    // value past the session end would have it hold a dead session open.
    let max_duration = renewed["max_duration_seconds"].as_u64().unwrap();
    assert!(
        max_duration > 0 && max_duration <= 20,
        "max_duration_seconds is not bounded by the session: {max_duration}"
    );
    // The LiveKit token is the one credential minted from a TTL rather than an
    // absolute instant, so it is the one that can drift past the clamp if the
    // TTL is computed from a stale clock read.
    let exp = livekit_claims(&renewed)["exp"].as_i64().unwrap();
    assert!(
        exp <= session_end.unix_timestamp(),
        "the LiveKit token outlives the session: exp {exp} > {}",
        session_end.unix_timestamp()
    );

    fixture.finish().await;
}

/// A renewal with too little of the session left to negotiate with is refused,
/// and refusing it must hand the concurrency slot back.
///
/// Issuing a two-second grant is worse than refusing: the browser spends its
/// renewal on credentials that expire mid-handshake, and the dying session goes
/// on occupying a slot its tenant paid for. So the control plane ends the
/// session itself rather than re-granting it.
#[tokio::test]
async fn a_renewal_too_short_to_be_usable_is_refused_and_frees_its_concurrency_slot() {
    let fixture = fixture(
        "renewal-unusably-short",
        MediaSetup::default().with_grant_ttl(30),
    )
    .await;
    set_policy(&fixture, true, 1).await;
    let (status, first) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{first}");
    let session_id = first["session_id"].as_str().unwrap().to_owned();

    // Still in the future — this is not the expired case — but far too little of
    // it left to be worth a credential.
    let (_, nearly_over) = canonical_utc(3);
    set_session_end(&fixture, &session_id, &nearly_over);

    let (status, body) = fixture.renew(VIEWER, &session_id).await;
    assert_eq!(status, 403, "unusably short renewal: {body}");
    assert_eq!(body["error"], "session_revoked");

    // The refusal ended the session, so the slot is free now rather than
    // whenever the old session would have run out.
    let (status, replacement) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(
        status, 201,
        "a session too short to renew must not keep holding its slot: {replacement}"
    );
    assert_ne!(replacement["session_id"], session_id.as_str());

    let revoked: Option<String> = database_connection(&fixture)
        .query_row(
            "SELECT revoked_at FROM media_sessions WHERE session_id=?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        revoked.is_some(),
        "the refused session was left alive and renewable"
    );
    assert!(
        reasons(&fixture.audit().await, "session_revoked")
            .contains(&"renewal_window_too_short".to_owned()),
        "the operator cannot tell this refusal from an ordinary revocation"
    );

    fixture.finish().await;
}

/// A session whose policy or permission no longer covers it can never be
/// renewed again, so holding its concurrency slot until it times out is a leak.
#[tokio::test]
async fn a_renewal_the_policy_no_longer_permits_frees_its_concurrency_slot() {
    let fixture = fixture("renewal-policy-denied-slot", MediaSetup::default()).await;
    set_policy(&fixture, true, 1).await;
    let (status, first) = fixture.open_session(OPERATOR, "main").await;
    assert_eq!(status, 201, "{first}");
    let session_id = first["session_id"].as_str().unwrap().to_owned();

    // The tenant withdraws the main representation. The live session that is
    // already running on it is now outside what the tenant permits.
    set_policy(&fixture, false, 1).await;

    let (status, body) = fixture.renew(OPERATOR, &session_id).await;
    assert_eq!(status, 403, "renewal outside policy: {body}");
    assert_eq!(body["error"], "representation_not_permitted");

    // Fail closed *and* let go: the tenant's single slot is usable again for
    // something the policy does permit.
    let (status, replacement) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(
        status, 201,
        "a session the policy no longer permits must not hold its slot: {replacement}"
    );

    assert!(
        reasons(&fixture.audit().await, "session_revoked")
            .contains(&"renewal_outside_policy".to_owned()),
        "the policy-driven end of a session was not audited"
    );

    fixture.finish().await;
}

#[tokio::test]
async fn adversarial_grant_denials_are_audited_without_presented_material() {
    let fixture = fixture(
        "grant-denial-audit",
        MediaSetup::default().with_grant_ttl(1),
    )
    .await;
    let (status, handoff) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{handoff}");
    let grant = handoff["grant"].as_str().expect("grant").to_owned();

    let mut forged_segments: Vec<String> = grant.split('.').map(str::to_owned).collect();
    let replacement = if forged_segments[2].starts_with('A') {
        "B"
    } else {
        "A"
    };
    forged_segments[2].replace_range(..1, replacement);
    let forged = forged_segments.join(".");
    let (status, body) = fixture.consume(&forged).await;
    assert_eq!(status, 403, "forged grant: {body}");

    let mut malformed_payload = decode_grant_payload(&grant);
    malformed_payload["scope"]["representation"] = json!("rtsp://user:secret@192.168.1.9/live");
    let malformed_scope = signed_grant_payload(&malformed_payload);
    let (status, body) = fixture.consume(&malformed_scope).await;
    assert_eq!(status, 400, "malformed scope: {body}");

    let (status, body) = fixture
        .open_session(VIEWER, "rtsp://user:secret@10.0.0.8/live")
        .await;
    assert_eq!(status, 400, "malformed representation: {body}");

    let connection = database_connection(&fixture);
    connection
        .execute_batch(
            "CREATE TRIGGER fail_malformed_open_audit BEFORE INSERT ON media_audit
             WHEN NEW.reason = 'malformed_grant'
             BEGIN SELECT RAISE(FAIL, 'forced audit failure'); END;",
        )
        .expect("install malformed-open audit failure");
    let (status, body) = fixture
        .server
        .post_bearer(
            "/v1/media/live-sessions",
            VIEWER,
            &json!({"node_id": fixture.node_id, "camera_id": fixture.camera_id}),
        )
        .await;
    assert_eq!(
        status, 500,
        "malformed open must fail closed on audit failure: {body}"
    );
    let response = reqwest::Client::new()
        .post(fixture.server.url("/v1/media/live-sessions"))
        .bearer_auth(VIEWER)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{not-json")
        .send()
        .await
        .expect("raw malformed request");
    assert_eq!(
        response.status().as_u16(),
        500,
        "invalid JSON must fail closed when its audit cannot persist"
    );
    connection
        .execute_batch("DROP TRIGGER fail_malformed_open_audit;")
        .expect("remove malformed-open audit failure");
    let (status, body) = fixture
        .server
        .post_bearer(
            "/v1/media/live-sessions",
            VIEWER,
            &json!({"node_id": fixture.node_id, "camera_id": fixture.camera_id}),
        )
        .await;
    assert_eq!(status, 400, "malformed open: {body}");

    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    let (status, body) = fixture.consume(&grant).await;
    assert_eq!(status, 410, "expired grant: {body}");

    let (_, fresh) = fixture.open_session(VIEWER, "sub").await;
    let fresh_grant = fresh["grant"].as_str().expect("fresh grant");
    assert_eq!(fixture.consume(fresh_grant).await.0, 200);
    let (status, body) = fixture.consume(fresh_grant).await;
    assert_eq!(status, 409, "replayed grant: {body}");

    // event, reason, principal, session
    type AuditRow = (String, Option<String>, Option<String>, Option<String>);
    let rows: Vec<AuditRow> = connection
        .prepare("SELECT event, reason, principal_id, session_id FROM media_audit ORDER BY rowid")
        .expect("prepare audit query")
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("query audit")
        .collect::<Result<_, _>>()
        .expect("audit rows");
    for expected in [
        "grant_signature_invalid",
        "malformed_grant",
        "grant_expired",
        "grant_already_consumed",
    ] {
        assert!(
            rows.iter()
                .any(|row| row.0 == "grant_denied" && row.1.as_deref() == Some(expected)),
            "missing safe denial {expected}: {rows:?}"
        );
    }
    let rendered = format!("{rows:?}");
    for forbidden in [
        &forged,
        &malformed_scope,
        VIEWER,
        "secret",
        "192.168",
        "10.0.0",
        "rtsp",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "audit leaked {forbidden}: {rendered}"
        );
    }
    assert!(rows.iter().all(|row| {
        row.0.len() <= 32
            && row.1.as_deref().map_or(true, |value| value.len() <= 64)
            && row.2.as_deref().map_or(true, |value| value.len() <= 128)
            && row.3.as_deref().map_or(true, |value| value.len() <= 128)
    }));

    drop(connection);
    fixture.finish().await;
}

#[tokio::test]
async fn audit_failure_rolls_back_session_issue_and_one_time_consumption() {
    let fixture = fixture("grant-audit-rollback", MediaSetup::default()).await;
    let connection = database_connection(&fixture);
    connection
        .execute_batch(
            "CREATE TRIGGER fail_grant_issue_audit BEFORE INSERT ON media_audit
             WHEN NEW.event = 'grant_issued' BEGIN SELECT RAISE(FAIL, 'forced audit failure'); END;",
        )
        .expect("install issue audit failure");

    let (status, body) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 500, "audit failure must fail issuance: {body}");
    let sessions: i64 = connection
        .query_row("SELECT count(*) FROM media_sessions", [], |row| row.get(0))
        .expect("count sessions");
    let opened_audits: i64 = connection
        .query_row(
            "SELECT count(*) FROM media_audit WHERE event IN ('session_opened','grant_issued')",
            [],
            |row| row.get(0),
        )
        .expect("count issue audits");
    assert_eq!(sessions, 0, "session insert survived its failed audit");
    assert_eq!(opened_audits, 0, "partial issue audit survived rollback");

    connection
        .execute_batch("DROP TRIGGER fail_grant_issue_audit;")
        .expect("remove issue audit failure");
    let (status, handoff) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "issue after trigger removal: {handoff}");
    let grant = handoff["grant"].as_str().expect("grant").to_owned();
    let session_id = handoff["session_id"].as_str().expect("session").to_owned();

    connection
        .execute_batch(
            "CREATE TRIGGER fail_grant_consumed_audit BEFORE INSERT ON media_audit
             WHEN NEW.event = 'grant_consumed' BEGIN SELECT RAISE(FAIL, 'forced audit failure'); END;",
        )
        .expect("install consumption audit failure");
    let (status, body) = fixture.consume(&grant).await;
    assert_eq!(status, 500, "audit failure must fail consumption: {body}");
    let consumed: Option<String> = connection
        .query_row(
            "SELECT consumed_at FROM media_sessions WHERE session_id=?1",
            [&session_id],
            |row| row.get(0),
        )
        .expect("read consumption state");
    assert_eq!(
        consumed, None,
        "one-time grant was burned without its audit"
    );

    connection
        .execute_batch("DROP TRIGGER fail_grant_consumed_audit;")
        .expect("remove consumption audit failure");
    let (status, body) = fixture.consume(&grant).await;
    assert_eq!(status, 200, "rolled-back grant remains spendable: {body}");

    drop(connection);
    fixture.finish().await;
}

#[tokio::test]
async fn stale_camera_authorization_cannot_create_a_session_after_node_unlink() {
    let fixture = fixture("grant-unlink-race", MediaSetup::default()).await;
    let store = nw_api::store::Store::open(&fixture.server.database).expect("open store");
    assert!(
        store
            .camera_exists(&fixture.tenant_id, &fixture.node_id, &fixture.camera_id)
            .expect("initial camera lookup"),
        "the route's preflight authorization initially succeeds"
    );
    let now = rfc3339(0);
    assert!(store
        .unlink_node(&fixture.tenant_id, &fixture.node_id, &now)
        .expect("unlink node"));

    let created = store
        .create_media_session(
            &fixture.tenant_id,
            "user-viewer",
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
            &fixture.node_id,
            &fixture.camera_id,
            "sub",
            &rfc3339(30),
            &rfc3339(3600),
            &now,
        )
        .expect("stale session decision");
    assert_eq!(
        created,
        nw_api::store::MediaSessionCreateOutcome::CameraUnavailable,
        "stale preflight created a session after unlink"
    );

    fixture.finish().await;
}

#[tokio::test]
async fn stale_policy_authorization_cannot_create_a_session() {
    let fixture = fixture("grant-policy-race", MediaSetup::default()).await;
    let store = nw_api::store::Store::open(&fixture.server.database).expect("open store");
    assert!(
        store
            .media_policy(&fixture.tenant_id)
            .expect("preflight policy")
            .expect("policy")
            .0
    );
    let now = rfc3339(0);
    store
        .upsert_media_policy(&fixture.tenant_id, false, true, 4, 2500, &now)
        .expect("disable live policy");

    let outcome = store
        .create_media_session(
            &fixture.tenant_id,
            "user-viewer",
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
            &fixture.node_id,
            &fixture.camera_id,
            "sub",
            &rfc3339(30),
            &rfc3339(3600),
            &now,
        )
        .expect("stale policy decision");
    assert_eq!(
        outcome,
        nw_api::store::MediaSessionCreateOutcome::CameraNotLiveEnabled,
        "stale live-enabled policy created a session"
    );

    fixture.finish().await;
}

#[tokio::test]
async fn audit_failure_rolls_back_node_unlink_and_its_session_revocations() {
    let fixture = fixture("grant-unlink-audit-rollback", MediaSetup::default()).await;
    let (status, handoff) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{handoff}");
    let grant = handoff["grant"].as_str().expect("grant").to_owned();
    let connection = database_connection(&fixture);
    connection
        .execute_batch(
            "CREATE TRIGGER fail_unlink_revocation_audit BEFORE INSERT ON media_audit
             WHEN NEW.event = 'session_revoked' AND NEW.reason = 'node_unlinked'
             BEGIN SELECT RAISE(FAIL, 'forced audit failure'); END;",
        )
        .expect("install unlink audit failure");

    let store = nw_api::store::Store::open(&fixture.server.database).expect("open store");
    assert!(
        store
            .unlink_node(&fixture.tenant_id, &fixture.node_id, &rfc3339(0))
            .is_err(),
        "unlink must fail when its revocation audit cannot be inserted"
    );
    let state: String = connection
        .query_row(
            "SELECT state FROM nodes WHERE tenant_id=?1 AND id=?2",
            [&fixture.tenant_id, &fixture.node_id],
            |row| row.get(0),
        )
        .expect("node state");
    assert_eq!(state, "linked", "node mutation survived failed audit");
    assert_eq!(
        fixture.consume(&grant).await.0,
        200,
        "session revocation survived failed audit"
    );

    drop(connection);
    fixture.finish().await;
}

#[tokio::test]
async fn revoking_a_session_kills_its_outstanding_grant() {
    let fixture = fixture("grant-revoke", MediaSetup::default()).await;
    let (status, handoff) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "{handoff}");
    let grant = handoff["grant"].as_str().expect("grant").to_owned();
    let session_id = handoff["session_id"].as_str().expect("session").to_owned();

    // Another tenant's principal cannot revoke it.
    let (status, body) = fixture
        .server
        .delete_bearer(
            &format!("/v1/media/live-sessions/{session_id}"),
            OTHER_TENANT,
        )
        .await;
    assert_eq!(status, 404, "cross-tenant revoke: {body}");

    let (status, body) = fixture
        .server
        .delete_bearer(&format!("/v1/media/live-sessions/{session_id}"), VIEWER)
        .await;
    assert_eq!(status, 204, "revoke: {body}");

    let (status, body) = fixture.consume(&grant).await;
    assert_eq!(status, 403, "grant after revoke: {body}");
    assert_eq!(body["error"], "session_revoked");

    let events = fixture.audit().await;
    assert!(
        events.iter().any(|row| row["event"] == "session_revoked"),
        "revocation is audited: {events:?}"
    );

    fixture.finish().await;
}

#[tokio::test]
async fn the_concurrent_session_cap_comes_from_persisted_policy() {
    let fixture = fixture("grant-concurrency", MediaSetup::default()).await;

    let (status, body) = fixture
        .server
        .put_admin(
            &format!("/admin/v1/tenants/{}/media-policy", fixture.tenant_id),
            &json!({
                "live_enabled": true,
                "allow_main_representation": true,
                "max_concurrent_sessions": 2,
                "max_bitrate_kbps": 2500,
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let mut sessions = Vec::new();
    for index in 0..2 {
        let (status, handoff) = fixture.open_session(VIEWER, "sub").await;
        assert_eq!(status, 201, "session {index}: {handoff}");
        sessions.push(handoff["session_id"].as_str().expect("session").to_owned());
    }

    let (status, body) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 429, "over the cap: {body}");
    assert_eq!(body["error"], "concurrent_session_limit");

    // Releasing one frees exactly one slot.
    let (status, body) = fixture
        .server
        .delete_bearer(&format!("/v1/media/live-sessions/{}", sessions[0]), VIEWER)
        .await;
    assert_eq!(status, 204, "{body}");

    let (status, body) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 201, "slot freed: {body}");
    let (status, body) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 429, "cap still holds: {body}");

    fixture.finish().await;
}

// ---------------------------------------------------------------------------
// Secrecy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nothing_about_the_customer_network_reaches_the_browser_or_the_database() {
    let fixture = fixture("grant-secrecy", MediaSetup::default()).await;
    let (status, handoff) = fixture.open_session(OPERATOR, "main").await;
    assert_eq!(status, 201, "{handoff}");
    let grant = handoff["grant"].as_str().expect("grant").to_owned();
    fixture.consume(&grant).await;

    let rendered = serde_json::to_string(&handoff).expect("render");
    let payload = serde_json::to_string(&decode_grant_payload(&grant)).expect("payload");
    for document in [&rendered, &payload] {
        assert!(
            nw_cloud_protocol::media_session::contains_local_material(document).is_none(),
            "site-local material reached the browser: {document}"
        );
        for forbidden in [
            "rtsp",
            "onvif",
            "/dev/shm",
            "control.sock",
            "192.168",
            "10.0.0",
        ] {
            assert!(
                !document.to_ascii_lowercase().contains(forbidden),
                "{forbidden} reached the browser: {document}"
            );
        }
    }

    // The grant payload's fields are an allowlist, not "whatever we had".
    let payload = decode_grant_payload(&grant);
    let mut keys: Vec<&str> = payload
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "audience",
            "expires_at_utc",
            "grant_id",
            "issued_at_utc",
            "max_bitrate_kbps",
            "not_before_utc",
            "protocol_version",
            "scope",
            "session_id",
        ]
    );

    // And the long-lived signing and TURN secrets are not in the database.
    let bytes = fixture.server.database_bytes();
    for secret in [
        TURN_SECRET.as_bytes(),
        SFU_TOKEN.as_bytes(),
        LIVEKIT_API_SECRET.as_bytes(),
    ] {
        assert!(
            !bytes.windows(secret.len()).any(|window| window == secret),
            "a long-lived secret was persisted"
        );
    }

    fixture.finish().await;
}

#[tokio::test]
async fn the_signing_key_route_publishes_only_the_public_half() {
    let fixture = fixture("grant-signing-key", MediaSetup::default()).await;
    let (status, body) = fixture.server.get("/v1/media/signing-key").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["algorithm"], "ed25519");
    assert_eq!(body["protocol_version"], 1);
    let public_key = body["public_key"].as_str().expect("public_key");
    assert_ne!(public_key, grant_signing_key(), "that is the private seed");
    fixture.finish().await;
}

#[tokio::test]
async fn the_media_surface_is_unavailable_until_an_edge_is_configured() {
    let fixture = fixture("grant-no-edge", MediaSetup::unconfigured()).await;

    let (status, body) = fixture.open_session(VIEWER, "sub").await;
    assert_eq!(status, 503, "no media edge: {body}");
    assert_eq!(body["error"], "media_edge_unavailable");

    let (status, body) = fixture.server.get("/v1/media/signing-key").await;
    assert_eq!(status, 503, "no signing key: {body}");

    fixture.finish().await;
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_grant_decision_is_audited_and_the_log_is_append_only() {
    let fixture = fixture("grant-audit", MediaSetup::default()).await;
    let (_, handoff) = fixture.open_session(VIEWER, "sub").await;
    let grant = handoff["grant"].as_str().expect("grant").to_owned();
    fixture.consume(&grant).await;
    fixture.consume(&grant).await;
    fixture.open_session(VIEWER, "main").await;

    let events = fixture.audit().await;
    for expected in [
        "session_opened",
        "grant_issued",
        "grant_consumed",
        "grant_denied",
    ] {
        assert!(
            events.iter().any(|row| row["event"] == expected),
            "missing {expected} in {events:?}"
        );
    }
    // Denials name the rule, not the value.
    let denials = reasons(&events, "grant_denied");
    assert!(
        denials.iter().any(|r| r == "representation_not_permitted"),
        "{denials:?}"
    );
    assert!(
        denials.iter().any(|r| r == "grant_already_consumed"),
        "{denials:?}"
    );
    // Audit rows carry the principal and never a credential.
    for row in &events {
        let rendered = serde_json::to_string(row).expect("render");
        assert!(
            !rendered.contains(VIEWER),
            "a credential was audited: {rendered}"
        );
        assert!(
            nw_cloud_protocol::media_session::contains_local_material(&rendered).is_none(),
            "{rendered}"
        );
    }

    // Another tenant cannot read this tenant's media audit trail.
    let (status, body) = fixture
        .server
        .get_admin("/admin/v1/tenants/00000000-0000-4000-8000-000000000000/media-audit-events")
        .await;
    assert_eq!(status, 404, "{body}");

    fixture.finish().await;
}
