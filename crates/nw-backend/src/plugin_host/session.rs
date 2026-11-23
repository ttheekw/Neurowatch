//! One plugin's session: connect, `hello`, keep its bindings in step with
//! config, forward media, take its events. Runs on its own thread; a reader
//! and a writer thread own the socket's two directions, so a plugin that
//! stops reading only fills (and drops from) its own bounded outbox.
use super::overlays::{self, OverlayStore};
use super::pump::{MediaOptions, Pump};
use super::{BindingView, Desired, DesiredBinding, ServerStatus};
use crate::events::{EventHub, NewEvent};
use crate::streams::StreamHubs;
use nw_analytics_protocol::{
    address::Address,
    frame::{read_frame, write_frame, Frame, MediaUnit},
    messages::{
        self, Accepted, Attach, CameraHint, CameraInfo, Configure, Detach, EventEnd, EventStart,
        EventUpdate, Hello, Manifest, NodeInfo, Overlay, Refusal, Status, StreamChoice,
    },
    method,
    rpc::{params, Message, RpcError},
    PROTOCOL_VERSION,
};
use nw_core::camera::CameraConfig;
use nw_core::event::{EventKind, EventSource};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

const BACKOFF_S: [u64; 4] = [2, 5, 10, 30];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REFUSED_RETRY: Duration = Duration::from_secs(30);
/// With a fallback to try next, QUIC gives up on its handshake this soon.
const QUIC_BEFORE_FALLBACK: Duration = Duration::from_secs(4);
/// On the fallback (TLS over TCP), try the preferred transport again this often.
const PREFERRED_RETRY: Duration = Duration::from_secs(10 * 60);
const PUMP_RETRY: Duration = Duration::from_secs(10);
const RECONCILE_EVERY: Duration = Duration::from_millis(500);
const OUTBOX_FRAMES: usize = 256;
const OUTBOX_BYTES: usize = 32 * 1024 * 1024;
const MAX_OPEN_EVENTS: usize = 16;
const EVENT_STARTS_PER_SECOND: u32 = 10;
const OVERLAYS_PER_SECOND: u32 = 25;
const MAX_ZONES: usize = 32;
/// A plugin's own event kind (a detection class): its id and its label.
const MAX_KIND_ID: usize = 64;
const MAX_KIND_LABEL: usize = 64;
const MAX_TEXT: usize = 200;

pub(crate) struct Context {
    pub server_id: String,
    /// The transport tried first; `run` swaps it with `fallback` while the
    /// fallback is the one that gets through.
    pub address: Address,
    /// Another way to the same plugin (a hosted plugin's TLS over TCP).
    pub fallback: Option<Address>,
    pub node: NodeInfo,
    pub streams: Arc<StreamHubs>,
    pub events: Arc<EventHub>,
    pub status: Arc<Mutex<HashMap<String, ServerStatus>>>,
    pub overlays: Arc<OverlayStore>,
    pub approval: Option<nw_analytics_protocol::auth::Approval>,
    /// Provided by NeuroWatch: presents the portal's ticket after signing in.
    pub managed: bool,
    pub tickets: Arc<Mutex<HashMap<String, String>>>,
    pub config_path: std::path::PathBuf,
    pub queries: Arc<Mutex<HashMap<String, mpsc::SyncSender<super::Query>>>>,
}

impl Context {
    fn update(&self, change: impl FnOnce(&mut ServerStatus)) {
        if let Ok(mut status) = self.status.lock() {
            let entry = status.entry(self.server_id.clone()).or_default();
            entry.id = self.server_id.clone();
            entry.address = self.address.to_string();
            change(entry);
            entry.track_reachability();
        }
    }

    fn view(&self, camera_id: &str, change: impl FnOnce(&mut BindingView)) {
        self.update(|server| change(server.bindings.entry(camera_id.to_owned()).or_default()));
    }
}

