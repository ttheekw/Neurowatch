//! One authenticated, bounded, outbound control connection per node identity.
//! Failed operations are returned to their existing scheduler; writes are never
//! silently replayed on reconnect. Media continues through LiveKit.
//!
//! The control connection asks for portal events (`?events=1`). When the portal
//! agrees, it sends `{"event": <topic>}` whenever something this node pulls for
//! changes; the event only wakes the matching loop, which then makes its usual
//! signed request (see [`CloudSignals`]).
use super::*;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_tungstenite::{client_async_tls_with_config, MaybeTlsStream, WebSocketStream};

const CONNECT_OPERATION: &str = "websocket_connect";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_INFLIGHT: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) fn socket_url(endpoint: &CloudEndpoint, node_id: &str) -> Result<reqwest::Url> {
    let mut url = endpoint.join(&format!("v1/nodes/{node_id}/ws"))?;
    let scheme = if endpoint.0.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    url.set_scheme(scheme)
        .map_err(|_| CloudError::Invalid("invalid WebSocket URL".into()))?;
    Ok(url)
}

pub(super) fn retry_delay(failures: u32, entropy: u16) -> Duration {
    let base = (1u64 << failures.min(5)).min(30) * 1000;
    let factor = 800 + u64::from(entropy % 1001) * 400 / 1000;
    Duration::from_millis(base * factor / 1000)
}

#[derive(Clone, PartialEq, Eq)]
struct Binding {
    endpoint: String,
    tenant: String,
    node: String,
    epoch: u64,
}

impl Binding {
    fn from_body(endpoint: &CloudEndpoint, body: &Value) -> Result<Self> {
        let tenant = body["tenant_id"].as_str().ok_or(CloudError::State)?;
        let node = body["node_id"].as_str().ok_or(CloudError::State)?;
        uuid::Uuid::parse_str(tenant).map_err(|_| CloudError::State)?;
        uuid::Uuid::parse_str(node).map_err(|_| CloudError::State)?;
        Ok(Self {
            endpoint: endpoint.0.as_str().into(),
            tenant: tenant.into(),
            node: node.into(),
            epoch: body["auth_epoch"].as_u64().ok_or(CloudError::State)?,
        })
    }

    fn matches_endpoint(&self, raw: &str, policy: EndpointPolicy) -> bool {
        // Enrollment retains the operator's URL spelling. Compare the validated
        // parsed URL so a missing root slash does not retire a valid session.
        parse_persisted_endpoint(raw, policy)
            .is_ok_and(|endpoint| endpoint.0.as_str() == self.endpoint)
    }

    fn is_current(&self, inner: &Inner) -> bool {
        let status = &inner.persisted.status;
        status.phase == LinkPhase::Linked
            && status.node_id.as_deref() == Some(&self.node)
            && status.tenant_id.as_deref() == Some(&self.tenant)
            && status.auth_epoch == Some(self.epoch)
            && status
                .endpoint
                .as_deref()
                .is_some_and(|raw| self.matches_endpoint(raw, inner.policy))
    }

    fn still_linked(&self, inner: &Inner) -> bool {
        self.is_current(inner)
            || inner
                .persisted
                .pending_unlink
                .as_ref()
                .is_some_and(|pending| {
                    pending.node_id == self.node
                        && pending.tenant_id == self.tenant
                        && pending.auth_epoch == self.epoch
                        && self.matches_endpoint(&pending.endpoint, inner.policy)
                })
    }
}

struct Command {
    id: String,
    method: String,
    text: String,
    deadline: Instant,
    reply: oneshot::Sender<Result<Value>>,
}

struct Inflight {
    method: String,
    deadline: Instant,
    reply: Option<oneshot::Sender<Result<Value>>>,
}

struct Client {
    binding: Binding,
    commands: mpsc::Sender<Command>,
    stop: watch::Sender<bool>,
    started: Instant,
    ended: Arc<Mutex<Option<Instant>>>,
}

#[derive(Default)]
struct State {
    client: Option<Client>,
    failures: u32,
    retry_at: Option<Instant>,
    binding: Option<Binding>,
}

