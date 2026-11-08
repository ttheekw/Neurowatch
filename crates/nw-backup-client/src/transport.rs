//! Connections to an ingest: QUIC, or TLS over TCP where UDP is blocked.
use nw_backup_protocol::{
    frame::{self, Frame, MediaUnit},
    messages::{self, Chunk, ClientMessage},
    ALPN, CHANNEL_BINDING_BYTES, EXPORTER_LABEL,
};
use quinn::{crypto::rustls::QuicClientConfig, Connection, Endpoint, SendStream, TransportConfig};
use std::{io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt, WriteHalf},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::client::TlsStream;

/// How long a connection attempt may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub type Reader = Box<dyn AsyncRead + Unpin + Send>;

/// An open connection: its kind, its control stream, the TLS channel binding.
pub struct Connected {
    pub link: Link,
    pub reader: Reader,
    pub binding: [u8; CHANNEL_BINDING_BYTES],
}

#[derive(Clone)]
pub enum Link {
    Quic {
        connection: Connection,
        control: Arc<Mutex<SendStream>>,
    },
    Tcp(Arc<Mutex<TcpOut>>),
}

/// TLS over TCP's one stream, and whose run it is carrying.
pub struct TcpOut {
    write: WriteHalf<TlsStream<TcpStream>>,
    /// (track, live, run number) of the last chunk header written.
    current: Option<(u32, bool, u64)>,
}

/// The TLS settings for ingests: public certificates, plus `extra_ca` (a
/// development ingest's private CA, PEM-decoded DER) when given.
pub fn tls_config(extra_ca: Option<&[u8]>) -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(der) = extra_ca {
        roots
            .add(rustls::pki_types::CertificateDer::from(der.to_vec()))
            .map_err(|e| format!("the ingest CA is not usable: {e}"))?;
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

async fn resolve(address: &str) -> io::Result<SocketAddr> {
    tokio::net::lookup_host(address)
        .await?
        .next()
        .ok_or_else(|| io::Error::other(format!("{address} has no address")))
}

/// Connect over QUIC.
pub async fn quic(
    address: &str,
    server_name: &str,
    tls: Arc<rustls::ClientConfig>,
) -> io::Result<Connected> {
    let remote = resolve(address).await?;
    let crypto = QuicClientConfig::try_from(tls.as_ref().clone()).map_err(io::Error::other)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = TransportConfig::default();
    transport
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .max_idle_timeout(Some(
            Duration::from_secs(20)
                .try_into()
                .map_err(io::Error::other)?,
        ));
    config.transport_config(Arc::new(transport));
    let local: SocketAddr = if remote.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
    .parse()
    .map_err(io::Error::other)?;
    let endpoint = Endpoint::client(local)?;
    let connecting = endpoint
        .connect_with(config, remote, server_name)
        .map_err(io::Error::other)?;
    let connection = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "QUIC connection timed out"))?
        .map_err(io::Error::other)?;
    let mut binding = [0u8; CHANNEL_BINDING_BYTES];
    connection
        .export_keying_material(&mut binding, EXPORTER_LABEL, b"")
        .map_err(|_| io::Error::other("TLS exporter unavailable"))?;
    let (send, recv) = connection.open_bi().await.map_err(io::Error::other)?;
    Ok(Connected {
        link: Link::Quic {
            connection,
            control: Arc::new(Mutex::new(send)),
        },
        reader: Box::new(recv),
        binding,
    })
}

/// Connect over TLS on TCP.
pub async fn tcp(
    address: &str,
    server_name: &str,
    tls: Arc<rustls::ClientConfig>,
) -> io::Result<Connected> {
    let remote = resolve(address).await?;
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(remote))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TCP connection timed out"))??;
    stream.set_nodelay(true)?;
    let name = rustls::pki_types::ServerName::try_from(server_name.to_owned())
        .map_err(io::Error::other)?;
    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_rustls::TlsConnector::from(tls).connect(name, stream),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))??;
    let binding = stream
        .get_ref()
        .1
        .export_keying_material([0u8; CHANNEL_BINDING_BYTES], EXPORTER_LABEL, Some(b""))
        .map_err(|_| io::Error::other("TLS exporter unavailable"))?;
    let (read, write) = tokio::io::split(stream);
    Ok(Connected {
        link: Link::Tcp(Arc::new(Mutex::new(TcpOut {
            write,
            current: None,
        }))),
        reader: Box::new(read),
        binding,
    })
}