/// The session thread: connects, serves, and reconnects with backoff until
/// `stop`.
pub(crate) fn run(mut context: Context, desired: Arc<Mutex<Desired>>, stop: Arc<AtomicBool>) {
    let mut failures = 0usize;
    // Since when the fallback has been tried first (it got through when the
    // preferred transport did not).
    let mut on_fallback: Option<Instant> = None;
    while !stop.load(Ordering::Acquire) {
        if on_fallback.is_some_and(|since| since.elapsed() >= PREFERRED_RETRY) {
            swap_transports(&mut context);
            on_fallback = None;
        }
        context.update(|s| s.state = "connecting");
        let (result, served) = match connect(&mut context, &mut on_fallback) {
            Ok(conn) => session(&context, conn, &desired, &stop),
            Err(error) => (Err(format!("Could not connect: {error}")), false),
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        if served {
            failures = 0;
        }
        let reason = result
            .err()
            .unwrap_or_else(|| "The plugin closed the session".into());
        tracing::warn!(plugin = %context.server_id, address = %context.address, %reason, "Analytics plugin unavailable");
        context.update(|s| {
            s.state = "error";
            s.detail = Some(reason);
            s.bindings.clear();
        });
        let wait = Duration::from_secs(BACKOFF_S[failures.min(BACKOFF_S.len() - 1)]);
        failures += 1;
        let until = Instant::now() + wait;
        while Instant::now() < until && !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Connect by `context.address`, else at once by its fallback; a fallback
/// that got through is tried first from then on (`on_fallback`).
fn connect(context: &mut Context, on_fallback: &mut Option<Instant>) -> Result<Conn, String> {
    let Some(fallback) = context.fallback.clone() else {
        return Conn::connect(&context.address).map_err(|error| error.to_string());
    };
    let first = match Conn::connect_before_fallback(&context.address) {
        Ok(conn) => return Ok(conn),
        Err(error) => error,
    };
    match Conn::connect_before_fallback(&fallback) {
        Ok(conn) => {
            tracing::info!(plugin = %context.server_id, from = %context.address, to = %fallback, reason = %first,
                "Analytics plugin reached over its other transport");
            swap_transports(context);
            *on_fallback = match on_fallback {
                Some(_) => None,
                None => Some(Instant::now()),
            };
            Ok(conn)
        }
        Err(second) => Err(format!(
            "{}: {first}; {fallback}: {second}",
            context.address
        )),
    }
}

fn swap_transports(context: &mut Context) {
    if let Some(fallback) = context.fallback.as_mut() {
        std::mem::swap(&mut context.address, fallback);
    }
}

/// One connection, start to end. `served`: the plugin answered `hello`.
fn session(
    context: &Context,
    conn: Conn,
    desired: &Mutex<Desired>,
    stop: &AtomicBool,
) -> (Result<(), String>, bool) {
    let io = (|| -> io::Result<_> {
        conn.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok((conn.try_clone()?, conn.try_clone()?))
    })();
    let (reader_conn, writer_conn) = match io {
        Ok(pair) => pair,
        Err(error) => return (Err(error.to_string()), false),
    };
    let (incoming_tx, incoming) = mpsc::sync_channel(64);
    let (query_tx, queries) = mpsc::sync_channel(8);
    if let Ok(mut senders) = context.queries.lock() {
        senders.insert(context.server_id.clone(), query_tx);
    }
    let (out_tx, out_rx) = mpsc::sync_channel(OUTBOX_FRAMES);
    let queued = Arc::new(AtomicUsize::new(0));
    let name = &context.server_id;
    let reader = std::thread::Builder::new()
        .name(format!("plugin-{name}-read"))
        .spawn(move || read_loop(reader_conn, incoming_tx));
    let writer = {
        let queued = Arc::clone(&queued);
        std::thread::Builder::new()
            .name(format!("plugin-{name}-write"))
            .spawn(move || write_loop(writer_conn, out_rx, &queued))
    };
    let mut session = Session {
        context,
        out: Outbox { tx: out_tx, queued },
        incoming,
        manifest: None,
        next_id: 1,
        next_binding: 1,
        runs: HashMap::new(),
        pending: HashMap::new(),
        open: HashMap::new(),
        starts: HashMap::new(),
        drawn: HashMap::new(),
        warned: HashSet::new(),
        last_ping: Instant::now(),
        last_pong: Instant::now(),
        received: (Instant::now(), 0),
        locality: None,
        local_decoded: false,
        credential: None,
        node_key: None,
        presented: None,
        challenge: String::new(),
        authenticated: false,
        authentication_requested: None,
        queries,
        channel_binding: match &conn {
            Conn::Quic(io) => io.channel_binding().into(),
            Conn::Tls(io) => io.channel_binding().into(),
            _ => String::new(),
        },
        tls_peer_key: match &conn {
            Conn::Quic(io) => io.peer_key().map(str::to_owned),
            Conn::Tls(io) => io.peer_key().map(str::to_owned),
            _ => None,
        },
        quic: match &conn {
            Conn::Quic(io) => Some(io.clone()),
            _ => None,
        },
    };
    let result = session.serve(desired, stop);
    let served = session.manifest.is_some();
    session.close();
    drop(session);
    if let Ok(mut senders) = context.queries.lock() {
        senders.remove(&context.server_id);
    }
    conn.shutdown();
    if let Ok(reader) = reader {
        let _ = reader.join();
    }
    if let Ok(writer) = writer {
        let _ = writer.join();
    }
    (result, served)
}

enum Incoming {
    Message(Message),
    Closed(String),
}

enum Pending {
    Hello,
    Authenticate,
    Entitlement,
    Proxy(tokio::sync::oneshot::Sender<Result<Value, String>>),
    Ping,
    Attach { camera_id: String, binding: u32 },
    Configure { camera_id: String, binding: u32 },
    Detach,
}

enum Phase {
    Attaching {
        since: Instant,
    },
    Active {
        pump: Option<Pump>,
        retry_at: Instant,
    },
    Refused {
        until: Instant,
    },
}

struct Run {
    binding: u32,
    camera: CameraConfig,
    config: Value,
    /// The configuration the plugin last accepted or is considering.
    sent_config: Value,
    /// The camera stream chosen for the plugin (`None`: the plugin's choice).
    stream: Option<StreamChoice>,
    /// Never offer it the recorder's decoded pictures.
    own_decoder: bool,
    /// What the plugin last said it consumes.
    accepted: Accepted,
    moving: bool,
    phase: Phase,
}

struct OpenEvent {
    id: i64,
    binding: u32,
}

struct Session<'a> {
    context: &'a Context,
    out: Outbox,
    incoming: mpsc::Receiver<Incoming>,
    manifest: Option<Manifest>,
    next_id: u64,
    next_binding: u32,
    /// By camera id.
    runs: HashMap<String, Run>,
    pending: HashMap<u64, (Pending, Instant)>,
    /// By the plugin's event number.
    open: HashMap<u64, OpenEvent>,
    /// Event starts per binding in the current second.
    starts: HashMap<u32, (Instant, u32)>,
    /// Drawings per binding and layer in the current second.
    drawn: HashMap<(u32, String), (Instant, u32)>,
    /// Problems already logged once (unknown kinds and methods).
    warned: HashSet<String>,
    last_ping: Instant,
    last_pong: Instant,
    received: (Instant, u32),
    locality: Option<nw_analytics_sdk::locality::LocalityProbe>,
    local_decoded: bool,
    credential: Option<String>,
    challenge: String,
    /// The recorder's identity key, and the ticket last shown (hosted plugins).
    node_key: Option<nw_analytics_sdk::identity::SigningKey>,
    presented: Option<String>,
    authenticated: bool,
    authentication_requested: Option<Instant>,
    quic: Option<nw_analytics_sdk::quic::QuicIo>,
    channel_binding: String,
    tls_peer_key: Option<String>,
    queries: mpsc::Receiver<super::Query>,
}

impl Session<'_> {
    fn serve(&mut self, desired: &Mutex<Desired>, stop: &AtomicBool) -> Result<(), String> {
        if matches!(self.context.address, Address::Unix(_))
            || (cfg!(windows) && matches!(self.context.address, Address::Pipe(_)))
        {
            let directory =
                std::env::temp_dir().join(format!("neurowatch-analytics-{}", std::process::id()));
            match nw_analytics_sdk::locality::LocalityProbe::create(&directory) {
                Ok(probe) => self.locality = Some(probe),
                Err(error) => {
                    tracing::debug!(%error,"Local decoded-picture sharing is unavailable")
                }
            }
        }
        self.challenge =
            nw_analytics_sdk::identity::challenge().map_err(|error| error.to_string())?;
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            node: self.context.node.clone(),
            locality: self.locality.as_ref().map(|probe| probe.offer()),
            challenge: self.challenge.clone(),
        };
        let descriptor = self
            .locality
            .as_ref()
            .and_then(|probe| probe.ring.descriptor().ok());
        self.request_with_descriptor(method::HELLO, hello, Pending::Hello, descriptor)?;
        let deadline = Instant::now() + HELLO_TIMEOUT;
        while self.manifest.is_none() {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("The plugin did not answer hello".into());
            }
            self.receive(Duration::from_millis(100))?;
        }
        let mut reconciled = Instant::now() - RECONCILE_EVERY;
        while !stop.load(Ordering::Acquire) {
            for _ in 0..8 {
                let Ok(query) = self.queries.try_recv() else {
                    break;
                };
                if query.reply.is_closed() {
                    continue;
                }
                if self.pending.len() >= 64 {
                    let _ = query
                        .reply
                        .send(Err("Plugin request capacity reached".into()));
                    continue;
                }
                if !self.authenticated {
                    let _ = query.reply.send(Err("Plugin is not authorized".into()));
                    continue;
                }
                self.request(&query.method, query.params, Pending::Proxy(query.reply))?;
            }
            if self.manifest.as_ref().is_some_and(|manifest| manifest.ping) {
                if self.last_pong.elapsed() >= Duration::from_secs(15) {
                    return Err("The plugin stopped answering liveness checks".into());
                }
                if self.last_ping.elapsed() >= Duration::from_secs(5) {
                    self.request(method::PING, Value::Null, Pending::Ping)?;
                    self.last_ping = Instant::now();
                }
            }
            if reconciled.elapsed() >= RECONCILE_EVERY {
                let wanted = desired.lock().map(|d| d.clone()).unwrap_or_default();
                self.reconcile(&wanted.bindings)?;
                reconciled = Instant::now();
            }
            self.receive(Duration::from_millis(100))?;
        }
        Ok(())
    }

    /// Handle what arrived within `wait`.
    fn receive(&mut self, wait: Duration) -> Result<(), String> {
        let first = match self.incoming.recv_timeout(wait) {
            Ok(incoming) => incoming,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("Connection lost".into()),
        };
        let mut next = Some(first);
        let mut handled = 0;
        while let Some(incoming) = next {
            if self.received.0.elapsed() >= Duration::from_secs(1) {
                self.received = (Instant::now(), 0);
            }
            self.received.1 += 1;
            if self.received.1 > 1000 {
                return Err("The plugin exceeded its message rate limit".into());
            }
            match incoming {
                Incoming::Message(message) => self.handle(message)?,
                Incoming::Closed(reason) => return Err(reason),
            }
            handled += 1;
            if handled >= 64 {
                break;
            }
            next = self.incoming.try_recv().ok();
        }
        Ok(())
    }

    fn request(
        &mut self,
        name: &str,
        params: impl serde::Serialize,
        pending: Pending,
    ) -> Result<u64, String> {
        self.request_with_descriptor(name, params, pending, None)
    }

    fn request_with_descriptor(
        &mut self,
        name: &str,
        params: impl serde::Serialize,
        pending: Pending,
        descriptor: Option<File>,
    ) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let message = Message::request(id, name, params);
        let sent = match descriptor {
            Some(file) => self.out.descriptor(&message, file),
            None => self.out.message(&message),
        };
        if !sent {
            return Err("Connection lost".into());
        }
        self.pending.insert(id, (pending, Instant::now()));
        Ok(id)
    }

    fn notify(&self, name: &str, params: impl serde::Serialize) -> Result<(), String> {
        if self.out.message(&Message::notification(name, params)) {
            Ok(())
        } else {
            Err("Connection lost".into())
        }
    }

    fn handle(&mut self, message: Message) -> Result<(), String> {
        match message {
            Message::Response { id, result } => {
                let Some((pending, _)) = self.pending.remove(&id) else {
                    return Ok(());
                };
                match pending {
                    Pending::Hello => self.on_hello(result)?,
                    Pending::Proxy(reply) => { let _ = reply.send(result.map_err(|error| clip(error.message))); }
                    Pending::Authenticate => {
                        if let Err(error) = result {
                            self.refusal(&error);
                            return Err(format!("The plugin refused authorization: {}", self.plugin_detail(error.message)));
                        }
                        if self.context.managed {
                            self.present_ticket()?;
                            self.authentication_requested = Some(Instant::now());
                        } else {
                            self.authentication_requested = None;
                            self.authenticated = true;
                            self.context.update(|status| { status.state = "connected"; status.detail = None; });
                        }
                    }
                    Pending::Entitlement => {
                        if let Err(error) = result {
                            self.refusal(&error);
                            return Err(format!("The service refused this recorder: {}", self.plugin_detail(error.message)));
                        }
                        self.authentication_requested = None;
                        self.authenticated = true;
                        self.context.update(|status| { status.state = "connected"; status.detail = None; });
                    }
                    Pending::Ping => {
                        result.map_err(|_|"The plugin refused a liveness check".to_owned())?;
                        self.last_pong = Instant::now();
                    }
                    Pending::Attach { camera_id, binding } => {
                        self.on_attached(&camera_id, binding, result)?
                    }
                    Pending::Configure { camera_id, binding } => {
                        self.on_configured(&camera_id, binding, result)
                    }
                    Pending::Detach => {}
                }
            }
            Message::Notification { .. } if !self.authenticated => {}
            Message::Notification { method: name, params: values } => match name.as_str() {
                method::EVENT_START => {
                    if let Ok(start) = params::<EventStart>(values) {
                        self.event_start(start);
                    }
                }
                method::EVENT_UPDATE => {
                    if let Ok(update) = params::<EventUpdate>(values) {
                        self.event_update(update);
                    }
                }
                method::EVENT_END => {
                    if let Ok(end) = params::<EventEnd>(values) {
                        self.event_end(&end);
                    }
                }
                method::STATUS => {
                    if let Ok(status) = params::<Status>(values) {
                        self.on_status(status);
                    }
                }
                method::OVERLAY => {
                    if let Ok(overlay) = params::<Overlay>(values) {
                        self.on_overlay(overlay);
                    }
                }
                other => self.warn_once(format!("method:{other}"), || {
                    tracing::debug!(plugin = %self.context.server_id, method = %other, "Plugin notification ignored");
                }),
            },
            Message::Request { id, .. } => {
                let error = RpcError::new(nw_analytics_protocol::error_code::METHOD_NOT_FOUND, "Method not found");
                self.out.message(&Message::reply(id, Err::<Value, _>(error)));
            }
        }
        Ok(())
    }

    fn on_hello(&mut self, result: Result<Value, RpcError>) -> Result<(), String> {
        if let Err(error) = &result {
            self.refusal(error);
        }
        let value =
            result.map_err(|error| format!("The plugin refused hello: {}", error.message))?;
        let mut manifest: Manifest = params(value)
            .map_err(|error| format!("The plugin's manifest is invalid: {}", error.message))?;
        if manifest.protocol != PROTOCOL_VERSION {
            return Err(format!(
                "The plugin speaks protocol {}; this NeuroWatch speaks {PROTOCOL_VERSION}",
                manifest.protocol
            ));
        }
        if manifest.permissions.len() > 16
            || manifest.permissions.iter().any(|permission| {
                ![
                    "video",
                    "audio",
                    "decoded_frames",
                    "events",
                    "overlays",
                    "web_ui",
                ]
                .contains(&permission.as_str())
            })
        {
            return Err("The plugin requests unsupported permissions".into());
        }
        if manifest.identity.is_none() {
            return Err(format!(
                "{} {} does not support session authentication. Update and restart the plugin daemon before connecting it to this recorder.",
                manifest.name.chars().take(128).collect::<String>(),
                manifest.version.chars().take(32).collect::<String>()
            ));
        }
        let channel = &self.channel_binding;
        if !nw_analytics_sdk::identity::verify_manifest(&manifest, &self.challenge, channel) {
            return Err("The plugin did not prove its session identity".into());
        }
        let identity = manifest.identity.clone().ok_or("Plugin identity missing")?;
        if self
            .tls_peer_key
            .as_ref()
            .is_some_and(|key| key != &identity.public_key)
        {
            return Err("The plugin's signed identity does not match its TLS key".into());
        }
        if let Some(io) = &self.quic {
            io.delivery(
                manifest.media.video.delivery,
                manifest.media.video.max_buffer_ms,
            );
        }
        let approved = self.context.approval.as_ref().is_some_and(|approval| {
            approval.public_key == identity.public_key
                && manifest
                    .permissions
                    .iter()
                    .all(|permission| approval.permissions.contains(permission))
        });
        self.local_decoded = self
            .locality
            .as_ref()
            .is_some_and(|probe| probe.verify(manifest.locality_proof.as_deref()))
            && manifest
                .permissions
                .iter()
                .any(|permission| permission == "decoded_frames");
        self.locality = None;
        manifest.locality_proof = None;
        self.last_ping = Instant::now();
        self.last_pong = Instant::now();
        tracing::info!(plugin = %self.context.server_id, id = %manifest.id, name = %manifest.name, version = %manifest.version, local_decoded=self.local_decoded, "Analytics plugin connected");
        let shown = manifest.clone();
        self.context.update(|s| {
            s.refusal = None;
            s.state = if approved {
                "authorizing"
            } else {
                "pending_approval"
            };
            s.detail = (!approved).then(|| {
                "Approve this plugin's key and requested permissions before binding cameras".into()
            });
            s.manifest = Some(shown);
        });
        self.manifest = Some(manifest);
        if approved {
            let parent = self
                .context
                .config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let key =
                crate::cloud::link::ProtectedKeyStore::open(parent.join("cloud-node/node.key"))
                    .and_then(|store| store.load_or_generate())
                    .map_err(|error| error.to_string())?;
            let credential_path = super::credentials::path(
                &self.context.config_path,
                &self.context.server_id,
                &identity.public_key,
            )?;
            let credential = match nw_analytics_sdk::secrets::read(
                &credential_path,
                super::credentials::PURPOSE,
            ) {
                Ok(bytes) => {
                    Some(String::from_utf8(bytes).map_err(|_| "Invalid stored vendor credential")?)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(format!("Could not read plugin credential: {error}")),
            };
            self.credential = credential.clone();
            self.node_key = Some(key.clone());
            let authentication = nw_analytics_sdk::identity::authenticate(
                &key,
                &self.challenge,
                &identity,
                credential,
            );
            self.request(method::AUTHENTICATE, authentication, Pending::Authenticate)?;
            self.authentication_requested = Some(Instant::now());
        }
        Ok(())
    }

    fn latest_ticket(&self) -> Option<String> {
        self.context
            .tickets
            .lock()
            .ok()
            .and_then(|tickets| tickets.get(&self.context.server_id).cloned())
    }

    /// Show the portal's ticket, bound to this connection by the recorder's key.
    fn present_ticket(&mut self) -> Result<(), String> {
        let ticket = self
            .latest_ticket()
            .ok_or("Waiting for NeuroWatch to provide this service")?;
        let key = self
            .node_key
            .as_ref()
            .ok_or("This recorder has no identity key")?;
        let signature = nw_service_protocol::sign_hex(
            key,
            &nw_service_protocol::entitlement_payload(&ticket, self.channel_binding.as_bytes()),
        );
        self.presented = Some(ticket.clone());
        self.request(
            method::ENTITLEMENT,
            messages::Entitlement { ticket, signature },
            Pending::Entitlement,
        )?;
        Ok(())
    }

    fn refusal(&self, error: &RpcError) {
        let Some(mut refusal) = error
            .data
            .clone()
            .and_then(|data| serde_json::from_value::<Refusal>(data).ok())
        else {
            return;
        };
        refusal.code = refusal.code.chars().take(64).collect();
        refusal.message = self.plugin_detail(refusal.message);
        refusal.url = refusal
            .url
            .filter(|url| url.len() <= 2048)
            .and_then(|url| reqwest::Url::parse(&url).ok())
            .filter(|url| {
                url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
            })
            .map(|url| url.to_string());
        self.context.update(|status| status.refusal = Some(refusal));
    }

    fn plugin_detail(&self, text: String) -> String {
        clip(match &self.credential {
            Some(secret) if !secret.is_empty() => text.replace(secret, "[credential]"),
            _ => text,
        })
    }

    fn reconcile(&mut self, wanted: &[DesiredBinding]) -> Result<(), String> {
        if self
            .authentication_requested
            .is_some_and(|since| since.elapsed() > REQUEST_TIMEOUT)
        {
            return Err("The plugin did not answer authorization".into());
        }
        if !self.authenticated {
            return Ok(());
        }
        if self.context.managed {
            let latest = self.latest_ticket();
            if latest.is_none() {
                return Err("NeuroWatch no longer provides this service to this recorder".into());
            }
            if latest != self.presented {
                self.present_ticket()?;
            }
        }
        let now = Instant::now();
        let gone: Vec<String> = self
            .runs
            .keys()
            .filter(|id| !wanted.iter().any(|want| &want.camera.id == *id))
            .cloned()
            .collect();
        for camera_id in gone {
            self.detach(&camera_id, "unbound")?;
        }
        for want in wanted {
            let Some(run) = self.runs.get_mut(&want.camera.id) else {
                self.attach(want)?;
                continue;
            };
            let restart_media = stream_fingerprint(&run.camera) != stream_fingerprint(&want.camera)
                || run.stream != want.stream
                || run.own_decoder != want.own_decoder;
            run.stream = want.stream;
            run.own_decoder = want.own_decoder;
            run.camera = want.camera.clone();
            let configure = run.config != want.config;
            run.config = want.config.clone();
            let hint = run.moving != want.moving;
            run.moving = want.moving;
            let binding = run.binding;
            let mut attach_again = false;
            match &mut run.phase {
                Phase::Attaching { since } => {
                    if since.elapsed() > REQUEST_TIMEOUT {
                        run.phase = Phase::Refused {
                            until: now + REFUSED_RETRY,
                        };
                        self.context.view(&want.camera.id, |v| {
                            v.state = "refused";
                            v.detail = Some("The plugin did not answer".into());
                        });
                    }
                }
                Phase::Refused { until } => attach_again = now >= *until || configure,
                Phase::Active { pump, retry_at } => {
                    if restart_media {
                        *pump = None;
                        *retry_at = now;
                    }
                }
            }
            if attach_again {
                self.runs.remove(&want.camera.id);
                self.attach(want)?;
                continue;
            }
            if matches!(
                self.runs.get(&want.camera.id).map(|r| &r.phase),
                Some(Phase::Active { .. })
            ) {
                if configure {
                    self.configure(&want.camera.id)?;
                }
                if hint {
                    self.notify(
                        method::CAMERA_HINT,
                        CameraHint {
                            binding,
                            moving: want.moving,
                        },
                    )?;
                }
                self.ensure_pump(&want.camera.id);
            }
        }
        self.expire_requests();
        Ok(())
    }

    fn attach(&mut self, want: &DesiredBinding) -> Result<(), String> {
        let binding = self.next_binding;
        self.next_binding += 1;
        let attach = Attach {
            binding,
            camera: CameraInfo {
                id: want.camera.id.clone(),
                name: want.camera.name.clone(),
            },
            config: want.config.clone(),
        };
        self.request(
            method::ATTACH,
            attach,
            Pending::Attach {
                camera_id: want.camera.id.clone(),
                binding,
            },
        )?;
        self.runs.insert(
            want.camera.id.clone(),
            Run {
                binding,
                camera: want.camera.clone(),
                config: want.config.clone(),
                sent_config: want.config.clone(),
                stream: want.stream,
                own_decoder: want.own_decoder,
                accepted: Accepted::default(),
                moving: want.moving,
                phase: Phase::Attaching {
                    since: Instant::now(),
                },
            },
        );
        self.context.view(&want.camera.id, |v| {
            *v = BindingView {
                binding,
                state: "attaching",
                ..Default::default()
            };
        });
        Ok(())
    }

    fn on_attached(
        &mut self,
        camera_id: &str,
        binding: u32,
        result: Result<Value, RpcError>,
    ) -> Result<(), String> {
        let Some(run) = self
            .runs
            .get_mut(camera_id)
            .filter(|run| run.binding == binding)
        else {
            return Ok(());
        };
        match result {
            Ok(value) => {
                run.accepted = params(value).unwrap_or_default();
                run.phase = Phase::Active {
                    pump: None,
                    retry_at: Instant::now(),
                };
                let (moving, reconfigure) = (run.moving, run.config != run.sent_config);
                self.context.view(camera_id, |v| {
                    v.state = "active";
                    v.detail = None;
                });
                self.start_pump(camera_id);
                if moving {
                    self.notify(method::CAMERA_HINT, CameraHint { binding, moving })?;
                }
                if reconfigure {
                    self.configure(camera_id)?;
                }
            }
            Err(error) => {
                let refusal = error
                    .data
                    .clone()
                    .and_then(|data| serde_json::from_value::<Refusal>(data).ok());
                let wait = refusal
                    .as_ref()
                    .and_then(|r| r.retry_after_s)
                    .map_or(REFUSED_RETRY, |s| {
                        Duration::from_secs(u64::from(s).clamp(5, 3600))
                    });
                let detail = clip(refusal.map_or(error.message, |r| r.message));
                tracing::warn!(plugin = %self.context.server_id, cam_id = %camera_id, %detail, "Plugin refused a camera");
                run.phase = Phase::Refused {
                    until: Instant::now() + wait,
                };
                self.context.view(camera_id, |v| {
                    v.state = "refused";
                    v.detail = Some(detail);
                });
            }
        }
        Ok(())
    }

    fn configure(&mut self, camera_id: &str) -> Result<(), String> {
        let Some(run) = self.runs.get_mut(camera_id) else {
            return Ok(());
        };
        run.sent_config = run.config.clone();
        let configure = Configure {
            binding: run.binding,
            config: run.config.clone(),
        };
        let binding = run.binding;
        self.request(
            method::CONFIGURE,
            configure,
            Pending::Configure {
                camera_id: camera_id.to_owned(),
                binding,
            },
        )
        .map(|_| ())
    }

    fn on_configured(&mut self, camera_id: &str, binding: u32, result: Result<Value, RpcError>) {
        if !self
            .runs
            .get(camera_id)
            .is_some_and(|run| run.binding == binding)
        {
            return;
        }
        match result {
            Ok(value) => {
                let accepted: Accepted = params(value).unwrap_or_default();
                self.context.view(camera_id, |v| v.detail = None);
                let Some(run) = self.runs.get_mut(camera_id) else {
                    return;
                };
                let before = run.accepted.clone();
                if accepted.stream.is_some() {
                    run.accepted.stream = accepted.stream;
                }
                if accepted.keyframes_only.is_some() {
                    run.accepted.keyframes_only = accepted.keyframes_only;
                }
                if run.accepted != before && matches!(run.phase, Phase::Active { .. }) {
                    self.start_pump(camera_id);
                }
            }
            Err(error) => {
                let detail = clip(format!(
                    "The plugin refused this configuration: {}",
                    error.message
                ));
                self.context.view(camera_id, |v| v.detail = Some(detail));
            }
        }
    }

    /// (Re)start a binding's media: the stream chosen for it, else the one
    /// the plugin asked for (its answer, else its manifest).
    fn start_pump(&mut self, camera_id: &str) {
        let Some(manifest) = self.manifest.as_ref() else {
            return;
        };
        if !manifest
            .permissions
            .iter()
            .any(|permission| permission == "video")
        {
            return;
        }
        let Some(run) = self.runs.get_mut(camera_id) else {
            return;
        };
        let stream = run
            .stream
            .or(run.accepted.stream)
            .unwrap_or(manifest.media.video.stream);
        let keyframes_only = run
            .accepted
            .keyframes_only
            .unwrap_or(manifest.media.video.keyframes_only);
        let own_decoder = run.own_decoder;
        let Phase::Active { pump, retry_at } = &mut run.phase else {
            return;
        };
        // The old subscription goes before the new one starts.
        *pump = None;
        *retry_at = Instant::now() + PUMP_RETRY;
        match Pump::start(
            &run.camera,
            MediaOptions {
                binding: run.binding,
                stream,
                keyframes_only,
                delivery: manifest.media.video.delivery,
                max_buffer_ms: manifest.media.video.max_buffer_ms,
                decoded: (self.local_decoded && !own_decoder)
                    .then(|| manifest.media.decoded.clone())
                    .flatten(),
            },
            &self.context.streams,
            self.out.clone(),
        ) {
            Ok(started) => {
                *pump = Some(started);
                self.context.view(camera_id, |v| {
                    v.state = "active";
                    v.detail = None;
                    v.stream = Some(stream);
                    v.format = None;
                    v.shared_pictures = false;
                    v.shared_fps = None;
                    v.shared_source = None;
                });
            }
            Err(problem) => {
                self.context.view(camera_id, |v| {
                    v.state = "stream_unavailable";
                    v.detail = Some(problem);
                    v.stream = Some(stream);
                });
            }
        }
    }

    /// Retry media that could not start, and copy the pump's counters into
    /// the binding's view.
    fn ensure_pump(&mut self, camera_id: &str) {
        let retry = match self.runs.get(camera_id).map(|run| &run.phase) {
            Some(Phase::Active {
                pump: None,
                retry_at,
            }) => Instant::now() >= *retry_at,
            Some(Phase::Active {
                pump: Some(pump), ..
            }) => {
                let sent = pump.counters.sent.load(Ordering::Relaxed);
                // Units the QUIC sender gave up as well (no room on a slow
                // link): queued here, never delivered.
                let binding = self.runs.get(camera_id).map(|run| run.binding);
                let lost = binding
                    .zip(self.quic.as_ref())
                    .map_or(0, |(binding, quic)| quic.lost(binding));
                let dropped = pump.counters.dropped.load(Ordering::Relaxed) + lost;
                let sent = sent.saturating_sub(lost);
                let problem = pump.counters.problem.lock().ok().and_then(|p| p.clone());
                let format = pump.counters.format.lock().ok().and_then(|f| f.clone());
                let shared = pump.counters.shared_pictures.load(Ordering::Relaxed);
                let shared_fps = shared
                    .then(|| pump.counters.shared_fps_tenths.load(Ordering::Relaxed) as f32 / 10.0);
                let shared_source = shared
                    .then(|| pump.counters.shared_source.lock().ok().and_then(|s| *s))
                    .flatten();
                self.context.view(camera_id, |v| {
                    v.shared_source = shared_source;
                    v.sent = sent;
                    v.dropped = dropped;
                    v.format = format;
                    v.shared_pictures = shared;
                    v.shared_fps = shared_fps;
                    v.state = if problem.is_some() {
                        "stream_unavailable"
                    } else {
                        "active"
                    };
                    if problem.is_some() {
                        v.detail = problem;
                    }
                });
                false
            }
            _ => false,
        };
        if retry {
            self.start_pump(camera_id);
        }
    }

    fn detach(&mut self, camera_id: &str, reason: &str) -> Result<(), String> {
        let Some(run) = self.runs.remove(camera_id) else {
            return Ok(());
        };
        let binding = run.binding;
        drop(run);
        self.end_events(Some(binding));
        self.context
            .overlays
            .clear(&self.context.server_id, Some(camera_id));
        self.context.update(|s| {
            s.bindings.remove(camera_id);
        });
        self.request(
            method::DETACH,
            Detach {
                binding,
                reason: reason.into(),
            },
            Pending::Detach,
        )
        .map(|_| ())
    }

    fn expire_requests(&mut self) {
        // Attach timeouts are handled per run; drop the rest so the map
        // stays bounded against a plugin that never answers.
        self.pending.retain(|_, (pending, since)| {
            matches!(pending, Pending::Hello) || since.elapsed() <= REQUEST_TIMEOUT
        });
    }

    fn camera_of(&self, binding: u32) -> Option<String> {
        self.runs
            .iter()
            .find(|(_, run)| run.binding == binding && matches!(run.phase, Phase::Active { .. }))
            .map(|(id, _)| id.clone())
    }

    fn event_start(&mut self, start: EventStart) {
        if !self.manifest.as_ref().is_some_and(|manifest| {
            manifest
                .permissions
                .iter()
                .any(|permission| permission == "events")
        }) {
            return;
        }
        let Some(camera_id) = self.camera_of(start.binding) else {
            return;
        };
        let frame = match start.seq {
            Some(seq) => match self.frame(start.binding, seq) {
                Some(frame) => Some(frame),
                None => return,
            },
            None => None,
        };
        let Some(manifest) = self.manifest.as_ref() else {
            return;
        };
        let Some((kind, detail)) = plugin_event_kind(&manifest.event_kinds, &start.kind) else {
            let plugin = self.context.server_id.clone();
            let kind = start.kind.clone();
            self.warn_once(format!("kind:{kind}"), || {
                tracing::warn!(plugin = %plugin, %kind, "Plugin event of an undeclared or unknown kind ignored");
            });
            return;
        };
        if self.open.contains_key(&start.event)
            || self
                .open
                .values()
                .filter(|o| o.binding == start.binding)
                .count()
                >= MAX_OPEN_EVENTS
        {
            return;
        }
        let now = Instant::now();
        let (window, count) = self.starts.entry(start.binding).or_insert((now, 0));
        if window.elapsed() >= Duration::from_secs(1) {
            *window = now;
            *count = 0;
        }
        if *count >= EVENT_STARTS_PER_SECOND {
            return;
        }
        *count += 1;
        let mut event = NewEvent::new(Some(camera_id), kind, EventSource::Plugin);
        event.topic = Some(manifest.id.clone());
        event.detail = detail;
        event.score = start.score.and_then(unit_interval);
        event.detection = start.alert == Some(false);
        let record = match frame {
            Some(frame) => self.context.events.start_at(event, frame.time_us / 1000),
            None => self.context.events.start(event),
        };
        self.open.insert(
            start.event,
            OpenEvent {
                id: record.id,
                binding: start.binding,
            },
        );
    }

    fn event_update(&mut self, update: EventUpdate) {
        let Some(open) = self
            .open
            .get(&update.event)
            .filter(|open| open.binding == update.binding)
        else {
            return;
        };
        let score = update.score.and_then(unit_interval);
        let bbox = update.bbox.filter(|b| {
            b.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
                && b[0] + b[2] <= 1.0 + 1e-3
                && b[1] + b[3] <= 1.0 + 1e-3
        });
        let zones: Vec<String> = update
            .zones
            .into_iter()
            .filter(|zone| !zone.is_empty() && zone.len() <= 64)
            .take(MAX_ZONES)
            .collect();
        let upgrade = update.alert == Some(true);
        self.context.events.update(open.id, |record| {
            if upgrade {
                record.detection = false;
            }
            if score.is_some() {
                record.score = score;
            }
            if bbox.is_some() && plugin_box_applies(record) {
                record.bbox = bbox;
            }
            if !zones.is_empty() {
                record.zones = zones;
            }
        });
    }

    fn event_end(&mut self, end: &EventEnd) {
        if self
            .open
            .get(&end.event)
            .is_some_and(|open| open.binding == end.binding)
        {
            if let Some(open) = self.open.remove(&end.event) {
                self.context.events.end(open.id);
            }
        }
    }

    fn end_events(&mut self, binding: Option<u32>) {
        let ending: Vec<u64> = self
            .open
            .iter()
            .filter(|(_, open)| binding.is_none_or(|b| open.binding == b))
            .map(|(event, _)| *event)
            .collect();
        for event in ending {
            if let Some(open) = self.open.remove(&event) {
                self.context.events.end(open.id);
            }
        }
    }

    fn on_overlay(&mut self, overlay: Overlay) {
        if !self.manifest.as_ref().is_some_and(|manifest| {
            manifest
                .permissions
                .iter()
                .any(|permission| permission == "overlays")
        }) {
            return;
        }
        let Some(camera_id) = self.camera_of(overlay.binding) else {
            return;
        };
        let frame = match overlay.seq {
            Some(seq) => match self.frame(overlay.binding, seq) {
                Some(frame) => Some(frame),
                None => return,
            },
            None => None,
        };
        let declared = self
            .manifest
            .as_ref()
            .is_some_and(|m| m.overlay_layers.iter().any(|l| l.id == overlay.layer));
        if !declared {
            let plugin = self.context.server_id.clone();
            let layer = overlay.layer.clone();
            self.warn_once(format!("layer:{layer}"), || {
                tracing::warn!(plugin = %plugin, %layer, "Plugin drawing on an undeclared layer ignored");
            });
            return;
        }
        // At most OVERLAYS_PER_SECOND per binding and layer: a flood is dropped.
        let now = Instant::now();
        let (window, count) = self
            .drawn
            .entry((overlay.binding, overlay.layer.clone()))
            .or_insert((now, 0));
        let is_result = !overlay.items.is_empty() || frame.is_some();
        if !within_overlay_rate(window, count, now, is_result) {
            return;
        }
        self.context.overlays.put_identified(
            &camera_id,
            &self.context.server_id,
            &overlay.layer,
            overlays::sanitize(overlay.items),
            frame,
            overlay.ttl_ms,
        );
    }

    fn frame(&self, binding: u32, seq: u64) -> Option<crate::streams::frames::Identity> {
        let run = self.runs.values().find(|run| run.binding == binding)?;
        let Phase::Active {
            pump: Some(pump), ..
        } = &run.phase
        else {
            return None;
        };
        pump.counters.frames.lock().ok()?.get(seq)
    }

    fn on_status(&mut self, status: Status) {
        for mut reported in status.bindings {
            let Some(camera_id) = self.camera_of(reported.binding) else {
                continue;
            };
            reported.detail = reported.detail.map(clip);
            reported.decoder = reported.decoder.map(clip);
            reported.figures = clip_figures(reported.figures);
            self.context.overlays.put_figures(
                &camera_id,
                &self.context.server_id,
                reported.figures.clone(),
            );
            self.context.view(&camera_id, |v| v.plugin = Some(reported));
        }
    }

    fn warn_once(&mut self, key: String, log: impl FnOnce()) {
        if self.warned.len() < 64 && self.warned.insert(key) {
            log();
        }
    }

    /// End everything this session started: media, then events.
    fn close(&mut self) {
        self.runs.clear();
        self.end_events(None);
        self.context.overlays.clear(&self.context.server_id, None);
        self.context.update(|s| s.bindings.clear());
    }
}