#[derive(Default)]
pub(super) struct WsTransport {
    state: AsyncMutex<State>,
    /// Ask the portal for events. Only the node's own control connection does;
    /// a temporary socket for an earlier identity must not take them over.
    events: bool,
}

pub(super) struct RequestOptions<'a> {
    pub wait: f64,
    pub known: Option<&'a str>,
    pub events: bool,
    pub events_only: bool,
}

impl WsTransport {
    pub(super) fn with_events() -> Self {
        Self {
            events: true,
            ..Self::default()
        }
    }

    pub(super) async fn close(&self) {
        let mut state = self.state.lock().await;
        if let Some(client) = state.client.take() {
            let _ = client.stop.send(true);
        }
        state.retry_at = None;
    }

    pub(super) async fn request(
        &self,
        node: &CloudNode,
        endpoint: &CloudEndpoint,
        method: &str,
        body: &Value,
        options: RequestOptions<'_>,
    ) -> Result<Value> {
        let binding = Binding::from_body(endpoint, body)?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let sender = tokio::time::timeout_at(deadline, self.client(node, endpoint, binding))
            .await
            .map_err(|_| CloudError::Network("WebSocket connection timed out".into()))??;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut message = json!({"id": id, "method": method, "body": body,
            "wait": options.wait, "known": options.known});
        // Event relay capabilities belong to the pull, not the signed proof.
        // Keep other messages compatible with portals that forbid extra fields.
        if method == CONFIG_PULL_OPERATION {
            message["events"] = json!(options.events);
            message["events_only"] = json!(options.events_only);
        }
        let text = message.to_string();
        if text.len() > MAX_BYTES {
            return Err(CloudError::Invalid("WebSocket message too large".into()));
        }
        let (tx, rx) = oneshot::channel();
        tokio::time::timeout_at(
            deadline,
            sender.send(Command {
                id,
                method: method.into(),
                text,
                deadline,
                reply: tx,
            }),
        )
        .await
        .map_err(|_| CloudError::Network("WebSocket queue timed out".into()))?
        .map_err(|_| CloudError::Network("WebSocket disconnected".into()))?;
        tokio::time::timeout_at(deadline, rx)
            .await
            .map_err(|_| CloudError::Network("WebSocket request timed out".into()))?
            .map_err(|_| CloudError::Network("WebSocket disconnected".into()))?
    }

