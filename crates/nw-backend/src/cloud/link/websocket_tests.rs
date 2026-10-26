use super::*;

#[test]
fn websocket_url_preserves_cloud_path_and_uses_tls() {
    let endpoint = CloudEndpoint::parse("https://cloud.example.test/portal/").unwrap();
    let url = websocket::socket_url(&endpoint, "node-1").unwrap();
    assert_eq!(
        url.as_str(),
        "wss://cloud.example.test/portal/v1/nodes/node-1/ws"
    );
    let dev = CloudEndpoint::loopback_test("http://127.0.0.1:8000/").unwrap();
    assert_eq!(
        websocket::socket_url(&dev, "node-1").unwrap().scheme(),
        "ws"
    );
}

#[test]
fn reconnect_backoff_is_bounded_and_has_jitter() {
    for attempt in 0..100 {
        let delay = websocket::retry_delay(attempt, 0);
        let other = websocket::retry_delay(attempt, 1000);
        assert!(delay >= Duration::from_millis(800));
        assert!(other <= Duration::from_secs(36));
        assert!(other > delay);
    }
}

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{accept_async, tungstenite::Message};

fn ws_node(endpoint: &str) -> (CloudNode, PathBuf) {
    let dir = std::env::temp_dir().join(format!("nw-ws-{}", uuid::Uuid::new_v4()));
    let mut node = CloudNode::open_with_options(
        dir.clone(),
        CloudOptions {
            endpoint: None,
            policy: EndpointPolicy {
                allow_loopback_http: true,
                pinned: None,
            },
        },
    )
    .unwrap();
    node.install_linked_for_test(
        endpoint,
        "6ba7b811-9dad-11d1-80b4-00c04fd430c8",
        "6ba7b812-9dad-11d1-80b4-00c04fd430c8",
        1,
    )
    .unwrap();
    node.websocket = Some(Arc::new(websocket::WsTransport::default()));
    (node, dir)
}

