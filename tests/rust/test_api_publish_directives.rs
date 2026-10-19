//! The node-facing publish-directive pull.
//!
//! A node has no inbound surface: the control plane cannot call it. So the node
//! asks, over the same signed, challenge-bound channel it already uses for
//! check-ins and inventory, whether any camera of its own must be published.
//!
//! These tests pin four properties:
//!
//! 1. **Opt-in.** A deployment with no media edge configured mints no directive
//!    and hands out no endpoint. There is no default.
//! 2. **Scope.** A directive names only opaque control-plane identifiers and the
//!    hosted SFU endpoint, and its grant is scoped to the pulling node.
//! 3. **One-time.** The authenticated pull is the spend: a session yields a
//!    directive exactly once, however many times the node asks.
//! 4. **Isolation.** A node sees its own obligations and nothing else.
//!
//! Nothing here contacts a LiveKit server and no test claims video flows.

mod api_harness;

use api_harness::{
    link_node, temp_dir, MediaSetup, StaticPrincipals, TestNode, TestServer, SFU_REGION,
    SFU_SIGNALING_BASE, SFU_TOKEN,
};
use nw_cloud_protocol::media_session::{MediaAction, PublishDirectiveBatch, Representation};
use serde_json::{json, Value};

const PUBLISH_DIRECTIVES_OPERATION: &str = "publish_directives";
const VIEWER_CREDENTIAL: &str = "viewer-credential-token";

fn principals(tenant_id: &str) -> std::sync::Arc<dyn nw_api::identity::PrincipalVerifier> {
    std::sync::Arc::new(StaticPrincipals::new().with(
        VIEWER_CREDENTIAL,
        StaticPrincipals::viewer("principal-1", tenant_id),
    ))
}

/// Ask for a directive batch the way the node client does.
async fn pull(server: &TestServer, node: &TestNode) -> (u16, Value) {
    pull_ack(server, node, Vec::new()).await
}

async fn pull_ack(
    server: &TestServer,
    node: &TestNode,
    acknowledgements: Vec<String>,
) -> (u16, Value) {
    let challenge = node.challenge(server, PUBLISH_DIRECTIVES_OPERATION).await;
    server
        .post(
            &format!("/v1/nodes/{}/publish-directives", node.node_id),
            &node.directive_proof_body(&challenge, acknowledgements),
        )
        .await
}

/// A linked node, a live-enabled tenant policy and one reported camera.
async fn ready(tag: &str) -> (TestServer, String, TestNode) {
    ready_with_media(tag, MediaSetup::default()).await
}

async fn ready_with_media(tag: &str, media: MediaSetup) -> (TestServer, String, TestNode) {
    let dir = temp_dir(tag);
    let database = dir.join("control.sqlite3");
    // The verifier's tenant is fixed at start-up, so the tenant is created,
    // then the server is restarted against the same database with a verifier
    // that knows it. Two starts, one database: exactly what a deployment does.
    let bootstrap = TestServer::start_with(
        &database,
        Some(api_harness::ADMIN_TOKEN),
        media.clone(),
        None,
    )
    .await;
    let tenant_id = bootstrap.create_tenant("Acme").await;
    bootstrap.stop().await;

    let server = TestServer::start_with(
        &database,
        Some(api_harness::ADMIN_TOKEN),
        media,
        Some(principals(&tenant_id)),
    )
    .await;
    let (status, body) = server
        .put_admin(
            &format!("/admin/v1/tenants/{tenant_id}/media-policy"),
            &json!({"live_enabled": true, "allow_main_representation": true,
                    "max_concurrent_sessions": 4, "max_bitrate_kbps": 2500}),
        )
        .await;
    assert_eq!(status, 200, "media policy: {body}");

    let node = link_node(&server, &tenant_id, "Front desk").await;
    let (status, body) = node
        .report_inventory(
            &server,
            json!([{"camera_id": "cam-lobby-01", "name": "Lobby", "status": "streaming", "recording": true}]),
        )
        .await;
    assert_eq!(status, 200, "inventory: {body}");
    (server, tenant_id, node)
}