    async fn client(
        &self,
        node: &CloudNode,
        endpoint: &CloudEndpoint,
        binding: Binding,
    ) -> Result<mpsc::Sender<Command>> {
        let mut state = self.state.lock().await;
        if let Some(client) = &state.client {
            if client.binding == binding && !client.commands.is_closed() {
                return Ok(client.commands.clone());
            }
        }
        if state.binding.as_ref() != Some(&binding) {
            state.failures = 0;
            state.retry_at = None;
            state.binding = Some(binding.clone());
        }
        if let Some(client) = state.client.take() {
            let _ = client.stop.send(true);
            if client.binding == binding {
                if client.started.elapsed() > Duration::from_secs(30) {
                    state.failures = 0;
                }
                let ended = client
                    .ended
                    .lock()
                    .ok()
                    .and_then(|end| *end)
                    .unwrap_or_else(Instant::now);
                state.retry_at = Some(ended + retry_delay(state.failures, entropy()));
                state.failures = state.failures.saturating_add(1);
            }
        }
        if state.retry_at.is_some_and(|at| Instant::now() < at) {
            return Err(CloudError::Network("WebSocket reconnect backoff".into()));
        }
        // Persist the retry gate before awaiting: a scheduler cancelling this
        // future must not turn parallel reporting loops into a reconnect storm.
        state.retry_at =
            Some(Instant::now() + Duration::from_secs(10) + retry_delay(state.failures, entropy()));
        tracing::info!(target: "cloud_ws::connection", node_id = %binding.node,
            cloud_endpoint = %binding.endpoint, "[ws.connection] connecting");
        let connected = tokio::time::timeout(
            Duration::from_secs(10),
            connect(node, endpoint, &binding, self.events),
        )
        .await
        .unwrap_or_else(|_| Err(CloudError::Network("WebSocket connection timed out".into())));
        let (socket, events) = match connected {
            Ok(connected) => connected,
            Err(error) => {
                let pause = if matches!(error, CloudError::Refused(_)) {
                    Duration::from_secs(30)
                } else {
                    retry_delay(state.failures, entropy())
                };
                state.retry_at = Some(Instant::now() + pause);
                state.failures = state.failures.saturating_add(1);
                // Log only fixed diagnostic codes, never driver/server text.
                let failure = match &error {
                    CloudError::Refused(_) => "authentication_refused",
                    CloudError::Network(message) => match message.as_str() {
                        "WebSocket DNS failed" => "dns_failed",
                        "WebSocket TCP connection failed" => "tcp_connection_failed",
                        "WebSocket TCP setup failed" => "tcp_setup_failed",
                        "WebSocket upgrade or TLS failed" => "upgrade_or_tls_failed",
                        "WebSocket TLS configuration failed" => "tls_configuration_failed",
                        "WebSocket connection timed out" => "connection_timeout",
                        "WebSocket handshake disconnected" => "handshake_disconnected",
                        _ => "protocol_failed",
                    },
                    CloudError::Invalid(_) => "endpoint_rejected",
                    _ => "local_state_unavailable",
                };
                tracing::warn!(target: "cloud_ws::connection", node_id = %binding.node,
                    failure, retry_ms = pause.as_millis() as u64,
                    "[ws.connection] connect failed; reconnect scheduled");
                return Err(error);
            }
        };
        let (commands, receiver) = mpsc::channel(MAX_INFLIGHT);
        let (stop, stopping) = watch::channel(false);
        let inner = Arc::clone(&node.inner);
        let running_binding = binding.clone();
        let ended = Arc::new(Mutex::new(None));
        let running_ended = Arc::clone(&ended);
        let signals = Arc::clone(&node.signals);
        let connection = if events { signals.connected() } else { 0 };
        tokio::spawn(async move {
            run(
                socket,
                receiver,
                stopping,
                inner,
                running_binding,
                running_ended,
                (signals, connection),
            )
            .await;
        });
        state.client = Some(Client {
            binding: binding.clone(),
            commands: commands.clone(),
            stop,
            started: Instant::now(),
            ended,
        });
        state.retry_at = None;
        if node.lock().is_ok_and(|inner| binding.is_current(&inner)) {
            node.mark_contact();
        }
        tracing::info!(target: "cloud_ws::connection", events, "[ws.connection] connected and authenticated");
        Ok(commands)
    }
}

fn entropy() -> u16 {
    u16::from_le_bytes(
        uuid::Uuid::new_v4().as_bytes()[..2]
            .try_into()
            .expect("two bytes"),
    )
}

