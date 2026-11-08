//! QUIC transport: reliable control plus independently cancellable GOP streams.
//! An unapproved connection may inspect a signed manifest only. The recorder
//! compares this certificate's Ed25519 key with that signed identity before it
//! authorizes media or supplies a vendor credential.
use crate::identity::{self, SigningKey};
use nw_analytics_protocol::{
    frame::{self, Frame, MediaUnit},
    messages::{DeliveryMode, Gap, GapReason, StreamFormat},
    method,
    rpc::{params, Message},
};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Read, Write},
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    runtime::Runtime,
    sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore},
};
const ALPN: &[u8] = b"nw-analytics/1";
const BUDGET: usize = 32 * 1024 * 1024;
const LANES: usize = 1024;
const HEADER: &str = "media.gop";
fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
fn runtime() -> io::Result<Arc<Runtime>> {
    Ok(Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
    ))
}

#[derive(Debug)]
pub(crate) struct IdentityVerifier(pub(crate) Arc<rustls::crypto::CryptoProvider>);
impl ServerCertVerifier for IdentityVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        certificate_key(cert)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        sig: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            msg,
            cert,
            sig,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        sig: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            msg,
            cert,
            sig,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
pub(crate) fn certificate_key(cert: &CertificateDer<'_>) -> Result<String, rustls::Error> {
    let parsed = rustls::server::ParsedCertificate::try_from(cert)?;
    let spki = parsed.subject_public_key_info();
    let bytes = spki.as_ref();
    const PREFIX: &[u8] = &[
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    if bytes.len() != 44 || !bytes.starts_with(PREFIX) {
        return Err(rustls::Error::General(
            "analytics identity must be Ed25519".into(),
        ));
    }
    Ok(identity::hex(&bytes[12..]))
}
fn transport() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config
        .max_concurrent_uni_streams(16u32.into())
        .max_concurrent_bidi_streams(1u32.into())
        .stream_receive_window((2u32 * 1024 * 1024).into())
        .receive_window((8u32 * 1024 * 1024).into())
        .max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap_or_default()));
    config
}
pub(crate) fn server_tls(key: &SigningKey) -> io::Result<rustls::ServerConfig> {
    let mut pkcs8 = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    pkcs8.extend_from_slice(&key.to_bytes());
    let private = PrivatePkcs8KeyDer::from(pkcs8);
    let pair =
        rcgen::KeyPair::from_der_and_sign_algo(&private.clone_key().into(), &rcgen::PKCS_ED25519)
            .map_err(error)?;
    let cert = rcgen::CertificateParams::new(vec!["analytics.local".into()])
        .map_err(error)?
        .self_signed(&pair)
        .map_err(error)?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(error)?
    .with_no_client_auth()
    .with_single_cert(vec![cert.der().clone()], private.into())
    .map_err(error)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(tls)
}
fn server_config(key: &SigningKey) -> io::Result<quinn::ServerConfig> {
    let tls = server_tls(key)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(error)?,
    ));
    config.transport_config(Arc::new(transport()));
    Ok(config)
}

