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

/// With extended debug logging on, requests slower than this are reported:
/// the desktop app gives up on a reply after a few seconds.
const SLOW_REQUEST: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod tests;

#[cfg(unix)]
use tokio::net::UnixListener;

#[cfg(unix)]
const FALLBACK_UNIX_TMP_DIR: &str = "/tmp";

pub fn ipc_endpoint() -> String {
    match std::env::var("NW_IPC_ENDPOINT") {
        Ok(endpoint) if endpoint != "unix:/control.sock" => endpoint,
        _ => default_endpoint(),
    }
}

#[cfg(unix)]
fn default_endpoint() -> String {
    let configured = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .or_else(|| login_runtime_dir().map(|dir| dir.to_string_lossy().into_owned()));
    match configured {
        Some(runtime_dir) => format!("unix:{runtime_dir}/neurowatch/control.sock"),
        None => tmp_fallback_endpoint(std::path::Path::new(FALLBACK_UNIX_TMP_DIR), unsafe {
            libc::geteuid()
        }),
    }
}

/// The last resort without a runtime folder. `/tmp` is shared, so a name in
/// it can be taken by another user first. An existing `/tmp/neurowatch` stays
/// in use when it is a real folder of this user that nobody else can write
/// (installs made before this rule); otherwise the endpoint is
/// `/tmp/neurowatch-<uid>/control.sock`, a folder `run_unix` creates 0700 and
/// refuses to use unless it is this user's own private folder.
#[cfg(unix)]
fn tmp_fallback_endpoint(tmp: &std::path::Path, uid: u32) -> String {
    use std::os::unix::fs::MetadataExt;
    let legacy = tmp.join("neurowatch");
    let usable = std::fs::symlink_metadata(&legacy).is_ok_and(|info| {
        info.file_type().is_dir() && info.uid() == uid && info.mode() & 0o022 == 0
    });
    if usable {
        format!("unix:{}/control.sock", legacy.display())
    } else {
        format!("unix:{}/neurowatch-{uid}/control.sock", tmp.display())
    }
}

/// `/run/user/<uid>` when it is this user's own folder, for a session started
/// without `XDG_RUNTIME_DIR` (the same choice the desktop app makes). The
/// shared `/tmp` is the last resort (see `tmp_fallback_endpoint`).
#[cfg(unix)]
fn login_runtime_dir() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let uid = unsafe { libc::geteuid() };
    let path = std::path::PathBuf::from(format!("/run/user/{uid}"));
    let info = std::fs::metadata(&path).ok()?;
    (info.is_dir() && info.uid() == uid).then_some(path)
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
            anyhow::bail!(
                "IPC Unix socket directory must be private and owner-controlled: {} is not a \
                 private folder of this user (another user may have created it); set \
                 XDG_RUNTIME_DIR or NW_IPC_ENDPOINT to a folder of your own",
                parent.display()
            );
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
                // One failed accept (out of file handles, a peer gone before
                // it was taken) is not a reason to stop the recorder: that
                // shut the whole backend down (vulnerability checklist R6).
                let stream = match accept {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "IPC accept failed; going on");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let Ok(credentials) = stream.peer_cred() else {
                    tracing::warn!("Cannot authenticate IPC peer; closed");
                    continue;
                };
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
                let (stream, peer) = match accept {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(%error, "IPC accept failed; going on");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                };
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

/// The longest request line a client may send. Requests carry settings and
/// pictures of figures, never footage; a peer that never ends its line would
/// otherwise grow the buffer until memory runs out.
const MAX_REQUEST_LINE: usize = 8 * 1024 * 1024;

/// One line (without its newline) of at most `limit` bytes; `None` at the end
/// of the stream. A longer line is an error and the caller closes the peer.
async fn read_request_line<R>(reader: &mut R, limit: usize) -> anyhow::Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut line: Vec<u8> = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        let (take, found) = match chunk.iter().position(|b| *b == b'\n') {
            Some(at) => (at, true),
            None => (chunk.len(), false),
        };
        if line.len() + take > limit {
            anyhow::bail!("IPC request line longer than {limit} bytes");
        }
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take + usize::from(found));
        if found {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(
        String::from_utf8(line).context("IPC request is not UTF-8")?,
    ))
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
    let mut reader = BufReader::new(reader);

    while let Some(line) = read_request_line(&mut reader, MAX_REQUEST_LINE).await? {
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
        // Long polls (`timeout_ms`, e.g. `events.wait`) take long on purpose.
        let method = (crate::logging::extended() && request["params"]["timeout_ms"].is_null())
            .then(|| request["method"].as_str().unwrap_or("?").to_owned());
        let started = std::time::Instant::now();
        let (response, idle) =
            crate::stats::ipc_metrics::measure_handler(handler.dispatch_gated(request, transport))
                .await;
        if let Some(method) = method {
            let elapsed = started.elapsed();
            if elapsed >= SLOW_REQUEST {
                tracing::warn!(
                    method = %method,
                    elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                    "IPC request was slow"
                );
            }
        }
        let mut out = serde_json::to_string(&response).context("Failed to serialize response")?;
        out.push('\n');
        writer.write_all(out.as_bytes()).await?;
        span.complete_with_idle(out.len(), idle);
    }
    Ok(())
}