/// Choose the crypto provider for this client explicitly, independently of
/// build dependencies or other libraries' process-wide TLS configuration.
pub(super) fn tls_connector() -> Result<tokio_tungstenite::Connector> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| CloudError::Network("WebSocket TLS configuration failed".into()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

/// Connect and authenticate; also whether the portal will send events.
async fn connect(
    node: &CloudNode,
    endpoint: &CloudEndpoint,
    binding: &Binding,
    events: bool,
) -> Result<(Socket, bool)> {
    let host = endpoint.0.host_str().ok_or(CloudError::State)?;
    let port = endpoint
        .0
        .port_or_known_default()
        .ok_or(CloudError::State)?;
    let addresses = if endpoint.0.scheme() == "http" && host == "127.0.0.1" {
        vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)]
    } else {
        let resolved: Vec<_> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| CloudError::Network("WebSocket DNS failed".into()))?
            .collect();
        validate_resolved_ips(&resolved.iter().map(SocketAddr::ip).collect::<Vec<_>>())?;
        resolved
    };
    // Connect only to checked addresses; retain the original host for TLS/SNI.
    let stream = TcpStream::connect(addresses.as_slice())
        .await
        .map_err(|_| CloudError::Network("WebSocket TCP connection failed".into()))?;
    stream
        .set_nodelay(true)
        .map_err(|_| CloudError::Network("WebSocket TCP setup failed".into()))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_BYTES))
        .max_frame_size(Some(MAX_BYTES));
    let mut url = socket_url(endpoint, &binding.node)?;
    if events {
        url.set_query(Some("events=1"));
    }
    let (mut socket, _) = client_async_tls_with_config(
        url.as_str(),
        stream,
        Some(config),
        if endpoint.0.scheme() == "https" {
            Some(tls_connector()?)
        } else {
            None
        },
    )
    .await
    .map_err(|_| CloudError::Network("WebSocket upgrade or TLS failed".into()))?;
    let key = node.lock()?.key.clone();
    let now = OffsetDateTime::now_utc();
    let mut request = json!({"protocol_version":1,"tenant_id":binding.tenant,"node_id":binding.node,
        "audience":NODE_CHALLENGE_AUDIENCE,"operation":CONNECT_OPERATION,"auth_epoch":binding.epoch,
        "issued_at":fmt_time(now)?,"expires_at":fmt_time(now+TimeDuration::seconds(60))?,
        "nonce":URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes()),"signature":""});
    request["signature"] = Value::String(sign_value(&key, &without_signature(&request))?);
    let issued = exchange(&mut socket, "challenge", request).await?;
    let challenge = parse_challenge_response(issued, CONNECT_OPERATION, OffsetDateTime::now_utc())?;
    let now = OffsetDateTime::now_utc();
    let mut proof = json!({"protocol_version":1,"tenant_id":binding.tenant,"node_id":binding.node,
        "audience":NODE_AUDIENCE,"auth_epoch":binding.epoch,"issued_at":fmt_time(now)?,
        "expires_at":fmt_time(now+TimeDuration::seconds(60))?,"challenge":challenge.challenge,"signature":""});
    proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
    let result = exchange(&mut socket, "authenticate", proof).await?;
    if result["protocol_version"] != 1
        || result["node_id"] != binding.node
        || result["auth_epoch"] != binding.epoch
    {
        return Err(CloudError::Network(
            "invalid WebSocket authentication response".into(),
        ));
    }
    // A portal without events ignores the request and leaves the field out.
    let offered = events && result.get("events") == Some(&Value::Bool(true));
    Ok((socket, offered))
}

async fn exchange(socket: &mut Socket, method: &str, body: Value) -> Result<Value> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let text = json!({"id":id,"method":method,"body":body}).to_string();
    tracing::info!(target: "cloud_ws::message", method, bytes = text.len(), "[ws.message] sent");
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| CloudError::Network("WebSocket send failed".into()))?;
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let raw: Value = serde_json::from_str(text.as_str())
                    .map_err(|_| CloudError::Network("invalid WebSocket response".into()))?;
                // Events follow authentication; connecting wakes every loop anyway.
                if raw.get("id").is_none() && raw.get("event").is_some() {
                    continue;
                }
                if raw["id"].as_str() != Some(&id) {
                    return Err(CloudError::Network("unexpected WebSocket response".into()));
                }
                tracing::info!(target: "cloud_ws::message", method, bytes = text.len(), "[ws.message] received");
                return response_body(&raw);
            }
            Some(Ok(Message::Ping(data))) => {
                socket
                    .send(Message::Pong(data))
                    .await
                    .map_err(|_| CloudError::Network("WebSocket pong failed".into()))?;
            }
            _ => {
                return Err(CloudError::Network(
                    "WebSocket handshake disconnected".into(),
                ))
            }
        }
    }
}

fn response_body(raw: &Value) -> Result<Value> {
    let status = raw["status"]
        .as_u64()
        .filter(|s| (100..600).contains(s))
        .ok_or_else(|| CloudError::Network("invalid WebSocket status".into()))?
        as u16;
    if (200..300).contains(&status) {
        if raw.get("body").is_none() {
            return Err(CloudError::Network(
                "missing WebSocket response body".into(),
            ));
        }
        Ok(raw["body"].clone())
    } else {
        Err(http_error(reqwest::StatusCode::from_u16(status).map_err(
            |_| CloudError::Network("invalid WebSocket status".into()),
        )?))
    }
}