async fn respond<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    request: &Value,
    status: u16,
    body: Value,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    ws.send(Message::Text(
        json!({"id": request["id"], "status": status, "body": body})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

async fn read_request<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(bytes) => ws.send(Message::Pong(bytes)).await.unwrap(),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

async fn handshake<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, public: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    handshake_with(ws, public, json!({})).await;
}

/// The handshake, with `extra` fields (such as `"events": true`) in the
/// authentication reply.
async fn handshake_with<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    public: &str,
    extra: Value,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = read_request(ws).await;
    assert_eq!(request["method"], "challenge");
    assert_eq!(request["body"]["operation"], "websocket_connect");
    assert!(verify_signed(
        public,
        request["body"]["signature"].as_str().unwrap(),
        &without_signature(&request["body"])
    ));
    respond(
        ws,
        &request,
        201,
        json!({"protocol_version": 1, "audience": NODE_AUDIENCE,
        "operation": "websocket_connect", "challenge": "new-connection-proof",
        "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()}),
    )
    .await;
    let proof = read_request(ws).await;
    assert_eq!(proof["method"], "authenticate");
    assert_eq!(proof["body"]["challenge"], "new-connection-proof");
    assert!(verify_signed(
        public,
        proof["body"]["signature"].as_str().unwrap(),
        &without_signature(&proof["body"])
    ));
    let mut accepted = json!({"protocol_version": 1,
        "node_id": proof["body"]["node_id"], "auth_epoch": 1});
    for (key, value) in extra.as_object().unwrap() {
        accepted[key] = value.clone();
    }
    respond(ws, &proof, 200, accepted).await;
}

#[tokio::test]
async fn shared_socket_multiplexes_requests_and_matches_out_of_order_responses() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let (node, dir) = ws_node(&endpoint);
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        handshake(&mut ws, &public).await;
        let first = read_request(&mut ws).await;
        let second = read_request(&mut ws).await;
        assert_eq!(first["method"], "challenge");
        assert_eq!(second["method"], "challenge");
        for request in [&second, &first] {
            respond(&mut ws, request, 201, json!({"protocol_version": 1, "audience": NODE_AUDIENCE,
                "operation": request["body"]["operation"], "challenge": request["body"]["operation"],
                "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()})).await;
        }
        let report1 = read_request(&mut ws).await;
        let report2 = read_request(&mut ws).await;
        for report in [&report2, &report1] {
            assert!(verify_signed(
                &public,
                report["body"]["signature"].as_str().unwrap(),
                &without_signature(&report["body"])
            ));
            let result = match report["method"].as_str().unwrap() {
                "inventory_snapshot" => {
                    json!({"node_id": report["body"]["node_id"], "accepted": true})
                }
                "recording_coverage" => {
                    json!({"node_id": report["body"]["node_id"], "accepted": false})
                }
                other => panic!("unexpected method {other}"),
            };
            respond(&mut ws, report, 200, result).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        while let Some(Ok(message)) = ws.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });
    let (inventory, coverage) =
        tokio::join!(node.push_inventory(vec![]), node.push_coverage(vec![]));
    assert!(inventory.unwrap());
    assert!(!coverage.unwrap());
    node.close_websocket().await;
    server.await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn portal_event_pull_preserves_flags_over_websocket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        handshake(&mut ws, &public).await;
        for events_only in [true, false] {
            let challenge = read_request(&mut ws).await;
            assert_eq!(challenge["body"]["operation"], CONFIG_PULL_OPERATION);
            respond(&mut ws, &challenge, 201, json!({"protocol_version": 1, "audience": NODE_AUDIENCE,
                "operation": CONFIG_PULL_OPERATION, "challenge": "event-pull-proof",
                "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()})).await;
            let pull = read_request(&mut ws).await;
            assert_eq!(pull["method"], CONFIG_PULL_OPERATION);
            assert_eq!(pull["events"], true);
            assert_eq!(pull["events_only"], events_only);
            assert_eq!(pull["wait"], 0.0);
            respond(&mut ws, &pull, 200, json!({"requests": []})).await;
        }
        while let Some(Ok(message)) = ws.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });
    // Bound the client too: a server assertion must fail, not hang this test.
    for events_only in [true, false] {
        let pulled = tokio::time::timeout(
            Duration::from_secs(5),
            node.pull_portal_requests(Duration::ZERO, events_only),
        )
        .await;
        assert!(pulled.unwrap().unwrap().is_empty());
    }
    node.close_websocket().await;
    server.await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn socket_authentication_refusal_is_not_retried_as_http_or_reported_online() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let request = read_request(&mut ws).await;
        respond(&mut ws, &request, 401, json!({"code": "unauthenticated"})).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    assert!(matches!(
        node.check_in_once().await,
        Err(CloudError::Refused(401))
    ));
    assert_eq!(node.status().phase, LinkPhase::Linked);
    assert!(!node.report().unwrap().cloud_reachable);
    assert_eq!(node.report().unwrap().cloud_problem, Some("link_refused"));
    server.await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_dropped_socket_reconnects_with_fresh_authentication_without_replaying_a_write() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            handshake(&mut ws, &public).await;
            let request = read_request(&mut ws).await;
            assert_eq!(request["method"], "challenge");
            assert_eq!(request["body"]["operation"], INVENTORY_OPERATION);
            respond(&mut ws, &request, 201, json!({"protocol_version": 1, "audience": NODE_AUDIENCE,
                "operation": INVENTORY_OPERATION, "challenge": "inventory-once",
                "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()})).await;
            let inventory = read_request(&mut ws).await;
            assert_eq!(inventory["method"], "inventory_snapshot");
            ws.close(None).await.unwrap();
        }
    });
    assert!(node.push_inventory(vec![]).await.is_err());
    assert_eq!(node.status().phase, LinkPhase::Linked);
    assert!(!node.report().unwrap().cloud_reachable);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(node.push_inventory(vec![]).await.is_err());
    server.await.unwrap();
    node.close_websocket().await;
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn heartbeat_detects_a_half_open_connection_and_clears_reachability() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let (heartbeat_tx, heartbeat_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        handshake(&mut ws, &public).await;
        let demand = read_request(&mut ws).await;
        assert_eq!(demand["body"]["operation"], THUMBNAIL_DEMAND_OPERATION);
        respond(
            &mut ws,
            &demand,
            201,
            json!({"protocol_version":1,"audience":NODE_AUDIENCE,
            "operation":THUMBNAIL_DEMAND_OPERATION,"challenge":"demand-once",
            "expires_at":fmt_time(OffsetDateTime::now_utc()+TimeDuration::seconds(120)).unwrap()}),
        )
        .await;
        let demand = read_request(&mut ws).await;
        assert_eq!(demand["method"], THUMBNAIL_DEMAND_OPERATION);
        respond(&mut ws, &demand, 200, json!({"camera_ids":[]})).await;
        let heartbeat = read_request(&mut ws).await;
        assert_eq!(heartbeat["method"], "heartbeat");
        heartbeat_tx.send(()).unwrap();
        // Deliberately do not answer; TCP stays established until the client closes.
        while let Some(Ok(message)) = ws.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });
    assert_eq!(
        node.pull_thumbnail_demands(Duration::ZERO, &[])
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    assert!(node.report().unwrap().cloud_reachable);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(21)).await;
    heartbeat_rx.await.unwrap();
    tokio::time::advance(Duration::from_secs(11)).await;
    server.await.unwrap();
    assert!(!node.report().unwrap().cloud_reachable);
    assert_eq!(node.status().phase, LinkPhase::Linked);
    node.close_websocket().await;
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
#[ignore = "requires the fake-storage portal in docs/integrations/node-websocket.md"]
async fn websocket_real_portal_contract() {
    let endpoint = std::env::var("NW_WS_TEST_ENDPOINT").expect("test portal URL required");
    let endpoint = endpoint.trim_end_matches('/');
    let (node, dir) = events_node(endpoint);
    // Public deterministic test identity; never used by a deployed node.
    node.lock().unwrap().key = SigningKey::from_bytes(&[7u8; 32]);
    node.check_in_once().await.unwrap();
    let camera = CameraSnapshot {
        camera_id: "cam-1".into(),
        name: "Café entrance".into(),
        status: "streaming",
        recording: true,
        // The portal must verify the recorder's signature over its streams.
        live: Some(std::collections::BTreeMap::from([
            ("main", Some("h264")),
            ("sub", None),
        ])),
    };
    assert!(node.push_inventory(vec![camera]).await.unwrap());
    assert!(node.push_coverage(vec![]).await.unwrap());
    assert!(node
        .pull_publish_directives()
        .await
        .unwrap()
        .active_session_ids
        .is_empty());
    assert!(!node.push_diagnostics(None, Duration::ZERO).await.unwrap());
    assert_eq!(
        node.pull_thumbnail_demands(Duration::ZERO, &[])
            .await
            .unwrap(),
        vec!["cam-1"]
    );
    assert_eq!(
        node.push_thumbnail_batch(vec![ThumbnailImage {
            camera_id: "cam-1".into(),
            jpeg_b64: base64::engine::general_purpose::STANDARD.encode([255, 216, 255, 217]),
        }])
        .await
        .unwrap(),
        1
    );
    let requests = node.pull_config_requests(Duration::ZERO).await.unwrap();
    assert_eq!(requests.len(), 1);
    node.push_config_result(&requests[0].request_id, &Ok(json!({"enabled":true})))
        .await
        .unwrap();
    // Cross the actor's link-check tick with the pre-WebSocket saved URL form.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(node.report().unwrap().cloud_reachable);
    assert!(node.push_coverage(vec![]).await.unwrap());
    // The portal's events reach the node over the same socket.
    assert!(node.events_pushed());
    let _ = woken(&node.signals.diagnostics, Duration::ZERO).await;
    assert!(woken(&node.signals.diagnostics, Duration::from_secs(3)).await);
    node.announce_departure().await.unwrap();
    node.close_websocket().await;
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn cancelling_an_authentication_attempt_keeps_reconnect_backoff() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        assert_eq!(read_request(&mut ws).await["method"], "challenge");
        seen_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "cancellation must not open another TCP connection immediately"
        );
    });
    let checking = node.shared();
    let attempt = tokio::spawn(async move { checking.check_in_once().await });
    seen_rx.await.unwrap();
    attempt.abort();
    let _ = attempt.await;
    let retry = tokio::time::timeout(Duration::from_millis(100), node.check_in_once()).await;
    assert!(matches!(retry, Ok(Err(CloudError::Network(message))) if message.contains("backoff")));
    server.await.unwrap();
    node.close_websocket().await;
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn secure_websocket_rejects_an_untrusted_certificate_without_panicking() {
    let cert = rcgen::generate_simple_self_signed(vec!["cloud.example.test".into()]).unwrap();
    // Public fake identity, generated only for this test; no deployment keys.
    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(
                cert.key_pair.serialize_der().into(),
            ),
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = tokio_rustls::TlsAcceptor::from(Arc::new(config))
            .accept(stream)
            .await;
    });
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let connection = tokio_tungstenite::client_async_tls_with_config(
        "wss://cloud.example.test/v1/nodes/node-1/ws",
        stream,
        None,
        Some(websocket::tls_connector().unwrap()),
    )
    .await;
    assert!(
        connection.is_err(),
        "an untrusted TLS certificate must never authenticate a portal"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn pending_unlink_for_an_older_identity_preserves_the_active_socket_and_status() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let (node, dir) = ws_node(&endpoint);
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    node.lock().unwrap().persisted.pending_unlink = Some(PendingUnlink {
        endpoint,
        tenant_id: "6ba7b811-9dad-11d1-80b4-00c04fd430c8".into(),
        node_id: "6ba7b813-9dad-11d1-80b4-00c04fd430c8".into(),
        auth_epoch: 1,
    });
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut active = accept_async(stream).await.unwrap();
        handshake(&mut active, &public).await;
        for cycle in 0..2 {
            let request = read_request(&mut active).await;
            assert_eq!(request["body"]["operation"], INVENTORY_OPERATION);
            respond(&mut active, &request, 201, json!({"protocol_version":1,"audience":NODE_AUDIENCE,
                "operation":INVENTORY_OPERATION,"challenge":"inventory-once",
                "expires_at":fmt_time(OffsetDateTime::now_utc()+TimeDuration::seconds(120)).unwrap()})).await;
            let request = read_request(&mut active).await;
            assert_eq!(request["method"], INVENTORY_OPERATION);
            respond(
                &mut active,
                &request,
                200,
                json!({"node_id":request["body"]["node_id"],"accepted":true}),
            )
            .await;
            if cycle == 0 {
                let (old_stream, _) = listener.accept().await.unwrap();
                let mut old = accept_async(old_stream).await.unwrap();
                let request = read_request(&mut old).await;
                assert_eq!(
                    request["body"]["node_id"],
                    "6ba7b813-9dad-11d1-80b4-00c04fd430c8"
                );
                respond(&mut old, &request, 401, json!({"code":"unauthenticated"})).await;
            }
        }
        while let Some(Ok(message)) = active.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });
    assert!(node.push_inventory(vec![]).await.unwrap());
    assert!(matches!(
        node.retry_pending_unlink().await,
        Err(CloudError::Refused(401))
    ));
    assert!(
        node.report().unwrap().cloud_reachable,
        "a retired identity must not change current connectivity"
    );
    assert_eq!(node.report().unwrap().cloud_problem, None);
    assert!(node.push_inventory(vec![]).await.unwrap());
    node.close_websocket().await;
    server.await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn closing_control_connectivity_has_a_budget_even_during_a_stalled_handshake() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        assert_eq!(read_request(&mut ws).await["method"], "challenge");
        seen_tx.send(()).unwrap();
        stop_rx.await.unwrap();
    });
    let checking = node.shared();
    let attempt = tokio::spawn(async move { checking.check_in_once().await });
    seen_rx.await.unwrap();
    tokio::time::pause();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), node.close_websocket())
            .await
            .is_ok(),
        "cloud cleanup must leave time for local recording finalization"
    );
    attempt.abort();
    let _ = attempt.await;
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[derive(Clone)]
struct CapturedWsLogs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedWsLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedWsLogs {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_ws_logs() -> (CapturedWsLogs, tracing::subscriber::DefaultGuard) {
    let output = CapturedWsLogs(Arc::new(Mutex::new(Vec::new())));
    // What the backend uses when `RUST_LOG` is unset.
    let filter = crate::logging::default_log_filter();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_env_filter(filter)
        .with_writer(output.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (output, guard)
}

#[tokio::test]
async fn default_logging_shows_real_websocket_connection_and_message_events() {
    let (output, _guard) = capture_ws_logs();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        handshake(&mut ws, &public).await;
        let request = read_request(&mut ws).await;
        assert_eq!(request["body"]["operation"], "recording_coverage");
        respond(&mut ws, &request, 201, json!({"protocol_version": 1,
            "audience": NODE_AUDIENCE, "operation": "recording_coverage",
            "challenge": "secret-test-operation-proof",
            "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()})).await;
        let request = read_request(&mut ws).await;
        assert_eq!(request["method"], "recording_coverage");
        respond(
            &mut ws,
            &request,
            200,
            json!({"node_id": request["body"]["node_id"], "accepted": true}),
        )
        .await;
        while let Some(Ok(message)) = ws.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });
    assert!(node.push_coverage(vec![]).await.unwrap());
    node.close_websocket().await;
    server.await.unwrap();
    let logs = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("[ws.connection] connecting"), "{logs}");
    assert!(
        logs.contains("[ws.connection] connected and authenticated"),
        "{logs}"
    );
    assert!(logs.contains("[ws.connection] disconnected"), "{logs}");
    assert!(logs.contains("[ws.message] sent"), "{logs}");
    assert!(logs.contains("[ws.message] received"), "{logs}");
    assert!(logs.contains("recording_coverage"), "{logs}");
    assert!(!logs.contains("secret-test-operation-proof"), "{logs}");
    assert!(!logs.contains("new-connection-proof"), "{logs}");
    assert!(!logs.contains("signature"), "{logs}");
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn default_logging_explains_unreachable_websocket_and_scheduled_retry() {
    let (output, _guard) = capture_ws_logs();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = ws_node(&format!("http://{}/", listener.local_addr().unwrap()));
    drop(listener);
    assert!(node.check_in_once().await.is_err());
    assert!(!node.report().unwrap().cloud_reachable);
    assert_eq!(node.status().phase, LinkPhase::Linked);
    let logs = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("[ws.connection] connecting"), "{logs}");
    assert!(logs.contains("tcp_connection_failed"), "{logs}");
    assert!(logs.contains("reconnect scheduled"), "{logs}");
    assert!(logs.contains("retry_ms="), "{logs}");
    fs::remove_dir_all(dir).unwrap();
}

// -- portal events ---------------------------------------------------------------

fn events_node(endpoint: &str) -> (CloudNode, PathBuf) {
    let (mut node, dir) = ws_node(endpoint);
    node.websocket = Some(Arc::new(websocket::WsTransport::with_events()));
    (node, dir)
}

async fn send_event<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, topic: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    ws.send(Message::Text(json!({ "event": topic }).to_string().into()))
        .await
        .unwrap();
}

async fn woken(notify: &tokio::sync::Notify, within: Duration) -> bool {
    tokio::time::timeout(within, notify.notified())
        .await
        .is_ok()
}

#[tokio::test]
// tungstenite's handshake callback fixes the error type (an HTTP response).
#[allow(clippy::result_large_err)]
async fn the_node_asks_for_events_and_an_event_wakes_only_its_loop() {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = events_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut query = None;
        let mut ws =
            tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
                query = request.uri().query().map(str::to_owned);
                Ok(response)
            })
            .await
            .unwrap();
        assert_eq!(query.as_deref(), Some("events=1"));
        handshake_with(&mut ws, &public, json!({"events": true})).await;
        let request = read_request(&mut ws).await;
        respond(&mut ws, &request, 503, json!({"code": "unavailable"})).await;
        // Events arrive ahead of the reply to the node's next request.
        let request = read_request(&mut ws).await;
        send_event(&mut ws, "camera.update").await;
        send_event(&mut ws, CONFIG_PULL_OPERATION).await;
        respond(&mut ws, &request, 503, json!({"code": "unavailable"})).await;
        let request = read_request(&mut ws).await;
        respond(&mut ws, &request, 503, json!({"code": "unavailable"})).await;
        let _ = read_request(&mut ws).await;
        ws.close(None).await.unwrap();
    });
    assert!(!node.events_pushed());
    assert!(node.push_inventory(vec![]).await.is_err());
    assert!(node.events_pushed());
    // Connecting asks every loop once, for anything missed while disconnected.
    for notify in [
        &node.signals.directives,
        &node.signals.thumbnails,
        &node.signals.config,
        &node.signals.diagnostics,
    ] {
        assert!(woken(notify, Duration::from_millis(200)).await);
    }

    assert!(node.push_inventory(vec![]).await.is_err());
    assert!(woken(&node.signals.config, Duration::from_secs(1)).await);
    assert!(!woken(&node.signals.directives, Duration::from_millis(100)).await);
    // An unknown topic is skipped, not a protocol error: the socket stays up.
    assert!(node.push_inventory(vec![]).await.is_err());
    assert!(node.events_pushed());

    assert!(node.push_inventory(vec![]).await.is_err());
    server.await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while node.events_pushed() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!node.events_pushed(), "a closed socket delivers no events");
    node.close_websocket().await;
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_portal_without_events_leaves_the_node_on_its_timers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = events_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        handshake(&mut ws, &public).await;
        let request = read_request(&mut ws).await;
        respond(&mut ws, &request, 503, json!({"code": "unavailable"})).await;
        let _ = read_request(&mut ws).await;
    });
    assert!(node.push_inventory(vec![]).await.is_err());
    assert!(!node.events_pushed());
    assert!(!woken(&node.signals.directives, Duration::from_millis(100)).await);
    node.close_websocket().await;
    server.abort();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_event_inside_the_handshake_does_not_break_authentication() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = events_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let request = read_request(&mut ws).await;
        send_event(&mut ws, PUBLISH_DIRECTIVES_OPERATION).await;
        respond(&mut ws, &request, 201, json!({"protocol_version": 1, "audience": NODE_AUDIENCE,
            "operation": "websocket_connect", "challenge": "c",
            "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()})).await;
        let proof = read_request(&mut ws).await;
        send_event(&mut ws, PUBLISH_DIRECTIVES_OPERATION).await;
        respond(
            &mut ws,
            &proof,
            200,
            json!({"protocol_version": 1,
            "node_id": proof["body"]["node_id"], "auth_epoch": 1, "events": true}),
        )
        .await;
        let request = read_request(&mut ws).await;
        respond(&mut ws, &request, 503, json!({"code": "unavailable"})).await;
        let _ = read_request(&mut ws).await;
    });
    assert!(matches!(
        node.push_inventory(vec![]).await,
        Err(CloudError::Network(_))
    ));
    assert!(node.events_pushed());
    node.close_websocket().await;
    server.abort();
    fs::remove_dir_all(dir).unwrap();
}