/// The event a plugin's `event.start` kind makes, and its detail. Only kinds
/// the manifest declares count. A detection plugin names its own classes,
/// whatever they are: every class is an [`EventKind::Object`] titled with the
/// plugin's label for it ("Person", "Forklift"), never matched against
/// NeuroWatch's own kinds. The one exception is `motion` (the Motion plugin).
/// A plugin never makes a system event (camera offline, disk low).
fn plugin_event_kind(
    declared: &[messages::EventKindInfo],
    kind: &str,
) -> Option<(EventKind, Option<String>)> {
    let info = declared.iter().find(|info| info.id == kind)?;
    if kind.is_empty() || kind.len() > MAX_KIND_ID {
        return None;
    }
    if kind == EventKind::Motion.as_str() {
        return Some((EventKind::Motion, None));
    }
    let label = info.label.trim();
    let label = if label.is_empty() { kind } else { label };
    Some((
        EventKind::Object,
        Some(clip_to(label.to_owned(), MAX_KIND_LABEL)),
    ))
}

/// Whether a plugin's box may replace an event's: only until the event has
/// its picture. The recorder then sets the box drawn on that very picture
/// (`snapshot.rs`, `plugin_host::event_box`); the plugin's own box comes from
/// another moment, so a later update must not move it off the object.
fn plugin_box_applies(record: &nw_core::event::EventRecord) -> bool {
    record.thumbnail.is_none()
}

