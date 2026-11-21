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

#[tokio::test]
async fn security_recording_and_wall_methods_respect_the_camera_limit() {
    // Vulnerability checklist R4: these name their camera as `id` (or in a
    // list), which the camera limit did not read.
    let (state, handler, path) = security_handler("recording-camera-limit");
    let operator = security_login(&handler, "operator").await;
    let viewer = security_login(&handler, "viewer").await;
    for (token, method, params) in [
        (&operator, "recording.stop", json!({"id": "cam-2"})),
        (&operator, "recording.start", json!({"id": "cam-2"})),
        (&viewer, "recording.list_segments", json!({"id": "cam-2"})),
        (
            &operator,
            "camera.set_positions",
            json!([{"id": "cam-1", "wall_position": {"page": 0, "row": 0, "col": 0}},
                   {"id": "cam-2", "wall_position": {"page": 0, "row": 0, "col": 1}}]),
        ),
    ] {
        let denied = security_call(&handler, token, method, params).await;
        assert_eq!(denied["error"]["code"], NOT_ALLOWED, "{method}: {denied}");
    }
    assert!(
        state.read().await.config.cameras[1].wall_position.is_none(),
        "the other camera was not moved"
    );
    let own = security_call(&handler, &viewer, "recording.list_segments", json!({"id": "cam-1"})).await;
    assert!(own.get("error").is_none(), "{own}");
    let placed = security_call(
        &handler,
        &operator,
        "camera.set_positions",
        json!([{"id": "cam-1", "wall_position": {"page": 0, "row": 0, "col": 0}}]),
    )
    .await;
    assert!(placed.get("error").is_none(), "{placed}");
    teardown(&state, &path).await;
}

#[tokio::test]
async fn security_talk_and_status_reads_respect_the_camera_limit() {
    // Checklist "Recorder local": talk.status/stop skipped the camera limit,
    // and status reads (open without sign-in) were never filtered.
    let (state, handler, path) = security_handler("talk-status-limit");
    let operator = security_login(&handler, "operator").await;
    let talk = Arc::clone(&state.read().await.talk);
    talk.pretend_talking("cam-2", "someone");
    let status = security_call(&handler, &operator, "talk.status", json!({})).await;
    assert!(status["result"].is_null(), "{status}");
    let stop = security_call(&handler, &operator, "talk.stop", json!({"client_id": "someone"})).await;
    assert_eq!(stop["error"]["code"], NOT_ALLOWED, "{stop}");
    assert!(talk.status().is_some(), "the other camera's talk went on");
    talk.pretend_talking("cam-1", "mine");
    let status = security_call(&handler, &operator, "talk.status", json!({})).await;
    assert_eq!(status["result"]["camera_id"], "cam-1");
    let stop = security_call(&handler, &operator, "talk.stop", json!({"client_id": "mine"})).await;
    assert!(stop.get("error").is_none(), "{stop}");

    {
        let mut st = state.write().await;
        st.pretend_camera_state("cam-1", "streaming");
        st.pretend_camera_state("cam-2", "streaming");
    }
    let all = security_call(&handler, &operator, "camera.status_all", json!({})).await;
    let cameras = all["result"].as_object().cloned().unwrap_or_default();
    assert!(cameras.contains_key("cam-1") && !cameras.contains_key("cam-2"), "{all}");
    let anonymous = dispatch(&handler, "camera.status_all", json!({})).await;
    assert!(anonymous["result"].as_object().is_some_and(|m| m.contains_key("cam-2")), "{anonymous}");
    teardown(&state, &path).await;
}

#[tokio::test]
async fn security_an_operator_exports_only_to_the_default_folder() {
    let (state, handler, path) = security_handler("export-path");
    let operator = security_login(&handler, "operator").await;
    let admin = security_login(&handler, "admin").await;
    let outside = std::env::temp_dir().join("nw-operator-chosen.mp4");
    let params = json!({"camera_id": "cam-1", "start_ms": 0, "end_ms": 5,
        "path": outside.to_string_lossy()});
    let denied = security_call(&handler, &operator, "export.start", params.clone()).await;
    assert_eq!(denied["error"]["code"], NOT_ALLOWED, "{denied}");
    // An administrator's own path passes the gate (then finds no recordings).
    let admin_call = security_call(&handler, &admin, "export.start", params).await;
    assert_ne!(admin_call["error"]["code"], NOT_ALLOWED, "{admin_call}");
    teardown(&state, &path).await;
}