impl Link {
    pub fn transport(&self) -> &'static str {
        match self {
            Link::Quic { .. } => "quic",
            Link::Tcp(_) => "tcp",
        }
    }

    /// One control message.
    pub async fn send(&self, message: &ClientMessage) -> io::Result<()> {
        let mut bytes = Vec::new();
        frame::encode_message(&mut bytes, &messages::encode(message))?;
        match self {
            Link::Quic { control, .. } => control
                .lock()
                .await
                .write_all(&bytes)
                .await
                .map_err(io::Error::other),
            Link::Tcp(out) => {
                let mut out = out.lock().await;
                out.write.write_all(&bytes).await?;
                out.write.flush().await
            }
        }
    }

    /// Start a run of one track's units: its own QUIC stream (live first),
    /// or a chunk header on the shared TCP stream before its next unit.
    pub async fn run(&self, track: u32, live: bool, run: u64) -> io::Result<RunOut> {
        match self {
            Link::Quic { connection, .. } => {
                let stream = connection.open_uni().await.map_err(io::Error::other)?;
                let _ = stream.set_priority(if live { 1 } else { 0 });
                Ok(RunOut::Quic {
                    stream,
                    header: Some(()),
                    track,
                    live,
                })
            }
            Link::Tcp(out) => Ok(RunOut::Tcp {
                out: out.clone(),
                track,
                live,
                run,
            }),
        }
    }

    pub fn close(&self) {
        if let Link::Quic { connection, .. } = self {
            connection.close(0u32.into(), b"bye");
        }
    }
}

pub enum RunOut {
    Quic {
        stream: SendStream,
        header: Option<()>,
        track: u32,
        live: bool,
    },
    Tcp {
        out: Arc<Mutex<TcpOut>>,
        track: u32,
        live: bool,
        run: u64,
    },
}

impl RunOut {
    /// Send one unit; `gop_us` is when its picture group's keyframe arrived
    /// (named in a header when the run does not start at that keyframe).
    pub async fn send(&mut self, unit: &MediaUnit, gop_us: i64) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(unit.data.len() + 96);
        let header = |track: u32, live: bool, bytes: &mut Vec<u8>| {
            let gop_start_us = (!unit.keyframe()).then_some(gop_us);
            let chunk = ClientMessage::Chunk(Chunk {
                track,
                live,
                gop_start_us,
            });
            frame::encode_message(bytes, &messages::encode(&chunk))
        };
        match self {
            RunOut::Quic {
                stream,
                header: pending,
                track,
                live,
            } => {
                if pending.take().is_some() {
                    header(*track, *live, &mut bytes)?;
                }
                frame::encode_media(&mut bytes, unit)?;
                stream.write_all(&bytes).await.map_err(io::Error::other)
            }
            RunOut::Tcp {
                out,
                track,
                live,
                run,
            } => {
                let mut out = out.lock().await;
                if out.current != Some((*track, *live, *run)) {
                    header(*track, *live, &mut bytes)?;
                    out.current = Some((*track, *live, *run));
                }
                frame::encode_media(&mut bytes, unit)?;
                out.write.write_all(&bytes).await?;
                out.write.flush().await
            }
        }
    }

    pub fn finish(self) {
        if let RunOut::Quic { mut stream, .. } = self {
            let _ = stream.finish();
        }
    }
}

/// One frame; `None` when the stream ended at a frame boundary.
pub async fn read_frame(recv: &mut Reader) -> io::Result<Option<Frame>> {
    let mut prefix = [0u8; frame::PREFIX_BYTES];
    let mut read = 0;
    while read < prefix.len() {
        let n = recv.read(&mut prefix[read..]).await?;
        if n == 0 {
            if read == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the stream ended inside a frame",
            ));
        }
        read += n;
    }
    let mut body = vec![0u8; frame::body_length(prefix)?];
    recv.read_exact(&mut body).await?;
    frame::decode_body(&body).map(Some)
}
