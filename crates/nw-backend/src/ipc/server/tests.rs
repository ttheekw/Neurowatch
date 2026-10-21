use super::*;
use crate::app_state::AppState;
use crate::cloud::cloud_handler::CloudHandler;
use crate::cloud::link::{CloudNode, CloudOptions};
use crate::config::AppConfig;
use crate::pipeline::hwaccel::HwAccelKind;
use crate::stats::test_support::{StaticCameras, StaticGpu, StaticIpc, StaticSystem};
use crate::stats::{IpcMetricsSource, PerformanceAggregator};
use crate::viewer::capabilities::ViewerCapabilities;
use crate::viewer::ViewerManager;
use nw_core::stats::GpuStatus;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;

struct Fixture {
    root: PathBuf,
    state: Arc<RwLock<AppState>>,
    handler: Arc<RpcHandler>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("nw-ipc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let state = Arc::new(RwLock::new(AppState::new(
            AppConfig::default(),
            root.join("config.json"),
            HwAccelKind::Software,
        )));
        let viewer = Arc::new(ViewerManager::new(
            HwAccelKind::Software,
            ViewerCapabilities::unavailable("test"),
        ));
        let stats = Arc::new(PerformanceAggregator::new(
            Arc::new(StaticSystem(Default::default())),
            Arc::new(StaticGpu(GpuStatus::unavailable("test"))),
            Arc::new(StaticIpc(Default::default())),
            Arc::new(StaticCameras(Mutex::new(vec![]))),
        ));
        let node = CloudNode::open_with_options(
            root.join("cloud"),
            CloudOptions {
                endpoint: None,
                ..CloudOptions::default()
            },
        )
        .unwrap();
        let handler = Arc::new(
            RpcHandler::new(Arc::clone(&state), viewer, stats)
                .with_cloud(Arc::new(CloudHandler::new(node))),
        );
        Self {
            root,
            state,
            handler,
        }
    }

    async fn exchange(
        &self,
        input: &[u8],
        transport: IpcTransport,
        token: Option<Arc<str>>,
    ) -> (Vec<Value>, IpcMetricsRecorder, anyhow::Result<()>) {
        let (mut client, server) = tokio::io::duplex(8192);
        let metrics = IpcMetricsRecorder::new();
        let task = tokio::spawn(handle_connection(
            server,
            Arc::clone(&self.handler),
            metrics.handle(),
            transport,
            token,
        ));
        client.write_all(input).await.unwrap();
        client.shutdown().await.unwrap();
        let mut output = String::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_string(&mut output))
            .await
            .expect("IPC connection must finish")
            .unwrap();
        let result = task.await.unwrap();
        let responses = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(metrics.snapshot().bytes_out_total, output.len() as u64);
        (responses, metrics, result)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn connection_frames_multiple_requests_and_records_only_nonempty_lines() {
    let fixture = Fixture::new();
    let first = json!({"jsonrpc":"2.0", "id":7, "method":"system.status"}).to_string();
    let second = json!({"jsonrpc":"2.0", "id":"next", "method":"unknown"}).to_string();
    let input = format!("\n{first}\n\n{second}\n");
    let (replies, metrics, result) = fixture
        .exchange(input.as_bytes(), IpcTransport::Protected, None)
        .await;
    result.unwrap();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 7);
    assert!(replies[0]["result"]["pid"].is_number());
    assert_eq!(replies[1]["id"], "next");
    assert_eq!(replies[1]["error"]["code"], -32601);
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.requests_total, 2);
    assert_eq!(snapshot.bytes_in_total, (first.len() + second.len()) as u64);
    assert_eq!(snapshot.inflight, 0);
    assert_eq!(snapshot.drops_total, 0);
}

#[tokio::test]
async fn malformed_json_closes_the_connection_and_releases_the_request_span() {
    let fixture = Fixture::new();
    let (replies, metrics, result) = fixture
        .exchange(b"{invalid json}\n", IpcTransport::Protected, None)
        .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Malformed JSON-RPC"));
    assert!(replies.is_empty());
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.requests_total, 1);
    assert_eq!(snapshot.drops_total, 1);
    assert_eq!(snapshot.inflight, 0);
}