#[tokio::test]
async fn an_expired_obligation_is_not_delivered_or_revived_by_a_later_pull() {
    let (server, _tenant, node) = ready_with_media(
        "publish-directives-expired",
        MediaSetup::default().with_grant_ttl(1),
    )
    .await;
    open_session(&server, &node, "sub").await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let (status, body) = pull(&server, &node).await;
    assert_eq!(status, 200, "expired pull: {body}");
    assert!(batch(&body).directives.is_empty(), "{body}");
    let (status, body) = pull(&server, &node).await;
    assert_eq!(status, 200, "replayed expired pull: {body}");
    assert!(batch(&body).directives.is_empty(), "{body}");

    server.stop().await;
}

/// Open a browser live session against the node's camera.
async fn open_session(server: &TestServer, node: &TestNode, representation: &str) -> String {
    let (status, body) = server
        .post_bearer(
            "/v1/media/live-sessions",
            VIEWER_CREDENTIAL,
            &json!({"node_id": node.node_id, "camera_id": "cam-lobby-01", "representation": representation}),
        )
        .await;
    assert_eq!(status, 201, "open live session: {body}");
    body["session_id"].as_str().expect("session_id").to_owned()
}

fn batch(body: &Value) -> PublishDirectiveBatch {
    serde_json::from_value(body.clone()).expect("the answer is a directive batch")
}

/// Spend a grant the way the SFU ingest edge does when a publisher arrives.
async fn spend(server: &TestServer, grant: &str) -> (u16, Value) {
    server
        .post_bearer(
            "/v1/media/grant-consumptions",
            SFU_TOKEN,
            &json!({"grant": grant}),
        )
        .await
}

/// A node that published successfully but never got its acknowledgement through
/// must not be told to publish the same thing again.
///
/// Redelivery is keyed on the obligation being outstanding, and spending the
/// grant at the ingest edge settles it just as an acknowledgement does. Without
/// that, the ordinary case of "published fine, ack lost" hands the node a
/// directive whose grant is already spent: it either publishes the camera twice
/// or fails at the edge, once per pull, for the life of the session.
#[tokio::test]
async fn a_spent_directive_is_not_redelivered_but_its_session_stays_active() {
    let (server, _tenant, node) = ready("publish-directives-spent").await;
    let session_id = open_session(&server, &node, "sub").await;

    let (status, delivered) = pull(&server, &node).await;
    assert_eq!(status, 200, "first pull: {delivered}");
    let grant = delivered["directives"][0]["grant"]
        .as_str()
        .expect("directive grant")
        .to_owned();

    let (status, spent) = spend(&server, &grant).await;
    assert_eq!(status, 200, "the ingest edge spends the grant: {spent}");
    assert_eq!(spent["session_id"], session_id.as_str());

    // No acknowledgement: this is precisely the lost-response case.
    let (status, after) = pull(&server, &node).await;
    assert_eq!(status, 200, "pull after publishing: {after}");
    assert!(
        batch(&after).directives.is_empty(),
        "a directive already spent was handed out again: {after}"
    );
    // Reconciliation is unchanged by that suppression: the node is still
    // publishing this session, so its absence here would read as cancellation.
    assert_eq!(
        batch(&after).active_session_ids,
        vec![session_id],
        "suppressing a spent directive must not cancel its session: {after}"
    );

    server.stop().await;
}

/// At-least-once delivery must not become at-least-once *authority*.
///
/// Every redelivery re-signs the same publish grant rather than minting a new
/// one, so however many copies of a directive reach the node, exactly one of
/// them can be spent at the ingest edge — one publication, not two.
#[tokio::test]
async fn redelivery_never_yields_a_second_spendable_publish_grant() {
    let (server, _tenant, node) = ready("publish-directives-single-authority").await;
    open_session(&server, &node, "sub").await;

    let (_, first) = pull(&server, &node).await;
    let (_, second) = pull(&server, &node).await;
    let first_grant = first["directives"][0]["grant"].as_str().unwrap().to_owned();
    let second_grant = second["directives"][0]["grant"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        api_harness::decode_grant_payload(&first_grant)["grant_id"],
        api_harness::decode_grant_payload(&second_grant)["grant_id"],
        "redelivery minted a second publish authority instead of re-signing the first"
    );

    let (status, body) = spend(&server, &first_grant).await;
    assert_eq!(status, 200, "first spend: {body}");
    let (status, body) = spend(&server, &second_grant).await;
    assert_eq!(status, 409, "second spend: {body}");
    assert_eq!(body["error"], "grant_already_consumed");

    server.stop().await;
}