/// Answers a node's requests like a portal with no work for it, reporting
/// each directive pull, and sends the events it is given.
async fn idle_portal(
    listener: tokio::net::TcpListener,
    public: String,
    pulls: tokio::sync::mpsc::UnboundedSender<tokio::time::Instant>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<&'static str>,
) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = accept_async(stream).await.unwrap();
    handshake_with(&mut ws, &public, json!({"events": true})).await;
    loop {
        tokio::select! {
            frame = ws.next() => {
                let request: Value = match frame {
                    Some(Ok(Message::Text(text))) => serde_json::from_str(text.as_str()).unwrap(),
                    Some(Ok(Message::Ping(bytes))) => { ws.send(Message::Pong(bytes)).await.unwrap(); continue; }
                    _ => break,
                };
                match request["method"].as_str().unwrap() {
                    "challenge" => respond(&mut ws, &request, 201, json!({"protocol_version": 1,
                        "audience": NODE_AUDIENCE, "operation": request["body"]["operation"],
                        "challenge": uuid::Uuid::new_v4().simple().to_string(),
                        "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()})).await,
                    "publish_directives" => {
                        let _ = pulls.send(tokio::time::Instant::now());
                        respond(&mut ws, &request, 200, json!({"protocol_version": 2,
                            "directives": [], "active_session_ids": []})).await;
                    }
                    "heartbeat" => respond(&mut ws, &request, 200, json!({"alive": true})).await,
                    _ => respond(&mut ws, &request, 503, json!({"code": "unavailable"})).await,
                }
            }
            event = events.recv() => match event {
                Some(topic) => send_event(&mut ws, topic).await,
                None => break,
            },
        }
    }
}

#[tokio::test]
async fn an_event_pulls_directives_at_once_and_the_timer_becomes_a_fallback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = events_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let (pulls_tx, mut pulls) = tokio::sync::mpsc::unbounded_channel();
    let (events, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(idle_portal(listener, public, pulls_tx, events_rx));
    let sink: DirectiveSink = Arc::new(|_| Box::pin(async {}));
    let scheduler = CloudScheduler::new(node.shared(), Duration::from_secs(60), Arc::default())
        .with_directives(sink)
        .spawn();
    // The first pull connects. Connecting also asks the loop once more for
    // anything missed, which may or may not fold into that first pull; let
    // either settle, past the minimum gap after the last pull.
    tokio::time::timeout(Duration::from_secs(5), pulls.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(DIRECTIVE_MIN_GAP * 2 + Duration::from_millis(500)).await;
    while pulls.try_recv().is_ok() {}
    assert!(node.events_pushed());

    let sent = tokio::time::Instant::now();
    events.send(PUBLISH_DIRECTIVES_OPERATION).unwrap();
    let woken = tokio::time::timeout(Duration::from_secs(3), pulls.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        woken - sent < Duration::from_millis(500),
        "pulled {:?} after the event",
        woken - sent
    );

    // Without events the node pulls every 5 s; now its timer only backs them up.
    assert!(tokio::time::timeout(Duration::from_secs(7), pulls.recv())
        .await
        .is_err());

    scheduler.shutdown().await;
    node.close_websocket().await;
    server.abort();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_dropped_control_connection_keeps_active_publications_within_the_grace() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (node, dir) = events_node(&format!("http://{}/", listener.local_addr().unwrap()));
    let public = URL_SAFE_NO_PAD.encode(node.lock().unwrap().key.verifying_key().to_bytes());
    let session = uuid::Uuid::new_v4().simple().to_string();
    // A portal that answers the first pull with one live session, then the
    // connection is gone (and stays gone: the listener accepts only once).
    let server = {
        let session = session.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            handshake_with(&mut ws, &public, json!({"events": true})).await;
            loop {
                let request: Value = match ws.next().await {
                    Some(Ok(Message::Text(text))) => serde_json::from_str(text.as_str()).unwrap(),
                    Some(Ok(Message::Ping(bytes))) => {
                        ws.send(Message::Pong(bytes)).await.unwrap();
                        continue;
                    }
                    _ => break,
                };
                match request["method"].as_str().unwrap() {
                    "challenge" => respond(&mut ws, &request, 201, json!({"protocol_version": 1,
                        "audience": NODE_AUDIENCE, "operation": request["body"]["operation"],
                        "challenge": uuid::Uuid::new_v4().simple().to_string(),
                        "expires_at": fmt_time(OffsetDateTime::now_utc() + TimeDuration::seconds(120)).unwrap()})).await,
                    "publish_directives" => {
                        respond(&mut ws, &request, 200, json!({"protocol_version": 2,
                            "directives": [], "active_session_ids": [session]})).await;
                        break;
                    }
                    _ => respond(&mut ws, &request, 503, json!({"code": "unavailable"})).await,
                }
            }
        })
    };
    let (batches_tx, mut batches) = tokio::sync::mpsc::unbounded_channel();
    let sink: DirectiveSink = Arc::new(move |batch| {
        let _ = batches_tx.send(batch.active_session_ids);
        Box::pin(async {})
    });
    let scheduler = CloudScheduler::new(node.shared(), Duration::from_secs(60), Arc::default())
        .with_directives(sink)
        .spawn();
    let first = tokio::time::timeout(Duration::from_secs(5), batches.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, vec![session]);

    // Later pulls fail while the connection is down (one every 5 s without
    // events): the session must not be handed an empty batch, which stops it.
    let within = tokio::time::timeout(Duration::from_secs(12), batches.recv()).await;
    assert!(
        within.is_err(),
        "publications were stopped by a failed pull within the grace: {within:?}"
    );

    scheduler.shutdown().await;
    node.close_websocket().await;
    server.abort();
    fs::remove_dir_all(dir).unwrap();
}
