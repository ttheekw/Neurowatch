//! Node enrollment: the one-time, expiry-bound transfer proof.

mod api_harness;

use api_harness::*;
use serde_json::json;

async fn claim_parameters(
    server: &TestServer,
    enrollment: &Enrollment,
    proof: &str,
) -> (u16, serde_json::Value) {
    server
        .post(
            &format!(
                "/v1/node-enrollments/{}/claim-parameters",
                enrollment.enrollment_id
            ),
            &json!({"tenant_id": enrollment.tenant_id, "transfer_proof": proof}),
        )
        .await
}

#[tokio::test]
async fn the_enrollment_code_matches_the_shape_the_node_parses() {
    let dir = temp_dir("code-shape");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;

    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;

    // `EnrollmentCode::parse` in the node: canonical UUIDs and an 8..=128
    // character url-safe proof.
    assert_eq!(enrollment.tenant_id, tenant);
    assert_eq!(
        uuid::Uuid::parse_str(&enrollment.enrollment_id)
            .unwrap()
            .to_string(),
        enrollment.enrollment_id
    );
    assert!((8..=128).contains(&enrollment.transfer_proof.len()));
    assert!(enrollment
        .transfer_proof
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_claim_parameters_answer_is_the_binding_the_node_expects() {
    let dir = temp_dir("claim-parameters");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;

    let (status, body) = claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["protocol_version"], 1);
    assert_eq!(body["audience"], ENROLLMENT_AUDIENCE);
    assert_eq!(body["enrollment_id"], enrollment.enrollment_id);
    assert_eq!(body["tenant_id"], enrollment.tenant_id);
    // The node requires a challenge of at least 32 characters and a canonical
    // UTC expiry in the future.
    assert!(body["challenge"].as_str().unwrap().len() >= 32);
    let expires = time::OffsetDateTime::parse(
        body["expires_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert!(expires > time::OffsetDateTime::now_utc());
    assert_eq!(expires.offset(), time::UtcOffset::UTC);
    // Nothing about the proof comes back out.
    assert!(!body.to_string().contains(&enrollment.transfer_proof));

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_wrong_transfer_proof_is_refused_and_leaves_the_enrollment_claimable() {
    let dir = temp_dir("wrong-proof");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;

    let (status, body) = claim_parameters(&server, &enrollment, "not-the-real-proof").await;
    assert_eq!(status, 401, "{body}");

    let (status, _) = claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;
    assert_eq!(status, 200, "a failed guess must not burn the enrollment");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_transfer_proof_is_single_use_and_a_replayed_claim_is_refused() {
    let dir = temp_dir("single-use");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;
    let (_, parameters) = claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;

    let first = TestNode::new(&tenant);
    let (status, claim) = server
        .post(
            &format!("/v1/node-enrollments/{}/claim", enrollment.enrollment_id),
            &first.claim_body(&enrollment, &parameters, "Front office"),
        )
        .await;
    assert_eq!(status, 200, "{claim}");
    assert!(uuid::Uuid::parse_str(claim["claim_id"].as_str().unwrap()).is_ok());

    // A second node replaying the same code cannot claim the enrollment.
    let attacker = TestNode::new(&tenant);
    let (status, body) = server
        .post(
            &format!("/v1/node-enrollments/{}/claim", enrollment.enrollment_id),
            &attacker.claim_body(&enrollment, &parameters, "Front office"),
        )
        .await;
    assert_eq!(status, 409, "{body}");

    // And the proof no longer buys the binding either.
    let (status, body) = claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;
    assert_eq!(status, 409, "{body}");

    // Only the first node exists.
    let (status, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{tenant}/nodes"))
        .await;
    assert_eq!(status, 200, "{nodes}");
    assert_eq!(
        nodes["nodes"].as_array().unwrap().len(),
        0,
        "not confirmed yet"
    );

    let (status, confirmation) = server
        .post(
            &format!("/v1/node-enrollments/{}/confirm", enrollment.enrollment_id),
            &first.confirm_body(
                &enrollment,
                &parameters,
                claim["claim_id"].as_str().unwrap(),
            ),
        )
        .await;
    assert_eq!(status, 200, "{confirmation}");
    assert!(matches!(
        confirmation["status"].as_str(),
        Some("confirmed" | "linked")
    ));
    assert_eq!(confirmation["node_id"], first.node_id);

    let (_, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{tenant}/nodes"))
        .await;
    let listed = nodes["nodes"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["node_id"], first.node_id);
    assert_eq!(listed[0]["auth_epoch"], 1);

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_expired_enrollment_cannot_be_claimed() {
    let dir = temp_dir("expired");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    // A comfortable lifetime, so the binding below is fetched while the
    // enrollment is unambiguously live however loaded the machine is; expiry is
    // then forced rather than waited for. A short lifetime makes this test race
    // its own setup, and a late claim-parameters answer carries no binding, so
    // the claim that follows is malformed and refused as `invalid_request`
    // before the server ever reaches the expiry check.
    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;
    let (status, parameters) =
        claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;
    assert_eq!(
        status, 200,
        "the binding must be issued while live: {parameters}"
    );
    assert!(parameters["expires_at"].is_string());
    assert!(parameters["challenge"].is_string());

    server.expire_enrollment(&enrollment);

    let (status, body) = claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;
    assert_eq!(status, 410, "{body}");
    assert_eq!(body["error"], "expired", "{body}");

    // A well-formed claim against the binding the enrollment really issued: the
    // only thing wrong with it is that the enrollment has expired, so the
    // answer must be `expired`, not a validation complaint.
    let node = TestNode::new(&tenant);
    let (status, body) = server
        .post(
            &format!("/v1/node-enrollments/{}/claim", enrollment.enrollment_id),
            &node.claim_body(&enrollment, &parameters, "Front office"),
        )
        .await;
    assert_eq!(status, 410, "{body}");
    assert_eq!(body["error"], "expired", "{body}");

    let (_, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{tenant}/nodes"))
        .await;
    assert!(nodes["nodes"].as_array().unwrap().is_empty());

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_claim_signed_by_another_key_is_refused() {
    let dir = temp_dir("claim-signature");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;
    let (_, parameters) = claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;

    let node = TestNode::new(&tenant);
    let other = TestNode::new(&tenant);
    let mut body = node.claim_body(&enrollment, &parameters, "Front office");
    // The right proof, the right node id, somebody else's public key.
    body["public_key"] = json!(other.public_key());

    let (status, response) = server
        .post(
            &format!("/v1/node-enrollments/{}/claim", enrollment.enrollment_id),
            &body,
        )
        .await;
    assert_eq!(status, 401, "{response}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn confirmation_requires_the_claiming_key_and_the_issued_claim_id() {
    let dir = temp_dir("confirm");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;
    let (_, parameters) = claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;
    let node = TestNode::new(&tenant);
    let (_, claim) = server
        .post(
            &format!("/v1/node-enrollments/{}/claim", enrollment.enrollment_id),
            &node.claim_body(&enrollment, &parameters, "Front office"),
        )
        .await;
    let claim_id = claim["claim_id"].as_str().unwrap().to_owned();

    // A different claim id.
    let (status, body) = server
        .post(
            &format!("/v1/node-enrollments/{}/confirm", enrollment.enrollment_id),
            &node.confirm_body(&enrollment, &parameters, &uuid::Uuid::new_v4().to_string()),
        )
        .await;
    assert_eq!(status, 409, "{body}");

    // The right claim id, the wrong key: an impostor with the same node id.
    let mut impostor = TestNode::new(&tenant);
    impostor.node_id = node.node_id.clone();
    let (status, body) = server
        .post(
            &format!("/v1/node-enrollments/{}/confirm", enrollment.enrollment_id),
            &impostor.confirm_body(&enrollment, &parameters, &claim_id),
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // The real node still can.
    let (status, body) = server
        .post(
            &format!("/v1/node-enrollments/{}/confirm", enrollment.enrollment_id),
            &node.confirm_body(&enrollment, &parameters, &claim_id),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_database_stores_a_hash_of_the_proof_and_never_the_proof() {
    let dir = temp_dir("proof-hash");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let enrollment = server.create_enrollment(&tenant, "Front office", 900).await;
    let _ = link_node(&server, &tenant, "Back office").await;

    let bytes = server.database_bytes();
    assert!(
        !bytes.is_empty(),
        "the database file exists and has content"
    );
    let found = bytes
        .windows(enrollment.transfer_proof.len())
        .any(|window| window == enrollment.transfer_proof.as_bytes());
    assert!(!found, "the one-time proof must never be written to disk");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// `node_id` is chosen by whoever holds the enrollment code, and the control
/// plane only checks that it is a canonical UUID. Two enrollments in one tenant
/// naming the same node must therefore be a refusal the caller can understand —
/// not a storage fault, and never a way to take over a node that is already
/// registered under a different key.
#[tokio::test]
async fn a_node_id_already_registered_in_the_tenant_is_refused_cleanly() {
    let dir = temp_dir("duplicate-node-id");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let registered = link_node(&server, &tenant, "Front office").await;

    // A second, entirely valid enrollment, claimed by a different key that
    // names the node id the tenant already registered.
    let enrollment = server.create_enrollment(&tenant, "Back office", 900).await;
    let mut impostor = TestNode::new(&tenant);
    impostor.node_id = registered.node_id.clone();

    let (status, parameters) =
        claim_parameters(&server, &enrollment, &enrollment.transfer_proof).await;
    assert_eq!(status, 200, "{parameters}");
    let (status, claim) = server
        .post(
            &format!("/v1/node-enrollments/{}/claim", enrollment.enrollment_id),
            &impostor.claim_body(&enrollment, &parameters, "Back office"),
        )
        .await;
    assert_eq!(status, 200, "{claim}");
    let claim_id = claim["claim_id"].as_str().expect("claim_id");

    let (status, body) = server
        .post(
            &format!("/v1/node-enrollments/{}/confirm", enrollment.enrollment_id),
            &impostor.confirm_body(&enrollment, &parameters, claim_id),
        )
        .await;
    assert_eq!(
        status, 409,
        "a node id already in use is a conflict, not a fault: {body}"
    );

    // The registered node is untouched: one node, its original label, and its
    // own key still authenticates.
    let (_, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{tenant}/nodes"))
        .await;
    assert_eq!(nodes["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(nodes["nodes"][0]["display_label"], "Front office");
    let (status, body) = registered.check_in(&server).await;
    assert_eq!(status, 200, "the registered node still works: {body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}
