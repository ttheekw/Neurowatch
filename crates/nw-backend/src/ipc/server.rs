//! Unix socket listener — accepts connections and dispatches JSON-RPC requests.

use anyhow::Context as _;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use super::handler::IpcTransport;
use super::handler::RpcHandler;
use super::token;
use crate::stats::IpcMetricsRecorder;

#[cfg(unix)]
use tokio::net::UnixListener;

#[cfg(unix)]
const FALLBACK_UNIX_RUNTIME_DIR: &str = "/tmp";

pub fn ipc_endpoint() -> String {
    match std::env::var("NW_IPC_ENDPOINT") {
        Ok(endpoint) if endpoint != "unix:/control.sock" => endpoint,
        _ => default_endpoint(),
    }
}

#[cfg(unix)]
fn default_endpoint() -> String {
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| FALLBACK_UNIX_RUNTIME_DIR.to_owned());
    format!("unix:{runtime_dir}/neurowatch/control.sock")
}

#[cfg(not(unix))]
fn default_endpoint() -> String {
    "tcp:127.0.0.1:7676".to_owned()
}

pub async fn run(
    handler: Arc<RpcHandler>,
    shutdown: broadcast::Receiver<()>,
    metrics: IpcMetricsRecorder,
) -> anyhow::Result<()> {
    let endpoint = ipc_endpoint();
    if let Some(path) = endpoint.strip_prefix("unix:") {
        #[cfg(unix)]
        {
            return run_unix(path, handler, shutdown, metrics).await;
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            let _ = metrics;
            anyhow::bail!("Unix sockets are not supported on this platform");
        }
    }

    let addr = endpoint.strip_prefix("tcp:").unwrap_or(&endpoint);
    run_tcp(addr, handler, shutdown, metrics).await
}

#[cfg(unix)]
async fn run_unix(
    socket_path: &str,
    handler: Arc<RpcHandler>,
    mut shutdown: broadcast::Receiver<()>,
    metrics: IpcMetricsRecorder,
) -> anyhow::Result<()> {
    // Only replace a stale socket owned by this effective user. Never unlink an
    // arbitrary filesystem object supplied through NW_IPC_ENDPOINT.
    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
                anyhow::bail!("Refusing to replace an IPC path not owned by this user");
            }
            std::fs::remove_file(socket_path)
                .with_context(|| format!("Cannot remove stale socket: {socket_path}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Cannot inspect IPC socket path"),
    }
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        if !parent.exists() {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(parent)
                .with_context(|| format!("Cannot create socket dir: {}", parent.display()))?;
        }
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::symlink_metadata(parent)
            .with_context(|| format!("Cannot inspect socket dir: {}", parent.display()))?;
        if !metadata.file_type().is_dir()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            anyhow::bail!("IPC Unix socket directory must be private and owner-controlled");
        }
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("Cannot bind to socket: {socket_path}"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Cannot protect socket: {socket_path}"))?;
    tracing::info!(endpoint = %format!("unix:{socket_path}"), "IPC server listening");

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _) = accept.context("Accept failed")?;
                let credentials = stream.peer_cred().context("Cannot authenticate IPC peer")?;
                if credentials.uid() != unsafe { libc::geteuid() } {
                    tracing::warn!(peer_uid = credentials.uid(), "Rejected unauthorized IPC peer");
                    continue;
                }
                let handler = Arc::clone(&handler);
                let conn_metrics = metrics.handle();
                tracing::debug!(transport = "unix", "IPC client connected");
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, handler, conn_metrics, IpcTransport::Protected, None).await {
                        tracing::warn!("IPC connection closed: {e}");
                    } else {
                        tracing::debug!(transport = "unix", "IPC client disconnected");
                    }
                });
            }
            _ = shutdown.recv() => {
                tracing::info!("IPC server shutting down");
                break;
            }
        }
    }
    Ok(())
}

async fn run_tcp(
    addr: &str,
    handler: Arc<RpcHandler>,
    mut shutdown: broadcast::Receiver<()>,
    metrics: IpcMetricsRecorder,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Cannot bind to TCP listener: {addr}"))?;
    tracing::info!(endpoint = %format!("tcp:{addr}"), "IPC server listening");
    // Any local process can reach a TCP port; this user's token marks its own.
    let token: Option<Arc<str>> = match token::publish(addr) {
        Ok((path, token)) => {
            tracing::info!(path = %path.display(), "IPC token written for this user's clients");
            Some(token.into())
        }
        Err(error) => {
            tracing::warn!(%error, "IPC token unavailable; extension methods are not served over TCP");
            None
        }
    };

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, peer) = accept.context("Accept failed")?;
                let handler = Arc::clone(&handler);
                let conn_metrics = metrics.handle();
                let token = token.clone();
                tracing::debug!(transport = "tcp", peer = %peer, "IPC client connected");
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, handler, conn_metrics, IpcTransport::Tcp, token).await {
                        tracing::warn!("IPC connection closed: {e}");
                    } else {
                        tracing::debug!(transport = "tcp", peer = %peer, "IPC client disconnected");
                    }
                });
            }
            _ = shutdown.recv() => {
                tracing::info!("IPC server shutting down");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_connection<S>(
    stream: S,
    handler: Arc<RpcHandler>,
    metrics: crate::stats::IpcMetricsHandle,
    transport: IpcTransport,
    token: Option<Arc<str>>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let span = metrics.begin_request(line.len());
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse incoming JSON-RPC request");
                drop(span);
                return Err(anyhow::anyhow!("Malformed JSON-RPC request: {e}"));
            }
        };
        let transport = match &token {
            Some(token) if token::presented(&request, token) => IpcTransport::Protected,
            _ => transport,
        };
        let response = handler.dispatch_from(&request, transport).await;
        let mut out = serde_json::to_string(&response).context("Failed to serialize response")?;
        out.push('\n');
        writer.write_all(out.as_bytes()).await?;
        span.complete(out.len());
    }
    Ok(())
}
