//! Health, explicit configuration, and state that outlives the process.

mod api_harness;

use api_harness::*;
use serde_json::json;

#[tokio::test]
async fn health_answers_from_the_database() {
    let dir = temp_dir("health");
    let server = TestServer::start(&dir.join("control.db"), Some(ADMIN_TOKEN)).await;

    let (status, body) = server.get("/health").await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["database"], "ok");
    assert!(body["schema_version"].as_i64().unwrap() >= 1);
    // Health is not an information leak. It is unauthenticated, so it reports
    // only that the service and its database answer — no tenant or node counts,
    // no names, no ids.
    for field in [
        "tenants",
        "linked_nodes",
        "nodes",
        "name",
        "node_id",
        "tenant_id",
    ] {
        assert!(
            body.get(field).is_none(),
            "/health must not report {field}: {body}"
        );
    }

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn tenants_nodes_and_inventory_survive_a_restart() {
    let dir = temp_dir("restart");
    let database = dir.join("control.db");

    let server = TestServer::start(&database, Some(ADMIN_TOKEN)).await;
    let tenant = server.create_tenant("Acme").await;
    let node = link_node(&server, &tenant, "Front office").await;
    let (status, _) = node
        .report_inventory(
            &server,
            json!([{"camera_id": "cam-1", "name": "Lobby", "status": "streaming", "recording": true}]),
        )
        .await;
    assert_eq!(status, 200);
    let first_address = server.base.clone();
    server.stop().await;

    // A brand new service, a new port, the same database file.
    let server = TestServer::start(&database, Some(ADMIN_TOKEN)).await;
    assert_ne!(server.base, first_address, "a genuinely new listener");

    let (status, body) = server.get("/health").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "ok");

    let (status, nodes) = server
        .get_admin(&format!("/admin/v1/tenants/{tenant}/nodes"))
        .await;
    assert_eq!(status, 200, "{nodes}");
    let listed = nodes["nodes"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["node_id"], node.node_id);
    assert_eq!(listed[0]["display_label"], "Front office");

    let (status, cameras) = server
        .get_admin(&format!(
            "/admin/v1/tenants/{tenant}/nodes/{}/cameras",
            node.node_id
        ))
        .await;
    assert_eq!(status, 200, "{cameras}");
    assert_eq!(cameras["cameras"][0]["camera_id"], "cam-1");

    // The node's registered identity survived too: it can authenticate against
    // the restarted service with the same key.
    let (status, body) = node.check_in(&server).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["node_id"], node.node_id);

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn without_an_administrator_credential_there_is_no_administrator_surface() {
    let dir = temp_dir("no-admin");
    let server = TestServer::start(&dir.join("control.db"), None).await;

    let (status, body) = server.get("/health").await;
    assert_eq!(status, 200, "the service still runs: {body}");

    for (path, body) in [
        ("/admin/v1/tenants", json!({"name": "Acme"})),
        (
            "/admin/v1/tenants/6ba7b811-9dad-11d1-80b4-00c04fd430c8/node-enrollments",
            json!({"label": "Front office"}),
        ),
    ] {
        let response = reqwest::Client::new()
            .post(server.url(path))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            404,
            "{path} must not exist without a configured credential"
        );
    }

    server.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_service_refuses_to_start_without_explicit_configuration() {
    use nw_api::config::{ADMIN_TOKEN_ENV, BIND_ENV, DATABASE_ENV};
    use nw_api::ConfigError;

    let missing_all = nw_api::ApiConfig::from_vars(|_| None);
    assert_eq!(missing_all.unwrap_err(), ConfigError::Missing(BIND_ENV));

    let missing_database = nw_api::ApiConfig::from_vars(|name| {
        (name == BIND_ENV).then(|| "127.0.0.1:8443".to_owned())
    });
    assert_eq!(
        missing_database.unwrap_err(),
        ConfigError::Missing(DATABASE_ENV)
    );

    // An empty value is not a default.
    let empty = nw_api::ApiConfig::from_vars(|name| match name {
        BIND_ENV => Some("  ".into()),
        DATABASE_ENV => Some("/tmp/nw-api.db".into()),
        _ => None,
    });
    assert_eq!(empty.unwrap_err(), ConfigError::Missing(BIND_ENV));

    // A short admin token is a configuration error, not a warning.
    let weak = nw_api::ApiConfig::from_vars(|name| match name {
        BIND_ENV => Some("127.0.0.1:8443".into()),
        DATABASE_ENV => Some("/tmp/nw-api.db".into()),
        ADMIN_TOKEN_ENV => Some("admin".into()),
        _ => None,
    });
    assert!(matches!(
        weak.unwrap_err(),
        ConfigError::Invalid {
            name: ADMIN_TOKEN_ENV,
            ..
        }
    ));

    let good = nw_api::ApiConfig::from_vars(|name| match name {
        BIND_ENV => Some("127.0.0.1:8443".into()),
        DATABASE_ENV => Some("/tmp/nw-api.db".into()),
        _ => None,
    })
    .expect("explicit configuration");
    assert_eq!(good.bind.port(), 8443);
    assert!(good.admin_token.is_none());
}