#[tokio::test]
async fn tcp_cloud_access_requires_the_correct_token_on_each_request() {
    let fixture = Fixture::new();
    let requests = [Value::Null, json!("wrong"), json!("secret"), Value::Null];
    let input: String = requests
        .iter()
        .enumerate()
        .map(|(id, auth)| {
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0", "id":id, "method":"cloud.status", "auth":auth})
            )
        })
        .collect();
    let (replies, _, result) = fixture
        .exchange(input.as_bytes(), IpcTransport::Tcp, Some("secret".into()))
        .await;
    result.unwrap();
    assert_eq!(replies.len(), 4);
    for index in [0, 1, 3] {
        assert_eq!(replies[index]["error"]["code"], -32601);
    }
    assert!(replies[2].get("error").is_none(), "{}", replies[2]);
    assert!(replies[2]["result"].is_object());

    let (replies, _, result) = fixture
        .exchange(
            b"{\"id\":1,\"method\":\"cloud.status\",\"auth\":\"secret\"}\n",
            IpcTransport::Tcp,
            None,
        )
        .await;
    result.unwrap();
    assert_eq!(replies[0]["error"]["code"], -32601);
}

#[tokio::test]
async fn protected_transport_still_enforces_local_account_sign_in() {
    let fixture = Fixture::new();
    fixture.state.write().await.config.accounts.enabled = true;
    let (replies, _, result) = fixture
        .exchange(
            b"{\"id\":1,\"method\":\"camera.list\"}\n{\"id\":2,\"method\":\"system.status\"}\n",
            IpcTransport::Protected,
            None,
        )
        .await;
    result.unwrap();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["error"]["code"], -32001);
    assert!(replies[1]["result"].is_object());
}

#[cfg(unix)]
#[tokio::test]
async fn unix_listener_protects_its_path_serves_requests_and_replaces_stale_sockets() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixStream;

    let fixture = Fixture::new();
    let path = fixture.root.join("private/control.sock");
    for stale in [false, true] {
        if stale {
            // The preceding listener left an owned socket at the same path.
            assert!(path.exists());
        }
        let (shutdown, stopped) = broadcast::channel(1);
        let server = tokio::spawn({
            let path = path.clone();
            let handler = Arc::clone(&fixture.handler);
            async move {
                run_unix(
                    path.to_str().unwrap(),
                    handler,
                    stopped,
                    IpcMetricsRecorder::new(),
                )
                .await
            }
        });
        let mut client = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(client) = UnixStream::connect(&path).await {
                    break client;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("listener must bind");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        client
            .write_all(b"{\"id\":42,\"method\":\"system.status\"}\n")
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(3),
            BufReader::new(client).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], 42);
        assert!(reply["result"].is_object());
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_listener_preserves_regular_files_and_rejects_unsafe_directories() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let fixture = Fixture::new();
    let path = fixture.root.join("existing");
    std::fs::write(&path, b"must stay").unwrap();
    let (_, stopped) = broadcast::channel(1);
    let result = run_unix(
        path.to_str().unwrap(),
        Arc::clone(&fixture.handler),
        stopped,
        IpcMetricsRecorder::new(),
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Refusing to replace"));
    assert_eq!(std::fs::read(&path).unwrap(), b"must stay");

    let link = fixture.root.join("link");
    symlink(&path, &link).unwrap();
    let (_, stopped) = broadcast::channel(1);
    assert!(run_unix(
        link.to_str().unwrap(),
        Arc::clone(&fixture.handler),
        stopped,
        IpcMetricsRecorder::new()
    )
    .await
    .is_err());
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());

    let public = fixture.root.join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
    let directory_link = fixture.root.join("directory-link");
    symlink(&public, &directory_link).unwrap();
    for directory in [&public, &directory_link] {
        let socket = directory.join("control.sock");
        let (_, stopped) = broadcast::channel(1);
        let result = run_unix(
            socket.to_str().unwrap(),
            Arc::clone(&fixture.handler),
            stopped,
            IpcMetricsRecorder::new(),
        )
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("private and owner-controlled"));
        assert!(!socket.exists());
    }
}

#[tokio::test]
async fn occupied_tcp_port_fails_without_publishing_a_token() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap().to_string();
    let (_, stopped) = broadcast::channel(1);
    let error = run_tcp(
        &addr,
        Arc::clone(&fixture.handler),
        stopped,
        IpcMetricsRecorder::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Cannot bind to TCP listener"));
}