#[cfg(test)]
mod event_box_rule_tests {
    use super::*;

    #[test]
    fn a_plugin_box_applies_only_until_the_event_has_its_picture() {
        let mut record: nw_core::event::EventRecord = serde_json::from_value(serde_json::json!({
            "id": 1, "camera_id": "c", "kind": "motion", "source": "plugin",
            "start_ms": 0, "end_ms": null
        }))
        .unwrap();
        assert!(plugin_box_applies(&record));
        record.thumbnail = Some("1.jpg".into());
        assert!(
            !plugin_box_applies(&record),
            "the box matched to the picture stays"
        );
    }
}

fn unit_interval(value: f32) -> Option<f32> {
    value.is_finite().then(|| value.clamp(0.0, 1.0))
}

fn clip(text: String) -> String {
    clip_to(text, MAX_TEXT)
}

fn clip_to(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// A plugin's figures as viewers get them: at most `MAX_FIGURES`, each
/// label and value one line of at most `MAX_FIGURE_TEXT` characters.
fn clip_figures(figures: Vec<messages::Figure>) -> Vec<messages::Figure> {
    let short = |text: String| -> String {
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(messages::MAX_FIGURE_TEXT)
            .collect()
    };
    figures
        .into_iter()
        .take(messages::MAX_FIGURES)
        .map(|figure| messages::Figure {
            label: short(figure.label),
            value: short(figure.value),
            icon: figure.icon.map(short),
            style: figure.style,
        })
        .collect()
}

/// What decides a binding's camera connection; a change restarts its media.
fn stream_fingerprint(camera: &CameraConfig) -> String {
    serde_json::to_string(&(
        &camera.url,
        &camera.sub_url,
        &camera.username,
        &camera.password_b64,
        &camera.transport,
    ))
    .unwrap_or_default()
}

// ---- Outbox and the socket ----------------------------------------------------

/// Inspect a signed hello only. No locality capability, node authorization,
/// licence credential, camera attachment or media is sent by discovery.
pub(super) fn inspect(address: &Address) -> Result<Manifest, String> {
    let conn = Conn::connect(address).map_err(|error| error.to_string())?;
    let shutdown = conn.try_clone().map_err(|error| error.to_string())?;
    let (done, wait) = mpsc::channel();
    let guard = std::thread::spawn(move || {
        let _ = wait.recv_timeout(Duration::from_secs(1));
        shutdown.shutdown();
    });
    let mut io = conn;
    let result = (|| {
        let challenge =
            nw_analytics_sdk::identity::challenge().map_err(|error| error.to_string())?;
        write_frame(
            &mut io,
            &Frame::Message(
                Message::request(
                    1,
                    method::HELLO,
                    Hello {
                        protocol: PROTOCOL_VERSION,
                        node: NodeInfo {
                            name: "NeuroWatch plugin discovery".into(),
                            version: env!("CARGO_PKG_VERSION").into(),
                        },
                        locality: None,
                        challenge: challenge.clone(),
                    },
                )
                .to_value(),
            ),
        )
        .map_err(|error| error.to_string())?;
        let Frame::Message(value) = read_frame(&mut io).map_err(|error| error.to_string())? else {
            return Err("Expected the plugin's manifest".into());
        };
        let Message::Response { id: 1, result } =
            Message::from_value(value).map_err(|error| error.to_string())?
        else {
            return Err("Expected the plugin's hello response".into());
        };
        let manifest: Manifest =
            params(result.map_err(|error| error.message)?).map_err(|error| error.message)?;
        if manifest.protocol != PROTOCOL_VERSION {
            return Err("Unsupported plugin protocol".into());
        }
        if manifest.identity.is_none() {
            return Err(
                "Update and restart this plugin: it does not support session authentication".into(),
            );
        }
        let (channel, peer) = match &io {
            Conn::Quic(io) => (io.channel_binding(), io.peer_key()),
            Conn::Tls(io) => (io.channel_binding(), io.peer_key()),
            _ => ("", None),
        };
        if !nw_analytics_sdk::identity::verify_manifest(&manifest, &challenge, channel)
            || peer.is_some_and(|key| {
                manifest
                    .identity
                    .as_ref()
                    .is_none_or(|identity| identity.public_key != key)
            })
        {
            return Err("The plugin did not prove its session identity".into());
        }
        Ok(manifest)
    })();
    let _ = done.send(());
    let _ = guard.join();
    result
}

/// Frames to the plugin, written in order by the writer thread. Messages
/// wait for room (the writer times out on a stuck socket); media is dropped
/// when the plugin is behind.
#[derive(Clone)]
pub(super) struct Outbox {
    tx: mpsc::SyncSender<Outgoing>,
    queued: Arc<AtomicUsize>,
}

enum Outgoing {
    Frame(Frame),
    Descriptor(Frame, File),
}

pub(super) enum Offer {
    Queued,
    Full,
    Closed,
}

impl Outbox {
    /// `false`: the connection is gone.
    pub fn message(&self, message: &Message) -> bool {
        self.tx
            .send(Outgoing::Frame(Frame::Message(message.to_value())))
            .is_ok()
    }

    pub fn descriptor(&self, message: &Message, file: File) -> bool {
        self.tx
            .send(Outgoing::Descriptor(
                Frame::Message(message.to_value()),
                file,
            ))
            .is_ok()
    }

    pub fn media(&self, unit: MediaUnit) -> Offer {
        let size = unit.data.len();
        if self.queued.load(Ordering::Relaxed) + size > OUTBOX_BYTES {
            return Offer::Full;
        }
        self.queued.fetch_add(size, Ordering::Relaxed);
        match self.tx.try_send(Outgoing::Frame(Frame::Media(unit))) {
            Ok(()) => Offer::Queued,
            Err(error) => {
                self.queued.fetch_sub(size, Ordering::Relaxed);
                match error {
                    mpsc::TrySendError::Full(_) => Offer::Full,
                    mpsc::TrySendError::Disconnected(_) => Offer::Closed,
                }
            }
        }
    }
}

fn read_loop(conn: Conn, incoming: mpsc::SyncSender<Incoming>) {
    let mut reader = BufReader::new(conn);
    loop {
        let closed = match read_frame(&mut reader) {
            Ok(Frame::Message(value)) => match Message::from_value(value) {
                Ok(message) => {
                    if incoming.send(Incoming::Message(message)).is_err() {
                        return;
                    }
                    continue;
                }
                Err(_) => "The plugin sent an invalid message".to_owned(),
            },
            Ok(Frame::Media(_)) => "The plugin sent media, which it may not".to_owned(),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                "The plugin closed the connection".to_owned()
            }
            Err(error) => format!("Connection error: {error}"),
        };
        let _ = incoming.send(Incoming::Closed(closed));
        return;
    }
}