async fn run(
    socket: Socket,
    mut commands: mpsc::Receiver<Command>,
    mut stop: watch::Receiver<bool>,
    inner: Arc<Mutex<Inner>>,
    binding: Binding,
    ended: Arc<Mutex<Option<Instant>>>,
    (signals, connection): (Arc<CloudSignals>, u64),
) {
    let (mut writer, mut reader) = socket.split();
    let mut pending: HashMap<String, Inflight> = HashMap::new();
    let mut check = tokio::time::interval(Duration::from_secs(1));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    let mut refusal = None;
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { break; }
            }
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if command.reply.is_closed() || command.deadline <= Instant::now() { continue; }
                if pending.len() >= MAX_INFLIGHT {
                    let _ = command.reply.send(Err(CloudError::Network("WebSocket channel is busy".into())));
                    continue;
                }
                tracing::info!(target: "cloud_ws::message", node_id = %binding.node,
                    method = %command.method, bytes = command.text.len(), "[ws.message] sent");
                if !matches!(tokio::time::timeout(Duration::from_secs(5), writer.send(Message::Text(command.text.into()))).await, Ok(Ok(()))) { break; }
                pending.insert(command.id, Inflight { method: command.method, deadline: command.deadline, reply: Some(command.reply) });
            }
            frame = reader.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(raw) = serde_json::from_str::<Value>(text.as_str()) else { break; };
                        if raw.get("id").is_none() {
                            // An event carries no data, so an unknown topic is only skipped.
                            let Some(topic) = raw["event"].as_str() else { break; };
                            if let Some(topic) = signals.wake(topic) {
                                tracing::info!(target: "cloud_ws::message", node_id = %binding.node,
                                    event = topic, "[ws.event] received");
                            }
                            continue;
                        }
                        let Some(id) = raw["id"].as_str() else { break; };
                        let Some(request) = pending.remove(id) else { continue; };
                        tracing::info!(target: "cloud_ws::message", node_id = %binding.node,
                            method = %request.method, status = raw["status"].as_u64().unwrap_or(0),
                            bytes = text.len(), "[ws.message] received");
                        let result = response_body(&raw);
                        if let Err(CloudError::Refused(code)) = &result { refusal = Some(*code); }
                        if result.is_ok() {
                            if let Ok(mut state) = inner.lock() {
                                if binding.is_current(&state) { state.last_contact = Some(std::time::Instant::now()); state.refused = false; }
                            }
                        }
                        if let Some(reply) = request.reply { let _ = reply.send(result); }
                        if refusal.is_some() { break; }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if !matches!(tokio::time::timeout(Duration::from_secs(5), writer.send(Message::Pong(data))).await, Ok(Ok(()))) { break; }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(close))) => {
                        if close.is_some_and(|c| u16::from(c.code) == 4401) { refusal = Some(401); }
                        break;
                    }
                    _ => break,
                }
            }
            _ = check.tick() => {
                if !inner.lock().is_ok_and(|state| binding.still_linked(&state)) { break; }
                let now = Instant::now();
                if pending.values().any(|p| p.reply.is_none() && p.deadline <= now) { break; }
                pending.retain(|_, p| p.deadline > now && p.reply.as_ref().map_or(true, |r| !r.is_closed()));
                if now >= next_heartbeat {
                    let id = uuid::Uuid::new_v4().simple().to_string();
                    let text = json!({"id":id,"method":"heartbeat","body":{}}).to_string();
                    tracing::info!(target: "cloud_ws::message", node_id = %binding.node, method = "heartbeat", "[ws.message] sent");
                    if !matches!(tokio::time::timeout(Duration::from_secs(5), writer.send(Message::Text(text.into()))).await, Ok(Ok(()))) { break; }
                    pending.insert(id, Inflight { method: "heartbeat".into(), deadline: now + HEARTBEAT_TIMEOUT, reply: None });
                    next_heartbeat = now + HEARTBEAT_INTERVAL;
                }
            }
        }
    }
    // Close the queue before waking callers so the next operation sees a dead session.
    commands.close();
    signals.disconnected(connection);
    if let Ok(mut ended) = ended.lock() {
        *ended = Some(Instant::now());
    }
    if let Ok(mut state) = inner.lock() {
        if binding.is_current(&state) {
            state.last_contact = None;
            if refusal.is_some() {
                state.refused = true;
            }
        }
    }
    for (_, request) in pending.drain() {
        if let Some(reply) = request.reply {
            let error = refusal
                .map(CloudError::Refused)
                .unwrap_or_else(|| CloudError::Network("WebSocket disconnected".into()));
            let _ = reply.send(Err(error));
        }
    }
    while let Ok(command) = commands.try_recv() {
        let _ = command
            .reply
            .send(Err(CloudError::Network("WebSocket disconnected".into())));
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), writer.close()).await;
    tracing::info!(target: "cloud_ws::connection", node_id = %binding.node, "[ws.connection] disconnected");
}

