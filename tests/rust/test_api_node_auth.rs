//! Authenticated node identity: only a linked node holding its private key can
//! obtain a challenge, check in or report anything.

mod api_harness;

use api_harness::*;
use serde_json::json;

#[tokio::test]
async fn a_check_in_from_an_unauthenticated_caller_is_denied() {
    let dir = temp_dir("unauthenticated");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;
    let challenge = node.challenge(&server, CHECK_IN_OPERATION).await;

    // An empty signature.
    let mut unsigned = node.proof_body(&challenge);
    unsigned["signature"] = json!("");
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &unsigned,
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // A signature over a different body: one field edited after signing.
    let mut tampered = node.proof_body(&challenge);
    tampered["auth_epoch"] = json!(1);
    tampered["issued_at"] = json!(rfc3339(-1));
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &tampered,
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // A signature from a key the control plane has never seen.
    let stranger = TestNode::new(&tenant);
    let mut forged = stranger.proof_body(&challenge);
    forged["node_id"] = json!(node.node_id);
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &forged,
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // The node itself still works, so the denials are about authentication.
    let (status, body) = node.check_in(&server).await;
    assert_eq!(status, 200, "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_unknown_node_is_not_told_anything() {
    let dir = temp_dir("unknown-node");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let stranger = TestNode::new(&tenant);

    let (status, body) = server
        .post(
            "/v1/node-auth/challenges",
            &stranger.challenge_body(CHECK_IN_OPERATION),
        )
        .await;
    assert_eq!(status, 404, "{body}");
    // Neither the field name nor the caller's own key is echoed, so the answer
    // cannot be used to confirm which part of the request was recognised.
    let text = body.to_string();
    assert!(!text.contains("public_key"), "{body}");
    assert!(!text.contains(&stranger.public_key()), "{body}");
    assert!(!text.contains(&stranger.node_id), "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_challenge_is_spendable_exactly_once() {
    let dir = temp_dir("challenge-once");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    let challenge = node.challenge(&server, CHECK_IN_OPERATION).await;
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &node.proof_body(&challenge),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &node.proof_body(&challenge),
        )
        .await;
    assert_eq!(status, 401, "a spent challenge: {body}");

    // A challenge nobody issued is equally useless.
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &node.proof_body("invented-challenge-000000000000001"),
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // A challenge issued for one operation cannot be spent on another.
    let inventory_challenge = node.challenge(&server, INVENTORY_OPERATION).await;
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &node.proof_body(&inventory_challenge),
        )
        .await;
    assert_eq!(status, 401, "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_replayed_challenge_request_nonce_is_refused() {
    let dir = temp_dir("nonce-replay");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    let request = node.challenge_body(CHECK_IN_OPERATION);
    let (status, first) = server.post("/v1/node-auth/challenges", &request).await;
    assert!((200..300).contains(&status), "{first}");

    let (status, body) = server.post("/v1/node-auth/challenges", &request).await;
    assert_eq!(status, 409, "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_stale_auth_epoch_or_an_expired_window_is_refused() {
    let dir = temp_dir("epoch-window");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let mut node = link_node(&server, &tenant, "Front office").await;

    node.auth_epoch = 2;
    let (status, body) = server
        .post(
            "/v1/node-auth/challenges",
            &node.challenge_body(CHECK_IN_OPERATION),
        )
        .await;
    assert_eq!(status, 401, "an epoch the node was never granted: {body}");
    node.auth_epoch = 1;

    let mut stale = node.challenge_body(CHECK_IN_OPERATION);
    stale["issued_at"] = json!(rfc3339(-600));
    stale["expires_at"] = json!(rfc3339(-540));
    let stale = signed(&node.key, stale);
    let (status, body) = server.post("/v1/node-auth/challenges", &stale).await;
    assert_eq!(status, 410, "{body}");

    // A window long enough to be replayed for a day is refused as well.
    let mut long = node.challenge_body(CHECK_IN_OPERATION);
    long["issued_at"] = json!(rfc3339(0));
    long["expires_at"] = json!(rfc3339(86_400));
    let long = signed(&node.key, long);
    let (status, body) = server.post("/v1/node-auth/challenges", &long).await;
    assert_eq!(status, 400, "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_successful_check_in_answers_exactly_what_the_node_validates() {
    let dir = temp_dir("check-in");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    // The challenge answer must deserialise into the node's
    // `NodeChallengeResponse`, which denies unknown fields.
    let (status, challenge) = server
        .post(
            "/v1/node-auth/challenges",
            &node.challenge_body(CHECK_IN_OPERATION),
        )
        .await;
    assert!((200..300).contains(&status), "{challenge}");
    let fields: Vec<&String> = challenge.as_object().unwrap().keys().collect();
    assert_eq!(
        fields,
        vec![
            "audience",
            "challenge",
            "expires_at",
            "operation",
            "protocol_version"
        ],
        "the node rejects any extra field"
    );
    assert_eq!(challenge["audience"], NODE_AUDIENCE);
    assert_eq!(challenge["operation"], CHECK_IN_OPERATION);
    assert_eq!(challenge["protocol_version"], 1);

    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", node.node_id),
            &node.proof_body(challenge["challenge"].as_str().unwrap()),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["node_id"], node.node_id);
    assert_eq!(body["auth_epoch"], 1);
    // The node treats any of these as a failed check-in (`check_in_once` in
    // `crates/nw-backend/src/cloud/mod.rs`), so the answer must avoid all of
    // them, not merely differ from one.
    assert!(
        !matches!(
            body["status"].as_str(),
            Some("revoked" | "disabled" | "stale")
        ),
        "the node rejects this status: {body}"
    );
    assert_ne!(body["revoked"].as_bool(), Some(true), "{body}");
    let checked_in = body["checked_in_at"].as_str().expect("checked_in_at");
    let parsed =
        time::OffsetDateTime::parse(checked_in, &time::format_description::well_known::Rfc3339)
            .expect("the node parses this as RFC 3339");
    assert_eq!(parsed.offset(), time::UtcOffset::UTC);

    let (_, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{tenant}/nodes"))
        .await;
    assert_eq!(nodes["nodes"][0]["last_check_in"], checked_in);

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_unlink_indication_retires_the_node() {
    let dir = temp_dir("unlink");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;
    node.report_inventory(
        &server,
        json!([{"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": false}]),
    )
    .await;

    let challenge = node.challenge(&server, UNLINK_OPERATION).await;
    let response = reqwest::Client::new()
        .post(server.url(&format!("/v1/nodes/{}/unlink-indications", node.node_id)))
        .json(&node.proof_body(&challenge))
        .send()
        .await
        .unwrap();
    // The node insists on exactly 204 here.
    assert_eq!(response.status().as_u16(), 204);
    assert!(response.bytes().await.unwrap().is_empty());

    let (status, body) = server
        .post(
            "/v1/node-auth/challenges",
            &node.challenge_body(CHECK_IN_OPERATION),
        )
        .await;
    assert_eq!(status, 404, "an unlinked node has no identity left: {body}");

    let (status, cameras) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{tenant}/nodes/{}/cameras",
            node.node_id
        ))
        .await;
    assert_eq!(status, 200, "{cameras}");
    assert!(cameras["cameras"].as_array().unwrap().is_empty());

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// The node persists its pending unlink to disk and clears it *only* after it
/// sees exactly `204` (`retry_unlink` / `post_no_content` in
/// `crates/nw-backend/src/cloud/mod.rs`). If that answer is lost — a dropped
/// connection, or the control plane dying between the commit and the response —
/// the node asks for a fresh unlink challenge and posts again, on every
/// scheduler tick, across restarts, with no give-up path. So an unlink that
/// already happened has to be completable a second time.
#[tokio::test]
async fn a_lost_unlink_answer_can_be_retried_to_completion() {
    let dir = temp_dir("unlink-retry");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    let unlink = |challenge: String| {
        let url = server.url(&format!("/v1/nodes/{}/unlink-indications", node.node_id));
        let body = node.proof_body(&challenge);
        async move {
            reqwest::Client::new()
                .post(url)
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };

    let challenge = node.challenge(&server, UNLINK_OPERATION).await;
    assert_eq!(unlink(challenge).await.status().as_u16(), 204);

    // The node never saw that answer, so it repeats the whole sequence.
    let challenge = node.challenge(&server, UNLINK_OPERATION).await;
    let retried = unlink(challenge).await;
    assert_eq!(
        retried.status().as_u16(),
        204,
        "a retried unlink must settle, or the node retries for ever"
    );
    assert!(retried.bytes().await.unwrap().is_empty());

    // Retirement is still final for everything else: the key that may still
    // finish an unlink may not check in or report cameras.
    for operation in [CHECK_IN_OPERATION, INVENTORY_OPERATION] {
        let (status, body) = server
            .post("/v1/node-auth/challenges", &node.challenge_body(operation))
            .await;
        assert_eq!(status, 404, "a retired node may not {operation}: {body}");
    }

    let (_, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{tenant}/nodes"))
        .await;
    assert_eq!(nodes["nodes"][0]["linked"], false, "still retired");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}
