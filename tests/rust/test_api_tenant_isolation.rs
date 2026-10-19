//! Tenant boundaries. Nothing belonging to one tenant is reachable, readable
//! or usable through another tenant's identifiers.

mod api_harness;

use api_harness::*;
use serde_json::json;

#[tokio::test]
async fn an_enrollment_cannot_be_claimed_under_another_tenant() {
    let dir = temp_dir("cross-enrollment");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let acme = server.create_tenant("Acme").await;
    let other = server.create_tenant("Globex").await;
    let enrollment = server.create_enrollment(&acme, "Front office", 900).await;

    // The right enrollment id and the right proof, the wrong tenant.
    let (status, body) = server
        .post(
            &format!(
                "/v1/node-enrollments/{}/claim-parameters",
                enrollment.enrollment_id
            ),
            &json!({"tenant_id": other, "transfer_proof": enrollment.transfer_proof}),
        )
        .await;
    assert_eq!(status, 404, "{body}");

    // And the enrollment is untouched: its real tenant can still use it.
    let (status, _) = server
        .post(
            &format!(
                "/v1/node-enrollments/{}/claim-parameters",
                enrollment.enrollment_id
            ),
            &json!({"tenant_id": acme, "transfer_proof": enrollment.transfer_proof}),
        )
        .await;
    assert_eq!(status, 200);

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_node_cannot_authenticate_under_another_tenant() {
    let dir = temp_dir("cross-node");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let acme = server.create_tenant("Acme").await;
    let globex = server.create_tenant("Globex").await;
    let acme_node = link_node(&server, &acme, "Acme recorder").await;
    let globex_node = link_node(&server, &globex, "Globex recorder").await;

    // Globex's node borrows Acme's node id under its own tenant: no such node.
    let mut borrowed = TestNode::new(&globex);
    borrowed.key = globex_node.key.clone();
    borrowed.node_id = acme_node.node_id.clone();
    let (status, body) = server
        .post(
            "/v1/node-auth/challenges",
            &borrowed.challenge_body(CHECK_IN_OPERATION),
        )
        .await;
    assert_eq!(status, 404, "{body}");

    // Globex's node claims to be Acme's node in Acme's tenant: the signature
    // does not match the key the control plane registered for that node.
    let mut impersonating = TestNode::new(&acme);
    impersonating.key = globex_node.key.clone();
    impersonating.node_id = acme_node.node_id.clone();
    let (status, body) = server
        .post(
            "/v1/node-auth/challenges",
            &impersonating.challenge_body(CHECK_IN_OPERATION),
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // A challenge issued to Acme's node cannot be spent by Globex's node.
    let challenge = acme_node.challenge(&server, CHECK_IN_OPERATION).await;
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/metadata-check-ins", globex_node.node_id),
            &globex_node.proof_body(&challenge),
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // The same holds for every other operation a challenge can be issued for,
    // not just check-in.
    for operation in [INVENTORY_OPERATION, UNLINK_OPERATION] {
        let challenge = acme_node.challenge(&server, operation).await;
        let path = if operation == INVENTORY_OPERATION {
            format!("/v1/nodes/{}/inventory-snapshots", globex_node.node_id)
        } else {
            format!("/v1/nodes/{}/unlink-indications", globex_node.node_id)
        };
        let body = if operation == INVENTORY_OPERATION {
            globex_node.inventory_body(&challenge, json!([]))
        } else {
            globex_node.proof_body(&challenge)
        };
        let (status, answer) = server.post(&path, &body).await;
        assert_eq!(status, 401, "{operation} across tenants: {answer}");
    }

    // Acme's own node is unaffected.
    let (status, body) = acme_node.check_in(&server).await;
    assert_eq!(status, 200, "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn one_tenants_inventory_is_invisible_to_another() {
    let dir = temp_dir("cross-inventory");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let acme = server.create_tenant("Acme").await;
    let globex = server.create_tenant("Globex").await;
    let acme_node = link_node(&server, &acme, "Acme recorder").await;

    let (status, body) = acme_node
        .report_inventory(
            &server,
            json!([{"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true}]),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    // Globex sees no nodes and cannot read Acme's node's cameras by id.
    let (status, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{globex}/nodes"))
        .await;
    assert_eq!(status, 200, "{nodes}");
    assert!(nodes["nodes"].as_array().unwrap().is_empty());

    let (status, body) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{globex}/nodes/{}/cameras",
            acme_node.node_id
        ))
        .await;
    assert_eq!(status, 404, "{body}");

    let (status, body) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{acme}/nodes/{}/cameras",
            acme_node.node_id
        ))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["cameras"][0]["camera_id"], "cam-1");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_admin_surface_requires_the_configured_credential() {
    let dir = temp_dir("admin-auth");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;

    let (status, body) = server
        .post_admin_with(
            "/admin/v1/tenants",
            "not-the-configured-admin-token-000000",
            &json!({"name": "Acme"}),
        )
        .await;
    assert_eq!(status, 401, "{body}");

    let response = reqwest::Client::new()
        .post(server.url("/admin/v1/tenants"))
        .json(&json!({"name": "Acme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401, "no credential at all");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}