fn write_loop(conn: Conn, frames: mpsc::Receiver<Outgoing>, queued: &AtomicUsize) {
    let mut writer = BufWriter::new(conn);
    while let Ok(mut frame) = frames.recv() {
        loop {
            let size = match &frame {
                Outgoing::Frame(Frame::Media(unit)) => unit.data.len(),
                _ => 0,
            };
            let written = match &frame {
                Outgoing::Frame(Frame::Media(unit))
                    if matches!(writer.get_ref(), Conn::Quic(_)) =>
                {
                    writer.flush().and_then(|_| match writer.get_ref() {
                        Conn::Quic(io) => io.media(unit),
                        _ => Ok(()),
                    })
                }
                Outgoing::Frame(frame) if matches!(writer.get_ref(), Conn::Quic(_)) => {
                    if let Conn::Quic(io) = writer.get_ref() {
                        io.observe(frame);
                    }
                    write_frame(&mut writer, frame)
                }
                Outgoing::Frame(frame) => write_frame(&mut writer, frame),
                Outgoing::Descriptor(frame, file) => writer
                    .flush()
                    .and_then(|_| writer.get_ref().write_descriptor(frame, file)),
            };
            queued.fetch_sub(size, Ordering::Relaxed);
            if written.is_err() {
                return;
            }
            match frames.try_recv() {
                Ok(next) => frame = next,
                Err(_) => break,
            }
        }
        if writer.flush().is_err() {
            return;
        }
    }
}

