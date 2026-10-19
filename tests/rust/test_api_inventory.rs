//! Camera inventory: an allowlist of metadata, and nothing else.

mod api_harness;

use api_harness::*;
use serde_json::{json, Value};

async fn report(server: &TestServer, node: &TestNode, cameras: Value) -> (u16, Value) {
    node.report_inventory(server, cameras).await
}

#[tokio::test]
async fn an_accepted_snapshot_stores_only_allowlisted_metadata() {
    let dir = temp_dir("inventory-ok");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    let (status, body) = report(
        &server,
        &node,
        json!([
            {"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true},
            {"camera_id": "cam-2", "name": "Car park", "status": "offline", "recording": false}
        ]),
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["node_id"], node.node_id);
    assert_eq!(body["accepted"], true);
    assert_eq!(body["camera_count"], 2);

    let (status, listing) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{tenant}/nodes/{}/cameras",
            node.node_id
        ))
        .await;
    assert_eq!(status, 200, "{listing}");
    let cameras = listing["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 2);
    let mut keys: Vec<&String> = cameras[0].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, vec!["camera_id", "name", "recording", "status"]);
    assert_eq!(cameras[0]["camera_id"], "cam-1");
    assert_eq!(cameras[0]["name"], "Lobby");
    assert_eq!(cameras[0]["status"], "streaming");
    assert_eq!(cameras[0]["recording"], true);

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_snapshot_carrying_a_stream_url_or_credential_is_refused_whole() {
    let dir = temp_dir("inventory-rtsp");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    for extra in [
        json!({"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true,
               "rtsp_url": "rtsp://operator:hunter2@192.168.1.64:554/Streaming/Channels/101"}),
        json!({"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true,
               "password": "hunter2"}),
        json!({"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true,
               "segment_path": "/var/lib/neurowatch/segments/cam-1"}),
    ] {
        let (status, body) = report(&server, &node, json!([extra])).await;
        assert_eq!(
            status, 400,
            "an unknown camera field must be refused: {body}"
        );
    }

    // A whole snapshot is refused, so a good camera alongside a bad one is not
    // quietly stored.
    let (status, body) = report(
        &server,
        &node,
        json!([
            {"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true},
            {"camera_id": "cam-2", "name": "Gate", "status": "streaming", "recording": true,
             "onvif_url": "http://192.168.1.65/onvif/device_service"}
        ]),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    let (_, listing) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{tenant}/nodes/{}/cameras",
            node.node_id
        ))
        .await;
    assert!(listing["cameras"].as_array().unwrap().is_empty());

    let bytes = server.database_bytes();
    for forbidden in ["rtsp://", "hunter2", "/var/lib/neurowatch", "onvif"] {
        assert!(
            !bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden.as_bytes()),
            "{forbidden} must never reach the database"
        );
    }

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn camera_values_outside_the_allowlist_are_refused() {
    let dir = temp_dir("inventory-values");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    let cases = [
        // A status the node's own `cloud_status` never produces.
        json!([{"camera_id": "cam-1", "name": "Lobby", "status": "rtsp://cam", "recording": true}]),
        // A display name carrying an address or a credential.
        json!([{"camera_id": "cam-1", "name": "Lobby 192.168.1.64", "status": "streaming", "recording": true}]),
        json!([{"camera_id": "cam-1", "name": "operator@lobby", "status": "streaming", "recording": true}]),
        json!([{"camera_id": "cam-1", "name": "rtsp://lobby/stream", "status": "streaming", "recording": true}]),
        // A camera id that is a path rather than an opaque identifier.
        json!([{"camera_id": "/var/lib/neurowatch/cam-1", "name": "Lobby", "status": "streaming", "recording": true}]),
        // A name long enough to be a payload.
        json!([{"camera_id": "cam-1", "name": "x".repeat(200), "status": "streaming", "recording": true}]),
    ];
    for cameras in cases {
        let (status, body) = report(&server, &node, cameras.clone()).await;
        assert_eq!(status, 400, "{cameras} was accepted: {body}");
    }

    let too_many: Vec<Value> = (0..257)
        .map(|index| {
            json!({"camera_id": format!("cam-{index}"), "name": format!("Camera {index}"),
                   "status": "stopped", "recording": false})
        })
        .collect();
    let (status, body) = report(&server, &node, json!(too_many)).await;
    assert_eq!(status, 400, "{body}");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_inventory_snapshot_needs_its_own_authenticated_challenge() {
    let dir = temp_dir("inventory-auth");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;
    let cameras =
        json!([{"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true}]);

    // A check-in challenge is not an inventory challenge.
    let wrong = node.challenge(&server, CHECK_IN_OPERATION).await;
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/inventory-snapshots", node.node_id),
            &node.inventory_body(&wrong, cameras.clone()),
        )
        .await;
    assert_eq!(status, 401, "{body}");

    // A snapshot whose camera list was edited after signing is not the
    // snapshot the node signed.
    let challenge = node.challenge(&server, INVENTORY_OPERATION).await;
    let mut tampered = node.inventory_body(&challenge, cameras.clone());
    tampered["cameras"] = json!([{"camera_id": "cam-9", "name": "Injected", "status": "streaming", "recording": true}]);
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/inventory-snapshots", node.node_id),
            &tampered,
        )
        .await;
    assert_eq!(status, 401, "{body}");

    let (_, listing) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{tenant}/nodes/{}/cameras",
            node.node_id
        ))
        .await;
    assert!(listing["cameras"].as_array().unwrap().is_empty());

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_older_snapshot_does_not_overwrite_a_newer_one() {
    let dir = temp_dir("inventory-order");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    let challenge = node.challenge(&server, INVENTORY_OPERATION).await;
    let mut newer = node.inventory_body(
        &challenge,
        json!([{"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true}]),
    );
    newer["issued_at"] = json!(rfc3339(0));
    newer["expires_at"] = json!(rfc3339(60));
    let newer = signed(&node.key, newer);
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/inventory-snapshots", node.node_id),
            &newer,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], true);

    let challenge = node.challenge(&server, INVENTORY_OPERATION).await;
    let mut older = node.inventory_body(
        &challenge,
        json!([{"camera_id": "cam-2", "name": "Stale", "status": "stopped", "recording": false}]),
    );
    older["issued_at"] = json!(rfc3339(-30));
    older["expires_at"] = json!(rfc3339(30));
    let older = signed(&node.key, older);
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/inventory-snapshots", node.node_id),
            &older,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], false, "the newer snapshot is kept");

    let (_, listing) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{tenant}/nodes/{}/cameras",
            node.node_id
        ))
        .await;
    let cameras = listing["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 1);
    assert_eq!(cameras[0]["camera_id"], "cam-1");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// The node always writes whole-second `Z` timestamps, but `+00:00` is the same
/// instant and the protocol accepts it. Snapshot ordering compares stored
/// timestamps, so the stored form has to be canonical — otherwise two spellings
/// of one instant order against each other by their punctuation.
#[tokio::test]
async fn an_equivalent_utc_spelling_is_the_same_instant_not_an_older_one() {
    let dir = temp_dir("inventory-utc-spelling");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;

    let issued = rfc3339(0);
    let challenge = node.challenge(&server, INVENTORY_OPERATION).await;
    let mut first = node.inventory_body(
        &challenge,
        json!([{"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true}]),
    );
    first["issued_at"] = json!(issued);
    let first = signed(&node.key, first);
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/inventory-snapshots", node.node_id),
            &first,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], true);

    // The same instant, spelled with an explicit zero offset instead of `Z`.
    let equivalent = issued.replace('Z', "+00:00");
    assert_ne!(equivalent, issued, "the two spellings differ as text");
    let challenge = node.challenge(&server, INVENTORY_OPERATION).await;
    let mut second = node.inventory_body(
        &challenge,
        json!([{"camera_id": "cam-2", "name": "Gate", "status": "streaming", "recording": false}]),
    );
    second["issued_at"] = json!(equivalent);
    let second = signed(&node.key, second);
    let (status, body) = server
        .post(
            &format!("/v1/nodes/{}/inventory-snapshots", node.node_id),
            &second,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["accepted"], true,
        "the same instant is not an older snapshot: {body}"
    );

    let (_, listing) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{tenant}/nodes/{}/cameras",
            node.node_id
        ))
        .await;
    let cameras = listing["cameras"].as_array().unwrap();
    assert_eq!(cameras.len(), 1);
    assert_eq!(cameras[0]["camera_id"], "cam-2");

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}