#[cfg(test)]
mod binding_tests {
    use super::*;

    fn saved_link(raw: &str) -> (CloudNode, PathBuf, Binding) {
        let dir = std::env::temp_dir().join(format!("nw-ws-url-{}", uuid::Uuid::new_v4()));
        let policy = EndpointPolicy {
            allow_loopback_http: true,
            pinned: None,
        };
        let node = CloudNode::open_with_options(
            dir.clone(),
            CloudOptions {
                endpoint: None,
                policy,
            },
        )
        .unwrap();
        let endpoint = CloudEndpoint::parse_with(raw, policy).unwrap();
        let body = json!({"tenant_id":"6ba7b811-9dad-11d1-80b4-00c04fd430c8",
            "node_id":"6ba7b812-9dad-11d1-80b4-00c04fd430c8", "auth_epoch":1});
        let binding = Binding::from_body(&endpoint, &body).unwrap();
        {
            let mut inner = node.lock().unwrap();
            inner.endpoint = Some(endpoint);
            inner.persisted.status = CloudStatus {
                phase: LinkPhase::Linked,
                endpoint: Some(raw.into()),
                tenant_id: body["tenant_id"].as_str().map(str::to_owned),
                node_id: body["node_id"].as_str().map(str::to_owned),
                auth_epoch: Some(1),
                last_check_in: None,
            };
        }
        (node, dir, binding)
    }

    #[test]
    fn legacy_link_urls_bind_to_the_validated_canonical_endpoint() {
        for raw in ["http://127.0.0.1:8000", "https://CLOUD.EXAMPLE.COM:443"] {
            let (node, dir, binding) = saved_link(raw);
            {
                let inner = node.lock().unwrap();
                assert_ne!(
                    inner.persisted.status.endpoint.as_deref(),
                    Some(binding.endpoint.as_str())
                );
                assert!(
                    binding.is_current(&inner),
                    "the saved URL and its parsed form identify the same cloud: {raw}"
                );
                assert!(binding.still_linked(&inner));
            }
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn legacy_pending_unlink_urls_keep_the_retired_identity_bound() {
        let (mut node, dir, binding) = saved_link("http://127.0.0.1:8000");
        node.unlink_local().unwrap();
        {
            let inner = node.lock().unwrap();
            assert!(!binding.is_current(&inner));
            assert!(
                binding.still_linked(&inner),
                "a pending unlink must match the parsed form of its saved URL"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn binding_rejects_different_cloud_paths_and_identities() {
        let (node, dir, binding) = saved_link("https://cloud.example.com/portal/");
        {
            let mut inner = node.lock().unwrap();
            assert!(binding.is_current(&inner));
            for raw in [
                "https://other.example.com/portal/",
                "https://cloud.example.com:444/portal/",
                "https://cloud.example.com/portal",
                "https://cloud.example.com/other/",
                "https://cloud.example.com/portal/?query=1",
                "https://cloud.example.com/portal/#fragment",
            ] {
                inner.persisted.status.endpoint = Some(raw.into());
                assert!(
                    !binding.is_current(&inner),
                    "different cloud URL must invalidate the socket: {raw}"
                );
                assert!(!binding.still_linked(&inner));
            }
            inner.persisted.status.endpoint = Some(binding.endpoint.clone());
            inner.persisted.status.auth_epoch = Some(2);
            assert!(!binding.still_linked(&inner));
            inner.persisted.status.auth_epoch = Some(1);
            inner.persisted.status.tenant_id = Some(uuid::Uuid::new_v4().to_string());
            assert!(!binding.still_linked(&inner));
            inner.persisted.status.tenant_id = Some(binding.tenant.clone());
            inner.persisted.status.node_id = Some(uuid::Uuid::new_v4().to_string());
            assert!(!binding.still_linked(&inner));
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