/// A connected socket of either kind.
enum Conn {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream),
    Quic(nw_analytics_sdk::quic::QuicIo),
    Tls(nw_analytics_sdk::tls::TlsIo),
    #[cfg(windows)]
    Pipe(nw_analytics_sdk::pipe::Pipe),
}

impl Conn {
    fn write_descriptor(&self, frame: &Frame, file: &File) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(socket) => nw_analytics_sdk::local::write_descriptor(socket, frame, file),
            #[cfg(windows)]
            Self::Pipe(pipe) => {
                use std::os::windows::io::AsRawHandle;
                let Frame::Message(message) = frame else {
                    return Err(io::Error::other("capabilities require a control message"));
                };
                let mut message = message.clone();
                let params = message
                    .get_mut("params")
                    .ok_or_else(|| io::Error::other("missing capability parameters"))?;
                let offer = if params.get("locality").is_some() {
                    &mut params["locality"]
                } else {
                    &mut params["ring"]
                };
                offer["handle"] = serde_json::json!(file.as_raw_handle() as usize as u64);
                write_frame(&mut pipe.clone(), &Frame::Message(message))
            }
            _ => {
                let _ = (frame, file);
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this transport cannot pass local descriptors",
                ))
            }
        }
    }
    /// As `connect`, with a short QUIC handshake: another transport follows.
    fn connect_before_fallback(address: &Address) -> io::Result<Self> {
        match address {
            Address::Quic { host, port } => {
                nw_analytics_sdk::quic::QuicIo::connect_within(host, *port, QUIC_BEFORE_FALLBACK)
                    .map(Self::Quic)
            }
            other => Self::connect(other),
        }
    }

    fn connect(address: &Address) -> io::Result<Self> {
        match address {
            Address::Tls { host, port } => {
                nw_analytics_sdk::tls::TlsIo::connect(host, *port).map(Self::Tls)
            }
            Address::Quic { host, port } => {
                nw_analytics_sdk::quic::QuicIo::connect(host, *port).map(Self::Quic)
            }
            #[cfg(unix)]
            Address::Unix(path) => {
                let socket =
                    socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
                socket.connect_timeout(&socket2::SockAddr::unix(path)?, CONNECT_TIMEOUT)?;
                Ok(Self::Unix(socket.into()))
            }
            #[cfg(not(unix))]
            Address::Unix(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix sockets are not available here; use tcp:127.0.0.1:<port>",
            )),
            Address::Tcp(socket) => {
                let stream = std::net::TcpStream::connect_timeout(socket, CONNECT_TIMEOUT)?;
                stream.set_nodelay(true)?;
                Ok(Self::Tcp(stream))
            }
            #[cfg(windows)]
            Address::Pipe(name) => nw_analytics_sdk::pipe::Pipe::connect(name).map(Self::Pipe),
            #[cfg(not(windows))]
            Address::Pipe(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows named pipes are unavailable here",
            )),
        }
    }

    fn try_clone(&self) -> io::Result<Self> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
            Self::Quic(io) => Ok(Self::Quic(io.clone())),
            Self::Tls(io) => Ok(Self::Tls(io.clone())),
            #[cfg(windows)]
            Self::Pipe(pipe) => Ok(Self::Pipe(pipe.clone())),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
            Self::Quic(_) | Self::Tls(_) => Ok(()),
            #[cfg(windows)]
            Self::Pipe(_) => Ok(()),
        }
    }

    fn shutdown(&self) {
        let _ = match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.shutdown(std::net::Shutdown::Both),
            Self::Tcp(stream) => stream.shutdown(std::net::Shutdown::Both),
            Self::Tls(io) => {
                io.shutdown();
                Ok(())
            }
            Self::Quic(io) => {
                io.shutdown();
                Ok(())
            }
            #[cfg(windows)]
            Self::Pipe(pipe) => {
                pipe.shutdown();
                Ok(())
            }
        };
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
            Self::Tcp(stream) => stream.read(buf),
            Self::Quic(io) => io.read(buf),
            Self::Tls(io) => io.read(buf),
            #[cfg(windows)]
            Self::Pipe(pipe) => pipe.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
            Self::Tcp(stream) => stream.write(buf),
            Self::Quic(io) => io.write(buf),
            Self::Tls(io) => io.write(buf),
            #[cfg(windows)]
            Self::Pipe(pipe) => pipe.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
            Self::Quic(io) => io.flush(),
            Self::Tls(io) => io.flush(),
            #[cfg(windows)]
            Self::Pipe(pipe) => pipe.flush(),
        }
    }
}