/// An acknowledgement is only ever a claim about work the node was actually
/// given. One that names nothing it holds must change nothing, and one that is
/// not even an identifier must be refused outright.
#[tokio::test]
async fn an_unknown_or_malformed_acknowledgement_suppresses_nothing() {
    let (server, _tenant, node) = ready("publish-directives-bogus-ack").await;
    open_session(&server, &node, "sub").await;
    let (_, delivered) = pull(&server, &node).await;
    assert_eq!(batch(&delivered).directives.len(), 1);

    let invented = uuid::Uuid::new_v4().to_string();
    let (status, body) = pull_ack(&server, &node, vec![invented]).await;
    assert_eq!(status, 200, "ack for an invented directive: {body}");
    assert_eq!(
        batch(&body).directives.len(),
        1,
        "an invented acknowledgement suppressed real work: {body}"
    );

    let (status, body) = pull_ack(&server, &node, vec!["../../etc/passwd".into()]).await;
    assert_eq!(status, 400, "malformed ack: {body}");

    let (status, body) = pull(&server, &node).await;
    assert_eq!(status, 200, "pull: {body}");
    assert_eq!(
        batch(&body).directives.len(),
        1,
        "a refused acknowledgement still spent the obligation: {body}"
    );

    server.stop().await;
}

#[tokio::test]
async fn media_policy_cannot_exceed_a_complete_node_directive_batch() {
    let dir = temp_dir("publish-directives-policy-bound");
    let server =
        TestServer::start(&dir.join("control.sqlite3"), Some(api_harness::ADMIN_TOKEN)).await;
    let tenant_id = server.create_tenant("Acme").await;
    let (status, body) = server
        .put_admin(
            &format!("/admin/v1/tenants/{tenant_id}/media-policy"),
            &json!({"live_enabled": true, "allow_main_representation": true,
                    "max_concurrent_sessions": 65, "max_bitrate_kbps": 2500}),
        )
        .await;
    assert_eq!(
        status, 400,
        "an incomplete active set would make omission look like cancellation: {body}"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_node_is_told_to_publish_exactly_the_session_that_was_opened_for_it() {
    let (server, tenant_id, node) = ready("publish-directives").await;
    let session_id = open_session(&server, &node, "sub").await;

    let (status, body) = pull(&server, &node).await;
    assert_eq!(status, 200, "pull: {body}");
    let batch = batch(&body);
    batch.validate().expect("the batch validates as a contract");
    assert_eq!(batch.directives.len(), 1, "{body}");
    assert_eq!(batch.active_session_ids, vec![session_id.clone()]);

    let directive = &batch.directives[0];
    assert_eq!(directive.session_id, session_id);
    assert_eq!(directive.sfu.region_id, SFU_REGION);
    assert_eq!(directive.sfu.signaling_url, SFU_SIGNALING_BASE);

    // The grant the node is handed is scoped to the node itself, to that one
    // camera, to live, and to the representation the viewer was granted.
    let payload = api_harness::decode_grant_payload(&directive.grant);
    assert_eq!(payload["session_id"], session_id.as_str());
    assert_eq!(payload["audience"], SFU_REGION);
    assert_eq!(payload["scope"]["tenant_id"], tenant_id.as_str());
    assert_eq!(payload["scope"]["site_id"], node.node_id.as_str());
    assert_eq!(payload["scope"]["camera_id"], "cam-lobby-01");
    assert_eq!(
        payload["scope"]["action"],
        serde_json::to_value(MediaAction::Live).unwrap()
    );
    assert_eq!(
        payload["scope"]["representation"],
        serde_json::to_value(Representation::Sub).unwrap()
    );

    // The node's directive grant is its own, not the browser's: spending one
    // must not spend the other.
    assert_ne!(payload["grant_id"], Value::Null);

    // The SFU token is minted here, so the node holds no LiveKit secret: it
    // may publish into this session's room only (and, live, take a viewer's
    // talk track), and it lapses with the grant.
    let claims = jwt_claims(&directive.publisher_token);
    assert_eq!(claims["video"]["room"], session_id.as_str());
    assert_eq!(claims["video"]["roomJoin"], true);
    assert_eq!(claims["video"]["canPublish"], true);
    assert_eq!(claims["video"]["canSubscribe"], true);
    assert_eq!(claims["video"]["canUpdateOwnMetadata"], false);
    assert_eq!(
        claims["sub"],
        format!("node:{}:{}", node.node_id, session_id).as_str()
    );
    let expires = claims["exp"].as_i64().expect("exp");
    let grant_end = time::OffsetDateTime::parse(
        payload["expires_at_utc"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
    .unix_timestamp();
    assert!(
        expires <= grant_end + 1,
        "the token outlives its grant: {expires} > {grant_end}"
    );
    assert!(!body.to_string().contains(api_harness::LIVEKIT_API_SECRET));

    server.stop().await;
}

fn jwt_claims(token: &str) -> Value {
    use base64::Engine as _;
    let payload = token.split('.').nth(1).expect("a JWT");
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .expect("base64url");
    serde_json::from_slice(&raw).expect("JSON claims")
}

#[tokio::test]
async fn a_lost_pull_response_is_redelivered_until_authentically_acknowledged() {
    let (server, _tenant, node) = ready("publish-directives-once").await;
    open_session(&server, &node, "sub").await;

    let (status, first) = pull(&server, &node).await;
    assert_eq!(status, 200, "first pull: {first}");
    assert_eq!(batch(&first).directives.len(), 1);

    let (status, second) = pull(&server, &node).await;
    assert_eq!(status, 200, "second pull: {second}");
    assert_eq!(
        batch(&second).directives.len(),
        1,
        "lost response must be retried: {second}"
    );
    let directive_id = second["directives"][0]["directive_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, acknowledged) = pull_ack(&server, &node, vec![directive_id.clone()]).await;
    assert_eq!(status, 200, "acknowledgement: {acknowledged}");
    assert!(
        batch(&acknowledged).directives.is_empty(),
        "acknowledged work was redelivered"
    );

    let (status, replayed) = pull_ack(&server, &node, vec![directive_id]).await;
    assert_eq!(status, 200, "idempotent acknowledgement replay: {replayed}");
    assert!(batch(&replayed).directives.is_empty());

    server.stop().await;
}

#[tokio::test]
async fn acknowledgement_cannot_cross_nodes_or_suppress_a_renewed_directive() {
    let (server, tenant_id, node) = ready("publish-directives-malicious-ack").await;
    let session_id = open_session(&server, &node, "sub").await;
    let (_, first) = pull(&server, &node).await;
    let old_id = first["directives"][0]["directive_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let other = link_node(&server, &tenant_id, "Back door").await;
    let (status, attempted) = pull_ack(&server, &other, vec![old_id.clone()]).await;
    assert_eq!(status, 200, "cross-node ack: {attempted}");
    let (_, still_pending) = pull(&server, &node).await;
    assert_eq!(
        batch(&still_pending).directives.len(),
        1,
        "another node suppressed the directive"
    );

    let (status, renewed) = server
        .post_bearer(
            &format!("/v1/media/live-sessions/{session_id}/renewals"),
            VIEWER_CREDENTIAL,
            &json!({}),
        )
        .await;
    assert_eq!(status, 200, "renewal: {renewed}");
    let (status, after_stale_ack) = pull_ack(&server, &node, vec![old_id]).await;
    assert_eq!(status, 200, "stale ack: {after_stale_ack}");
    assert_eq!(
        batch(&after_stale_ack).directives.len(),
        1,
        "stale ack suppressed renewed grant"
    );
    assert_ne!(
        after_stale_ack["directives"][0]["directive_id"],
        first["directives"][0]["directive_id"]
    );

    server.stop().await;
}

#[tokio::test]
async fn a_revoked_session_is_never_delivered() {
    let (server, _tenant, node) = ready("publish-directives-revoked").await;
    let session_id = open_session(&server, &node, "sub").await;
    let (status, body) = server
        .delete_bearer(
            &format!("/v1/media/live-sessions/{session_id}"),
            VIEWER_CREDENTIAL,
        )
        .await;
    assert_eq!(status, 204, "revoke: {body}");

    let (status, body) = pull(&server, &node).await;
    assert_eq!(status, 200, "pull: {body}");
    assert!(batch(&body).directives.is_empty(), "{body}");
    assert!(batch(&body).active_session_ids.is_empty(), "{body}");

    server.stop().await;
}

#[tokio::test]
async fn a_node_never_sees_another_nodes_obligation() {
    let (server, tenant_id, node) = ready("publish-directives-isolation").await;
    open_session(&server, &node, "sub").await;

    let other = link_node(&server, &tenant_id, "Back door").await;
    let (status, body) = pull(&server, &other).await;
    assert_eq!(status, 200, "pull: {body}");
    assert!(
        batch(&body).directives.is_empty(),
        "another node's session leaked: {body}"
    );

    server.stop().await;
}

#[tokio::test]
async fn an_unconfigured_deployment_hands_out_no_directive_and_no_endpoint() {
    let dir = temp_dir("publish-directives-unconfigured");
    let database = dir.join("control.sqlite3");
    let server = TestServer::start(&database, Some(api_harness::ADMIN_TOKEN)).await;
    let tenant_id = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant_id, "Front desk").await;

    let (status, body) = pull(&server, &node).await;
    assert_eq!(
        status, 503,
        "an unconfigured media edge must fail closed: {body}"
    );
    assert_eq!(body["error"], "media_edge_unavailable");
    let text = body.to_string();
    assert!(
        !text.contains("wss:") && !text.contains("https://"),
        "no endpoint may be invented for an unconfigured deployment: {text}"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_pull_without_a_matching_challenge_is_refused() {
    let (server, _tenant, node) = ready("publish-directives-auth").await;
    open_session(&server, &node, "sub").await;

    // A challenge minted for a different operation must not open this one.
    let challenge = node
        .challenge(&server, api_harness::CHECK_IN_OPERATION)
        .await;
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/publish-directives", node.node_id),
            &node.proof_body(&challenge),
        )
        .await;
    assert_eq!(status, 401, "cross-operation challenge: {body}");

    // And an unsigned body is refused before any payload is looked at.
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/publish-directives", node.node_id),
            &json!({"protocol_version": 1, "tenant_id": node.tenant_id, "node_id": node.node_id,
                    "audience": api_harness::NODE_AUDIENCE, "auth_epoch": node.auth_epoch,
                    "issued_at": api_harness::rfc3339(0), "expires_at": api_harness::rfc3339(60),
                    "challenge": "not-a-challenge", "signature": "not-a-signature"}),
        )
        .await;
    assert_eq!(status, 401, "unsigned pull: {body}");

    // The refused pulls must not have spent the obligation.
    let (status, body) = pull(&server, &node).await;
    assert_eq!(status, 200, "pull: {body}");
    assert_eq!(batch(&body).directives.len(), 1, "{body}");

    server.stop().await;
}

#[tokio::test]
async fn a_delivered_directive_carries_no_site_local_material() {
    let (server, _tenant, node) = ready("publish-directives-secrecy").await;
    open_session(&server, &node, "sub").await;
    let (status, body) = pull(&server, &node).await;
    assert_eq!(status, 200, "pull: {body}");

    let text = body.to_string();
    assert!(
        nw_cloud_protocol::media_session::contains_local_material(&text).is_none(),
        "the answer carries site-local material: {text}"
    );
    // The stored control-plane state gains no local material either.
    let bytes = server.database_bytes();
    let haystack = String::from_utf8_lossy(&bytes);
    assert!(
        !haystack.contains("rtsp://"),
        "a stream URL reached the control database"
    );

    server.stop().await;
}
