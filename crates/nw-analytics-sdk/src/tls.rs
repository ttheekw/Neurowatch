//! TLS 1.3 fallback for networks that block UDP. Same bounded frame stream and
//! certificate/exporter identity checks as QUIC; never eligible for raw offers.
use crate::{
    identity::{self, SigningKey},
    quic::{certificate_key, IdentityVerifier},
};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
const PART: usize = 64 * 1024;
const WAIT: Duration = Duration::from_secs(5);
fn error(value: impl std::fmt::Display) -> io::Error {
    io::Error::other(value.to_string())
}
struct Shared {
    tx: SyncSender<Vec<u8>>,
    rx: Mutex<Receiver<Vec<u8>>>,
    current: Mutex<(Vec<u8>, usize)>,
    socket: TcpStream,
    closed: Arc<AtomicBool>,
    binding: String,
    peer: Option<String>,
    worker: std::thread::Thread,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        self.worker.unpark();
    }
}
#[derive(Clone)]
pub struct TlsIo(Arc<Shared>);
impl TlsIo {
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let address = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| error("hostname has no address"))?;
        let socket = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(error)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(IdentityVerifier(provider)))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"nw-analytics/1".to_vec()];
        let name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(error)?;
        Self::new(
            socket,
            rustls::ClientConnection::new(Arc::new(config), name)
                .map_err(error)?
                .into(),
            false,
        )
    }
    fn new(mut socket: TcpStream, mut tls: rustls::Connection, plugin: bool) -> io::Result<Self> {
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while tls.is_handshaking() {
            if Instant::now() >= deadline {
                return Err(error("TLS handshake timed out"));
            }
            match tls.complete_io(&mut socket) {
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        if tls.alpn_protocol() != Some(b"nw-analytics/1") {
            return Err(error("analytics ALPN not negotiated"));
        }
        let mut binding = [0; 32];
        tls.export_keying_material(&mut binding, b"NeuroWatch analytics session v1", Some(b""))
            .map_err(error)?;
        let peer = if plugin {
            None
        } else {
            Some(
                certificate_key(
                    tls.peer_certificates()
                        .and_then(|certs| certs.first())
                        .ok_or_else(|| error("missing plugin certificate"))?,
                )
                .map_err(error)?,
            )
        };
        tls.set_buffer_limit(Some(PART));
        socket.set_nonblocking(true)?;
        let shutdown = socket.try_clone()?;
        // Four MiB each direction, including buffers whose consumers stalled.
        let (tx, out) = mpsc::sync_channel(64);
        let (input, rx) = mpsc::sync_channel(64);
        let closed = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&closed);
        let worker = std::thread::Builder::new()
            .name("analytics-tls".into())
            .spawn(move || {
                let _ = pump(&mut socket, &mut tls, out, input, &stopping);
                stopping.store(true, Ordering::Release);
                let _ = socket.shutdown(std::net::Shutdown::Both);
            })?;
        Ok(Self(Arc::new(Shared {
            tx,
            rx: Mutex::new(rx),
            current: Mutex::new((vec![], 0)),
            socket: shutdown,
            closed,
            binding: identity::hex(&binding),
            peer,
            worker: worker.thread().clone(),
        })))
    }
    pub fn channel_binding(&self) -> &str {
        &self.0.binding
    }
    pub fn peer_key(&self) -> Option<&str> {
        self.0.peer.as_deref()
    }
    pub fn shutdown(&self) {
        self.0.closed.store(true, Ordering::Release);
        let _ = self.0.socket.shutdown(std::net::Shutdown::Both);
        self.0.worker.unpark();
    }
}
fn blocked(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
fn pump(
    socket: &mut TcpStream,
    tls: &mut rustls::Connection,
    out: Receiver<Vec<u8>>,
    input: SyncSender<Vec<u8>>,
    closed: &AtomicBool,
) -> io::Result<()> {
    let mut outgoing = None::<(Vec<u8>, usize)>;
    let mut incoming = None;
    let mut buffer = [0; PART];
    let mut last_write = Instant::now();
    while !closed.load(Ordering::Acquire) {
        let mut progress = false;
        if outgoing.is_none() {
            outgoing = out.try_recv().ok().map(|bytes| (bytes, 0));
        }
        if let Some((bytes, offset)) = outgoing.as_mut() {
            match tls.writer().write(&bytes[*offset..]) {
                Ok(count) => {
                    *offset += count;
                    progress |= count > 0;
                    if *offset == bytes.len() {
                        outgoing = None;
                    }
                }
                Err(e) if blocked(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if tls.wants_write() {
            match tls.write_tls(socket) {
                Ok(0) => return Err(error("TLS socket closed")),
                Ok(_) => {
                    progress = true;
                    last_write = Instant::now();
                }
                Err(e) if blocked(&e) => {
                    if last_write.elapsed() >= WAIT {
                        return Err(error("TLS writer stalled"));
                    }
                }
                Err(e) => return Err(e),
            }
        } else {
            last_write = Instant::now();
        }
        if let Some(bytes) = incoming.take() {
            match input.try_send(bytes) {
                Ok(()) => progress = true,
                Err(TrySendError::Full(bytes)) => incoming = Some(bytes),
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
        if incoming.is_none() {
            match tls.reader().read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    incoming = Some(buffer[..count].to_vec());
                    progress = true;
                }
                Err(e) if blocked(&e) => {}
                Err(e) => return Err(e),
            }
            if incoming.is_none() && tls.wants_read() {
                match tls.read_tls(socket) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {
                        tls.process_new_packets().map_err(error)?;
                        progress = true;
                    }
                    Err(e) if blocked(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        if !progress {
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
    Ok(())
}
impl Read for TlsIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut current = self.0.current.lock().map_err(error)?;
        if current.1 == current.0.len() {
            let next = self.0.rx.lock().map_err(error)?.recv();
            match next {
                Ok(next) => *current = (next, 0),
                Err(_) => return Ok(0),
            }
        }
        let count = bytes.len().min(current.0.len() - current.1);
        bytes[..count].copy_from_slice(&current.0[current.1..current.1 + count]);
        current.1 += count;
        self.0.worker.unpark();
        Ok(count)
    }
}
impl Write for TlsIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(PART);
        let mut part = bytes[..count].to_vec();
        let deadline = Instant::now() + WAIT;
        loop {
            if self.0.closed.load(Ordering::Acquire) {
                return Err(error("TLS session closed"));
            }
            match self.0.tx.try_send(part) {
                Ok(()) => {
                    self.0.worker.unpark();
                    return Ok(count);
                }
                Err(TrySendError::Disconnected(_)) => return Err(error("TLS writer closed")),
                Err(TrySendError::Full(bytes)) => part = bytes,
            }
            if Instant::now() >= deadline {
                return Err(error("TLS writer stalled"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub struct Listener {
    socket: TcpListener,
    config: Arc<rustls::ServerConfig>,
}
impl Listener {
    pub fn bind(address: SocketAddr, key: &SigningKey) -> io::Result<Self> {
        Ok(Self {
            socket: TcpListener::bind(address)?,
            config: Arc::new(crate::quic::server_tls(key)?),
        })
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
    pub fn accept(&self) -> io::Result<TlsIo> {
        loop {
            let (socket, _) = self.socket.accept()?;
            let tls = rustls::ServerConnection::new(Arc::clone(&self.config)).map_err(error)?;
            if let Ok(io) = TlsIo::new(socket, tls.into(), true) {
                return Ok(io);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_handshakes_do_not_end_the_listener_and_shutdown_unblocks_readers() {
        let key = SigningKey::from_bytes(&[13; 32]);
        let listener = Listener::bind("127.0.0.1:0".parse().unwrap(), &key).unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut plugin = listener.accept().unwrap();
            tx.send(plugin.channel_binding().to_owned()).unwrap();
            let mut bytes = [0];
            assert_eq!(plugin.read(&mut bytes).unwrap(), 0);
        });
        let mut bad = TcpStream::connect(address).unwrap();
        bad.write_all(b"not a TLS handshake").unwrap();
        drop(bad);
        let node = TlsIo::connect("127.0.0.1", address.port()).unwrap();
        assert_eq!(
            node.peer_key(),
            Some(identity::hex(key.verifying_key().as_bytes()).as_str())
        );
        assert_eq!(
            node.channel_binding(),
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        );
        node.shutdown();
        thread.join().unwrap();
    }
}