/// Counts one drawing in the layer's one-second window; `false` when it is
/// over [`OVERLAYS_PER_SECOND`] and must be dropped. A clear (an empty overlay
/// without a picture) always passes; results, empty ones too, are limited.
fn within_overlay_rate(
    window: &mut Instant,
    count: &mut u32,
    now: Instant,
    is_result: bool,
) -> bool {
    if now.saturating_duration_since(*window) >= Duration::from_secs(1) {
        *window = now;
        *count = 0;
    }
    if *count >= OVERLAYS_PER_SECOND && is_result {
        return false;
    }
    *count += 1;
    true
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    fn kinds(pairs: &[(&str, &str)]) -> Vec<messages::EventKindInfo> {
        pairs
            .iter()
            .map(|(id, label)| messages::EventKindInfo {
                id: (*id).into(),
                label: (*label).into(),
            })
            .collect()
    }

    #[test]
    fn a_plugins_classes_are_named_objects_and_motion_is_motion() {
        let declared = kinds(&[
            ("person", "Person"),
            ("forklift", "Forklift"),
            ("traffic_light", " "),
            ("disk_low", "Disk low"),
            ("motion", "Motion"),
        ]);
        // Class names are the plugin's own: "person" is not NeuroWatch's Person.
        assert_eq!(
            plugin_event_kind(&declared, "person"),
            Some((EventKind::Object, Some("Person".into())))
        );
        assert_eq!(
            plugin_event_kind(&declared, "motion"),
            Some((EventKind::Motion, None))
        );
        assert_eq!(
            plugin_event_kind(&declared, "forklift"),
            Some((EventKind::Object, Some("Forklift".into())))
        );
        // No label: the class's own name.
        assert_eq!(
            plugin_event_kind(&declared, "traffic_light"),
            Some((EventKind::Object, Some("traffic_light".into())))
        );
        // A plugin never raises a system event, whatever it names a class.
        assert_eq!(
            plugin_event_kind(&declared, "disk_low"),
            Some((EventKind::Object, Some("Disk low".into())))
        );
        assert_eq!(plugin_event_kind(&declared, "dragon"), None, "undeclared");
        let long = "x".repeat(MAX_KIND_ID + 1);
        assert_eq!(plugin_event_kind(&kinds(&[(&long, "Long")]), &long), None);
    }
}

#[cfg(test)]
mod overlay_rate_tests {
    use super::*;

    #[test]
    fn results_are_rate_limited_but_a_clear_always_passes() {
        let start = Instant::now();
        let (mut window, mut count) = (start, 0);
        // Per-picture results, empty ones too: 25 pass, then the flood is dropped.
        for _ in 0..OVERLAYS_PER_SECOND {
            assert!(within_overlay_rate(&mut window, &mut count, start, true));
        }
        assert!(!within_overlay_rate(&mut window, &mut count, start, true));
        // A clear (no picture, no items) is never dropped, or a layer could stay up.
        assert!(within_overlay_rate(&mut window, &mut count, start, false));
        // The next second starts afresh.
        let later = start + Duration::from_millis(1100);
        assert!(within_overlay_rate(&mut window, &mut count, later, true));
        assert_eq!(count, 1);
    }
}