#[derive(Serialize, Deserialize)]
struct Gop {
    binding: u32,
    number: u64,
    format: StreamFormat,
    delivery: DeliveryMode,
    max_buffer_ms: u32,
    gap: Option<GapReason>,
}
struct Chunk {
    enqueued: std::time::Instant,
    bytes: Vec<u8>,
    _permit: Option<OwnedSemaphorePermit>,
}
struct Inbound {
    frame: Frame,
    permit: OwnedSemaphorePermit,
}
async fn read_frame(
    stream: &mut RecvStream,
    budget: &Arc<Semaphore>,
) -> io::Result<Option<Inbound>> {
    let mut header = [0; 5];
    match stream.read_exact(&mut header[..4]).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(error(e)),
    }
    let len = u32::from_le_bytes(header[..4].try_into().map_err(error)?) as usize;
    stream.read_exact(&mut header[4..]).await.map_err(error)?;
    let limit = match header[4] {
        1 => frame::MAX_MESSAGE_BYTES + 1,
        2 => frame::MAX_MEDIA_BYTES + 41,
        _ => 0,
    };
    if len == 0 || len > limit {
        return Err(error("invalid analytics frame length"));
    }
    let permit = Arc::clone(budget)
        .acquire_many_owned((len + 4) as u32)
        .await
        .map_err(error)?;
    let mut bytes = vec![0; len + 4];
    bytes[..5].copy_from_slice(&header);
    stream.read_exact(&mut bytes[5..]).await.map_err(error)?;
    let frame = frame::read_frame(&mut bytes.as_slice())?;
    Ok(Some(Inbound { frame, permit }))
}
async fn deliver(inbound: Inbound, out: &mpsc::Sender<io::Result<Chunk>>) -> bool {
    let mut bytes = Vec::new();
    if frame::write_frame(&mut bytes, &inbound.frame).is_err() {
        return false;
    }
    out.send(Ok(Chunk {
        enqueued: std::time::Instant::now(),
        bytes,
        _permit: Some(inbound.permit),
    }))
    .await
    .is_ok()
}
async fn notify(message: Message, out: &mpsc::Sender<io::Result<Chunk>>) -> bool {
    let mut bytes = Vec::new();
    if frame::write_frame(&mut bytes, &Frame::Message(message.to_value())).is_err() {
        return false;
    }
    out.send(Ok(Chunk {
        enqueued: std::time::Instant::now(),
        bytes,
        _permit: None,
    }))
    .await
    .is_ok()
}
struct ReceivedGop {
    finished: Arc<std::sync::atomic::AtomicBool>,
    header: Gop,
    units: mpsc::Receiver<Inbound>,
}
async fn binding_reader(
    mut incoming: mpsc::Receiver<ReceivedGop>,
    out: mpsc::Sender<io::Result<Chunk>>,
    connection: Connection,
) {
    let mut pending = BTreeMap::new();
    let mut active: Option<ReceivedGop> = None;
    let mut expected = 1;
    let mut last_seq = None;
    let mut gap = false;
    loop {
        if active.is_none() {
            if let Some(next) = pending.remove(&expected) {
                active = Some(next);
            } else {
                tokio::select! {
                    _=connection.closed()=>return,
                    next=incoming.recv()=> {let Some(next)=next else{return;}; if next.header.number<expected{continue;} pending.insert(next.header.number,next);}
                    _=tokio::time::sleep(Duration::from_secs(1)), if !pending.is_empty()=>{gap=true; if let Some((&first,_))=pending.first_key_value(){expected=first;}}
                }
                if pending.len() > 8 {
                    pending.pop_first();
                    gap = true;
                }
                continue;
            }
        }
        let Some(current) = active.as_mut() else {
            continue;
        };
        let is_latest = current.header.delivery == DeliveryMode::Latest;
        tokio::select! {
            _=connection.closed()=>return,
            next=incoming.recv()=>{
                let Some(next)=next else{return;};
                if next.header.number<expected{continue;}
                if is_latest && next.header.number>current.header.number {
                    gap=true;expected=next.header.number;pending.clear();active=Some(next);
                } else {pending.insert(next.header.number,next); if pending.len()>8{pending.pop_first();gap=true;}}
            }
            frame=current.units.recv()=>{
                let Some(mut frame)=frame else{gap |= !current.finished.load(std::sync::atomic::Ordering::Acquire);expected=current.header.number.saturating_add(1); active=None;continue;};
                let Frame::Media(unit)=&mut frame.frame else{continue;};
                if unit.binding!=current.header.binding || last_seq.is_some_and(|last|unit.seq<=last){continue;}
                if last_seq.is_none() || unit.keyframe() {
                    if gap || current.header.gap.is_some() {unit.flags|=frame::flags::DISCONTINUITY; if !notify(Message::notification(method::GAP,Gap{binding:unit.binding,reason:current.header.gap.take().unwrap_or(GapReason::Behind),from_seq:last_seq,to_seq:Some(unit.seq)}),&out).await{return;}}
                    gap=false;
                    if !notify(Message::notification(method::STREAM_FORMAT,&current.header.format),&out).await{return;}
                }
                last_seq=Some(unit.seq);
                if !deliver(frame,&out).await{return;}
            }
        }
    }
}
#[derive(Default)]
struct Receivers {
    lanes: HashMap<u32, mpsc::Sender<ReceivedGop>>,
    allowed: HashMap<u32, Option<std::time::Instant>>,
}
impl Receivers {
    fn prune(&mut self) {
        self.allowed
            .retain(|_, expiry| expiry.map_or(true, |expiry| expiry > std::time::Instant::now()));
        let allowed = &self.allowed;
        self.lanes
            .retain(|binding, _| allowed.contains_key(binding));
    }
    fn control(&mut self, value: &serde_json::Value) -> bool {
        self.prune();
        let Some(binding) = value["params"]["binding"]
            .as_u64()
            .and_then(|binding| u32::try_from(binding).ok())
        else {
            return true;
        };
        match value["method"].as_str() {
            Some(method::ATTACH) => {
                self.allowed.insert(
                    binding,
                    Some(std::time::Instant::now() + Duration::from_secs(15)),
                );
            }
            Some(method::STREAM_FORMAT) => {
                self.allowed.insert(binding, None);
            }
            Some(method::DETACH) => {
                self.allowed.remove(&binding);
                self.lanes.remove(&binding);
            }
            _ => {}
        }
        self.allowed.len() <= LANES
    }
}
async fn receive_gops(
    connection: Connection,
    budget: Arc<Semaphore>,
    out: mpsc::Sender<io::Result<Chunk>>,
    receivers: Arc<Mutex<Receivers>>,
) {
    let slots = Arc::new(Semaphore::new(16));
    loop {
        let mut stream = match connection.accept_uni().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let permit = match Arc::clone(&slots).acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };
        let first =
            match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream, &budget))
                .await
            {
                Ok(Ok(Some(frame))) => frame,
                _ => continue,
            };
        let Frame::Message(value) = first.frame else {
            continue;
        };
        let Ok(Message::Notification {
            method,
            params: values,
        }) = Message::from_value(value)
        else {
            continue;
        };
        if method != HEADER {
            continue;
        }
        let Ok(header) = params::<Gop>(values) else {
            continue;
        };
        drop(first.permit);
        if header.format.binding != header.binding || header.number == 0 {
            continue;
        }
        let lane = {
            let Ok(mut receivers) = receivers.lock() else {
                return;
            };
            receivers.prune();
            let Some(allowed) = receivers.allowed.get_mut(&header.binding) else {
                let _ = stream.stop(1u32.into());
                continue;
            };
            *allowed = None;
            let lane = receivers.lanes.entry(header.binding).or_insert_with(|| {
                let (tx, rx) = mpsc::channel(8);
                tokio::spawn(binding_reader(rx, out.clone(), connection.clone()));
                tx
            });
            lane.clone()
        };
        let (tx, rx) = mpsc::channel(8);
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if lane
            .try_send(ReceivedGop {
                header,
                units: rx,
                finished: Arc::clone(&finished),
            })
            .is_err()
        {
            let _ = stream.stop(1u32.into());
            continue;
        }
        let budget = Arc::clone(&budget);
        let connection = connection.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut first = true;
            loop {
                let frame = tokio::select! {_ = connection.closed()=>return,result=read_frame(&mut stream,&budget)=>result};
                let frame = match frame {
                    Ok(Some(frame)) => frame,
                    Ok(None) => {
                        finished.store(true, std::sync::atomic::Ordering::Release);
                        return;
                    }
                    Err(_) => return,
                };
                if !matches!(&frame.frame,Frame::Media(unit) if !first || unit.keyframe()) {
                    return;
                }
                first = false;
                if tx.send(frame).await.is_err() {
                    let _ = stream.stop(1u32.into());
                    return;
                }
            }
        });
    }
}
struct Lane {
    number: u64,
    newest: watch::Sender<u64>,
    units: Option<mpsc::Sender<Chunk>>,
    waiting: bool,
    gap: bool,
}
struct Output {
    formats: HashMap<u32, StreamFormat>,
    gaps: HashMap<u32, GapReason>,
    lanes: HashMap<u32, Lane>,
    delivery: DeliveryMode,
    max_buffer_ms: u32,
}
struct Shared {
    runtime: Arc<Runtime>,
    _endpoint: Endpoint,
    connection: Connection,
    send: tokio::sync::Mutex<SendStream>,
    incoming: tokio::sync::Mutex<mpsc::Receiver<io::Result<Chunk>>>,
    output: Mutex<Output>,
    budget: Arc<Semaphore>,
    peer_key: Option<String>,
    binding: String,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"session closed");
    }
}
#[derive(Clone)]
pub struct QuicIo {
    shared: Arc<Shared>,
    chunk: Arc<Mutex<Option<(Chunk, usize)>>>,
}
impl QuicIo {
    fn new(
        runtime: Arc<Runtime>,
        endpoint: Endpoint,
        connection: Connection,
        send: SendStream,
        mut recv: RecvStream,
        plugin: bool,
    ) -> io::Result<Self> {
        let mut binding = [0; 32];
        connection
            .export_keying_material(&mut binding, b"NeuroWatch analytics session v1", b"")
            .map_err(|_| error("TLS exporter unavailable"))?;
        let peer_key = if plugin {
            None
        } else {
            let certificates = connection
                .peer_identity()
                .ok_or_else(|| error("missing plugin certificate"))?
                .downcast::<Vec<CertificateDer<'static>>>()
                .map_err(|_| error("invalid certificate identity"))?;
            Some(
                certificate_key(
                    certificates
                        .first()
                        .ok_or_else(|| error("empty certificate chain"))?,
                )
                .map_err(error)?,
            )
        };
        let (out, incoming) = mpsc::channel(64);
        let budget = Arc::new(Semaphore::new(BUDGET));
        let task_out = out.clone();
        let task_budget = Arc::clone(&budget);
        let receivers = Arc::new(Mutex::new(Receivers::default()));
        let control_receivers = Arc::clone(&receivers);
        runtime.spawn(async move {
            loop {
                match read_frame(&mut recv, &task_budget).await {
                    Ok(Some(frame)) => {
                        if matches!(frame.frame, Frame::Media(_)) {
                            let _ = task_out.send(Err(error("media on control stream"))).await;
                            return;
                        }
                        if plugin {
                            if let Frame::Message(value) = &frame.frame {
                                if !control_receivers
                                    .lock()
                                    .map(|mut receivers| receivers.control(value))
                                    .unwrap_or(false)
                                {
                                    let _ =
                                        task_out.send(Err(error("too many media bindings"))).await;
                                    return;
                                }
                                if value["method"] == method::GAP
                                    || value["method"] == method::STREAM_FORMAT
                                {
                                    continue;
                                }
                            }
                        }
                        if !deliver(frame, &task_out).await {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        let _ = task_out.send(Err(e)).await;
                        return;
                    }
                }
            }
        });
        if plugin {
            runtime.spawn(receive_gops(connection.clone(), budget, out, receivers));
        } else {
            let connection = connection.clone();
            runtime.spawn(async move {
                if connection.accept_uni().await.is_ok() {
                    connection.close(1u32.into(), b"plugins cannot send media");
                }
            });
        }
        Ok(Self {
            shared: Arc::new(Shared {
                runtime,
                _endpoint: endpoint,
                connection,
                send: tokio::sync::Mutex::new(send),
                incoming: tokio::sync::Mutex::new(incoming),
                output: Mutex::new(Output {
                    formats: HashMap::new(),
                    gaps: HashMap::new(),
                    lanes: HashMap::new(),
                    delivery: DeliveryMode::Latest,
                    max_buffer_ms: 1000,
                }),
                budget: Arc::new(Semaphore::new(BUDGET)),
                peer_key,
                binding: identity::hex(&binding),
            }),
            chunk: Arc::new(Mutex::new(None)),
        })
    }
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let remote = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| error("plugin hostname has no address"))?;
        let runtime = runtime()?;
        let mut endpoint = {
            let _guard = runtime.enter();
            Endpoint::client(
                if remote.is_ipv6() {
                    "[::]:0"
                } else {
                    "0.0.0.0:0"
                }
                .parse()
                .map_err(error)?,
            )?
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(error)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(IdentityVerifier(provider)))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(error)?,
        ));
        config.transport_config(Arc::new(transport()));
        endpoint.set_default_client_config(config);
        let (connection, send, recv) = runtime.block_on(async {
            let connection = tokio::time::timeout(
                Duration::from_secs(10),
                endpoint.connect(remote, host).map_err(error)?,
            )
            .await
            .map_err(error)?
            .map_err(error)?;
            let (send, recv) = connection.open_bi().await.map_err(error)?;
            Ok::<_, io::Error>((connection, send, recv))
        })?;
        Self::new(runtime, endpoint, connection, send, recv, false)
    }
    pub fn peer_key(&self) -> Option<&str> {
        self.shared.peer_key.as_deref()
    }
    pub fn channel_binding(&self) -> &str {
        &self.shared.binding
    }
    pub fn shutdown(&self) {
        self.shared.connection.close(0u32.into(), b"shutdown");
    }
    pub fn delivery(&self, delivery: DeliveryMode, max_buffer_ms: u32) {
        if let Ok(mut output) = self.shared.output.lock() {
            output.delivery = delivery;
            output.max_buffer_ms = max_buffer_ms.clamp(100, 10000);
        }
    }
    pub fn observe(&self, frame: &Frame) {
        let Frame::Message(value) = frame else {
            return;
        };
        let Ok(message) = Message::from_value(value.clone()) else {
            return;
        };
        if let Message::Request { method, params, .. } = &message {
            if method == method::DETACH {
                if let Some(binding) = params["binding"]
                    .as_u64()
                    .and_then(|binding| u32::try_from(binding).ok())
                {
                    if let Ok(mut output) = self.shared.output.lock() {
                        output.formats.remove(&binding);
                        output.gaps.remove(&binding);
                        output.lanes.remove(&binding);
                    }
                }
            }
        }
        if let Message::Notification {
            method,
            params: values,
        } = message
        {
            if method == method::GAP {
                if let Ok(gap) = params::<Gap>(values.clone()) {
                    if let Ok(mut output) = self.shared.output.lock() {
                        output.gaps.insert(gap.binding, gap.reason);
                    }
                }
            }
            if method == method::STREAM_FORMAT {
                if let Ok(format) = params::<StreamFormat>(values) {
                    if let Ok(mut output) = self.shared.output.lock() {
                        output.formats.insert(format.binding, format);
                    }
                }
            }
        }
    }
    /// Queue without blocking control or another camera. On loss, resume only
    /// at a keyframe; the receiver gets an explicit discontinuity/gap.
    pub fn media(&self, unit: &MediaUnit) -> io::Result<()> {
        let mut output = self
            .shared
            .output
            .lock()
            .map_err(|_| error("media state unavailable"))?;
        let Some(format) = output.formats.get(&unit.binding).cloned() else {
            return Err(error("format must precede media"));
        };
        let delivery = output.delivery;
        let max_buffer_ms = output.max_buffer_ms;
        if output.lanes.len() >= LANES && !output.lanes.contains_key(&unit.binding) {
            return Err(error("too many media bindings"));
        }
        let gap = if unit.keyframe() {
            output.gaps.remove(&unit.binding)
        } else {
            None
        };
        let lane = output.lanes.entry(unit.binding).or_insert_with(|| {
            let (newest, _) = watch::channel(0);
            Lane {
                number: 0,
                newest,
                units: None,
                waiting: true,
                gap: false,
            }
        });
        if unit.keyframe() {
            lane.number = lane.number.saturating_add(1);
            lane.newest.send_replace(lane.number);
            lane.units = None;
            lane.waiting = false;
        }
        if lane.waiting {
            return Ok(());
        }
        // Acquire the byte budget before copying or starting another task.
        let size = unit.data.len() + 45;
        let permit = match Arc::clone(&self.shared.budget).try_acquire_many_owned(size as u32) {
            Ok(permit) => permit,
            Err(_) => {
                lane.waiting = true;
                lane.gap = true;
                lane.units = None;
                return Ok(());
            }
        };
        let mut bytes = Vec::new();
        frame::write_frame(&mut bytes, &Frame::Media(unit.clone()))?;
        if unit.keyframe() {
            let (tx, rx) = mpsc::channel(64);
            lane.units = Some(tx);
            let header = Gop {
                binding: unit.binding,
                number: lane.number,
                format,
                delivery,
                max_buffer_ms,
                gap: gap.or_else(|| lane.gap.then_some(GapReason::Behind)),
            };
            lane.gap = false;
            self.shared.runtime.spawn(send_gop(
                self.shared.connection.clone(),
                header,
                rx,
                lane.newest.subscribe(),
            ));
        }
        let sent = lane.units.as_ref().is_some_and(|tx| {
            tx.try_send(Chunk {
                enqueued: std::time::Instant::now(),
                bytes,
                _permit: Some(permit),
            })
            .is_ok()
        });
        if !sent {
            lane.waiting = true;
            lane.gap = true;
            lane.units = None;
        }
        Ok(())
    }
}

