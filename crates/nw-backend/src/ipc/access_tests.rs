// Regression tests through the real IPC accounts gate. State and recordings
// are isolated; no camera connection or export worker is started.
use super::NOT_ALLOWED;

fn security_handler(name: &str) -> (Arc<RwLock<AppState>>, RpcHandler, std::path::PathBuf) {
    use crate::accounts::{AccountsConfig, LocalUser, PasswordHash, Role};
    let mut config = config_with_camera("cam-1", "rtsp://127.0.0.1:1/a");
    let mut other = nw_core::camera::CameraConfig::new("Other", "rtsp://127.0.0.1:1/b");
    other.id = "cam-2".into();
    config.cameras.push(other);
    let password = PasswordHash::new("password-1").unwrap();
    config.accounts = AccountsConfig {
        enabled: true,
        users: [
            ("admin", Role::Admin, vec![]),
            ("viewer", Role::Viewer, vec!["cam-1".into()]),
            ("operator", Role::Operator, vec!["cam-1".into()]),
            ("all", Role::Viewer, vec![]),
        ]
        .into_iter()
        .map(|(username, role, cameras)| LocalUser {
            username: username.into(),
            role,
            cameras,
            password: password.clone(),
        })
        .collect(),
    };
    let path = test_config_path(name);
    let state = Arc::new(RwLock::new(AppState::with_database(
        config,
        path.clone(),
        HwAccelKind::Software,
        crate::db::connection::open_in_memory().unwrap(),
    )));
    let handler = RpcHandler::new(Arc::clone(&state), test_viewer_manager(), test_aggregator());
    (state, handler, path)
}

async fn security_login(handler: &RpcHandler, username: &str) -> String {
    let response = dispatch(
        handler,
        "auth.login",
        json!({"username":username,"password":"password-1"}),
    )
    .await;
    response["result"]["token"].as_str().unwrap().into()
}

async fn security_call(handler: &RpcHandler, token: &str, method: &str, params: Value) -> Value {
    handler
        .dispatch_gated(
            json!({"jsonrpc":"2.0","id":1,"session":token,"method":method,"params":params}),
            IpcTransport::Protected,
        )
        .await
}

#[tokio::test]
async fn security_event_ids_are_authorized_before_reads_and_mutations() {
    use crate::events::NewEvent;
    use nw_core::event::{EventKind, EventSource};
    let (state, handler, path) = security_handler("event-authorization");
    let hub = Arc::clone(&state.read().await.events);
    let own = hub.start(
        NewEvent::new(Some("cam-1".into()), EventKind::Bookmark, EventSource::User).kept(true),
    );
    let other = hub.start(
        NewEvent::new(Some("cam-2".into()), EventKind::Bookmark, EventSource::User).kept(true),
    );
    let global = hub.start(NewEvent::new(
        None,
        EventKind::BackendStarted,
        EventSource::System,
    ));
    let viewer = security_login(&handler, "viewer").await;
    let operator = security_login(&handler, "operator").await;
    for method in ["events.get", "events.thumbnail"] {
        let denied = security_call(
            &handler,
            &viewer,
            method,
            json!({"id":other.id,"camera_id":"cam-1"}),
        )
        .await;
        assert_eq!(denied["error"]["code"], NOT_ALLOWED, "{method}: {denied}");
    }
    assert_eq!(
        security_call(&handler, &viewer, "events.get", json!({"id":own.id})).await["result"]["id"],
        own.id
    );
    assert_eq!(
        security_call(&handler, &viewer, "events.get", json!({"id":global.id})).await["result"]
            ["id"],
        global.id
    );
    let denied = security_call(
        &handler,
        &operator,
        "events.update_bookmark",
        json!({"id":other.id,"kept":false,"note":"changed"}),
    )
    .await;
    assert_eq!(denied["error"]["code"], NOT_ALLOWED);
    let denied = security_call(
        &handler,
        &viewer,
        "events.acknowledge",
        json!({"ids":[own.id,other.id]}),
    )
    .await;
    assert_eq!(denied["error"]["code"], NOT_ALLOWED);
    let untouched = dispatch(&handler, "events.get", json!({"id":other.id})).await;
    assert_eq!(untouched["result"]["kept"], true);
    assert_eq!(untouched["result"]["acknowledged"], false);
    assert_eq!(
        dispatch(&handler, "events.get", json!({"id":own.id})).await["result"]["acknowledged"],
        false,
        "a mixed batch must have no partial side effects"
    );
    let allowed = security_call(
        &handler,
        &operator,
        "events.update_bookmark",
        json!({"id":own.id,"kept":false}),
    )
    .await;
    assert_eq!(allowed["result"]["id"], own.id, "{allowed}");
    assert!(!allowed["result"]["kept"].as_bool().unwrap_or(false));
    let allowed = security_call(
        &handler,
        &viewer,
        "events.acknowledge",
        json!({"ids":[own.id]}),
    )
    .await;
    assert_eq!(allowed["result"]["changed"], 1);
    let all = security_login(&handler, "all").await;
    assert_eq!(
        security_call(&handler, &all, "events.get", json!({"id":other.id})).await["result"]["id"],
        other.id
    );
    let admin = security_login(&handler, "admin").await;
    let allowed = security_call(
        &handler,
        &admin,
        "events.update_bookmark",
        json!({"id":other.id,"kept":false}),
    )
    .await;
    assert_eq!(allowed["result"]["id"], other.id, "{allowed}");
    assert!(!allowed["result"]["kept"].as_bool().unwrap_or(false));
    teardown(&state, &path).await;
}

#[tokio::test]
async fn security_export_jobs_are_filtered_and_authorized_by_their_stored_camera() {
    let (state, handler, path) = security_handler("export-authorization");
    let exports = Arc::clone(&state.read().await.exports);
    let own = crate::export::testing::running_job(&exports, "cam-1");
    let other = crate::export::testing::running_job(&exports, "cam-2");
    let operator = security_login(&handler, "operator").await;
    let listed = security_call(&handler, &operator, "export.list", json!({})).await;
    assert_eq!(listed["result"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"][0]["job_id"], own);
    assert!(!listed.to_string().contains(&other));
    for method in ["export.status", "export.cancel"] {
        let denied = security_call(
            &handler,
            &operator,
            method,
            json!({"job_id":other,"camera_id":"cam-1"}),
        )
        .await;
        assert_eq!(denied["error"]["code"], NOT_ALLOWED, "{method}: {denied}");
    }
    assert!(!crate::export::testing::cancel_requested(&exports, &other));
    assert_eq!(
        security_call(&handler, &operator, "export.status", json!({"job_id":own})).await["result"]
            ["camera_id"],
        "cam-1"
    );
    assert_eq!(
        security_call(&handler, &operator, "export.cancel", json!({"job_id":own})).await["result"]
            ["cancelled"],
        true
    );
    assert!(crate::export::testing::cancel_requested(&exports, &own));
    let admin = security_login(&handler, "admin").await;
    assert_eq!(
        security_call(&handler, &admin, "export.list", json!({})).await["result"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        security_call(&handler, &admin, "export.cancel", json!({"job_id":other})).await["result"]
            ["cancelled"],
        true
    );
    teardown(&state, &path).await;
}