async fn send_gop(
    connection: Connection,
    header: Gop,
    mut units: mpsc::Receiver<Chunk>,
    mut newest: watch::Receiver<u64>,
) {
    let opened = tokio::time::timeout(
        Duration::from_millis(u64::from(header.max_buffer_ms.clamp(100, 10000))),
        connection.open_uni(),
    );
    let result = if header.delivery == DeliveryMode::Latest {
        tokio::select! { result=opened=>result, _=newest.changed()=>return }
    } else {
        opened.await
    };
    let mut stream = match result {
        Ok(Ok(stream)) => stream,
        _ => return,
    };
    let mut bytes = Vec::new();
    if frame::write_frame(
        &mut bytes,
        &Frame::Message(Message::notification(HEADER, &header).to_value()),
    )
    .is_err()
    {
        return;
    }
    let header_write = tokio::time::timeout(
        Duration::from_millis(u64::from(header.max_buffer_ms.clamp(100, 10000))),
        stream.write_all(&bytes),
    );
    let written = if header.delivery == DeliveryMode::Latest {
        tokio::select! {result=header_write=>result, _=newest.changed()=>{let _=stream.reset(1u32.into());return;}}
    } else {
        header_write.await
    };
    if !matches!(written, Ok(Ok(()))) {
        let _ = stream.reset(1u32.into());
        return;
    }
    while let Some(chunk) = units.recv().await {
        let write = tokio::time::timeout(
            Duration::from_millis(u64::from(header.max_buffer_ms.clamp(100, 10000)))
                .saturating_sub(chunk.enqueued.elapsed()),
            stream.write_all(&chunk.bytes),
        );
        let result = if header.delivery == DeliveryMode::Latest {
            tokio::select! {result=write=>result, _=newest.changed()=>{let _=stream.reset(1u32.into());return;}}
        } else {
            write.await
        };
        if !matches!(result, Ok(Ok(()))) {
            let _ = stream.reset(1u32.into());
            return;
        }
    }
    let _ = stream.finish();
}
impl Read for QuicIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut slot = self.chunk.lock().map_err(|_| error("reader unavailable"))?;
        if slot.is_none() {
            let next = self
                .shared
                .runtime
                .block_on(async { self.shared.incoming.lock().await.recv().await });
            let Some(next) = next else {
                return Ok(0);
            };
            *slot = Some((next?, 0));
        }
        let Some((chunk, at)) = slot.as_mut() else {
            return Ok(0);
        };
        let count = bytes.len().min(chunk.bytes.len() - *at);
        bytes[..count].copy_from_slice(&chunk.bytes[*at..*at + count]);
        *at += count;
        if *at == chunk.bytes.len() {
            *slot = None;
        }
        Ok(count)
    }
}
impl Write for QuicIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.shared.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                self.shared.send.lock().await.write_all(bytes),
            )
            .await
            .map_err(error)?
            .map_err(error)?;
            Ok(bytes.len())
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub struct Listener {
    runtime: Arc<Runtime>,
    endpoint: Endpoint,
}
impl Listener {
    pub fn bind(address: SocketAddr, key: &SigningKey) -> io::Result<Self> {
        let runtime = runtime()?;
        let endpoint = {
            let _guard = runtime.enter();
            Endpoint::server(server_config(key)?, address)?
        };
        Ok(Self { runtime, endpoint })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }
    pub fn accept(&self) -> io::Result<QuicIo> {
        let (connection, send, recv) = self.runtime.block_on(async {
            loop {
                let incoming = self
                    .endpoint
                    .accept()
                    .await
                    .ok_or_else(|| error("listener closed"))?;
                if !incoming.remote_address_validated() {
                    let _ = incoming.retry();
                    continue;
                }
                let connection = match tokio::time::timeout(Duration::from_secs(10), incoming).await
                {
                    Ok(Ok(connection)) => connection,
                    _ => continue,
                };
                let (send, recv) =
                    match tokio::time::timeout(Duration::from_secs(10), connection.accept_bi())
                        .await
                    {
                        Ok(Ok(streams)) => streams,
                        _ => {
                            connection.close(1u32.into(), b"control stream timeout");
                            continue;
                        }
                    };
                break Ok::<_, io::Error>((connection, send, recv));
            }
        })?;
        QuicIo::new(
            Arc::clone(&self.runtime),
            self.endpoint.clone(),
            connection,
            send,
            recv,
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_analytics_protocol::{
        frame::{flags, MediaKind},
        messages::Codec,
    };
    fn pair() -> (QuicIo, QuicIo) {
        let key = SigningKey::from_bytes(&[7; 32]);
        let listener = Listener::bind("127.0.0.1:0".parse().unwrap(), &key).unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let io = listener.accept().unwrap();
            tx.send(io).unwrap();
        });
        let mut node = QuicIo::connect("127.0.0.1", address.port()).unwrap();
        frame::write_frame(
            &mut node,
            &Frame::Message(Message::request(1, method::PING, serde_json::json!({})).to_value()),
        )
        .unwrap();
        let mut plugin = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let _ = frame::read_frame(&mut plugin).unwrap();
        frame::write_frame(&mut node,&Frame::Message(Message::request(2,method::ATTACH,serde_json::json!({"binding":1,"camera":{"id":"c","name":"Camera"},"config":{}})).to_value())).unwrap();
        let _ = frame::read_frame(&mut plugin).unwrap();
        assert_eq!(node.peer_key(), Some(identity::public_key(&key).as_str()));
        assert_eq!(node.channel_binding(), plugin.channel_binding());
        let format = StreamFormat {
            binding: 1,
            codec: Codec::H264,
            width: 640,
            height: 360,
        };
        node.observe(&Frame::Message(
            Message::notification(method::STREAM_FORMAT, format).to_value(),
        ));
        (node, plugin)
    }
    fn unit(seq: u64, keyframe: bool, size: usize) -> MediaUnit {
        MediaUnit {
            binding: 1,
            media: MediaKind::Video,
            flags: if keyframe { flags::KEYFRAME } else { 0 },
            seq,
            time_us: seq as i64,
            pts_ns: Some(seq * 1_000_000),
            dts_ns: None,
            data: vec![seq as u8; size],
        }
    }
    #[test]
    fn tls_identity_and_reliable_gop_streams_preserve_order_and_camera_bytes() {
        let (node, mut plugin) = pair();
        node.delivery(DeliveryMode::Reliable, 1000);
        for seq in 1..=12 {
            node.media(&unit(seq, seq % 4 == 1, 128)).unwrap();
        }
        let reader = std::thread::spawn(move || {
            let mut units = vec![];
            while units.len() < 12 {
                if let Frame::Media(unit) = frame::read_frame(&mut plugin).unwrap() {
                    units.push(unit);
                }
            }
            units
        });
        let units = reader.join().unwrap();
        assert_eq!(
            units.iter().map(|unit| unit.seq).collect::<Vec<_>>(),
            (1..=12).collect::<Vec<_>>()
        );
        for received in units {
            assert_eq!(received.data, vec![received.seq as u8; 128]);
        }
        node.shutdown();
    }
    #[test]
    fn latest_abandons_a_blocked_gop_and_reports_the_gap_before_the_new_keyframe() {
        let (node, mut plugin) = pair();
        for seq in 1..=200 {
            node.media(&unit(seq, seq == 1, 512 * 1024)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        node.media(&unit(1000, true, 128)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        node.media(&unit(2000, true, 128)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut gap = false;
            loop {
                match frame::read_frame(&mut plugin) {
                    Ok(Frame::Media(unit)) if unit.seq >= 1000 => {
                        tx.send((gap, unit)).unwrap();
                        return;
                    }
                    Ok(Frame::Message(value)) if value["method"] == method::GAP => gap = true,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });
        let (gap, last) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(gap);
        assert!(last.keyframe() && last.discontinuity());
        assert_eq!(last.data, vec![last.seq as u8; 128]);
        node.shutdown();
    }
}
