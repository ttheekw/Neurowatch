//! A whole session against a small stand-in ingest on TLS over TCP: hello and
//! welcome, a track opened, units sent in a chunk and confirmed, a camera
//! without a plan held back, a refused session explained, and goodbye. The
//! real ingest's own tests (in its repository) cover the rest.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use nw_backup_client::{tls_config, AssignFuture, Assigner, Assignment, Backup, Config, Status};
use nw_backup_protocol::frame::{self, flags, Frame, MediaKind, MediaUnit};
use nw_backup_protocol::messages::{
    self, Ack, ClientMessage, Codec, RefusalCode, Refused, ServerMessage, TrackFormat, TrackKind,
    TrackOpened, Welcome,
};
use nw_backup_protocol::ranges::SeqRange;
use nw_backup_protocol::ALPN;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

struct Fixed(Assignment);

impl Assigner for Fixed {
    fn assign(&self) -> AssignFuture<'_> {
        let answer = Ok(self.0.clone());
        Box::pin(async move { answer })
    }
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A ticket for `cam-1` only (the stand-in does not check its signature).
fn ticket() -> String {
    let claims = serde_json::json!({
        "v": 1, "id": "t-session", "org_id": "org", "node_id": "node",
        "node_key": "00".repeat(32), "audience": "localhost",
        "issued_at_ms": now_ms(), "expires_at_ms": now_ms() + 15 * 60 * 1000,
        "cameras": [{"camera_id": "cam-1", "retention_s": 3600,
                     "limits": {"max_long_side": 1920, "max_short_side": 1080,
                                "max_fps": 30.0, "max_bitrate_bps": 4000000}}]
    });
    format!(
        "{}.{}",
        base64url(claims.to_string().as_bytes()),
        "00".repeat(64)
    )
}

fn h264() -> TrackFormat {
    TrackFormat {
        codec: Codec::H264,
        width: Some(1920),
        height: Some(1080),
        sample_rate: None,
        channels: None,
        codec_data: None,
    }
}

fn unit(seq: u64) -> MediaUnit {
    MediaUnit {
        media: MediaKind::Video,
        flags: if seq == 1 { flags::KEYFRAME } else { 0 },
        track: 0,
        seq,
        // Now: older units than the plan's window would be given up as a gap.
        time_us: now_ms() * 1000 + seq as i64 * 40_000,
        pts_ns: Some(seq * 40_000_000),
        dts_ns: None,
        data: vec![0, 0, 0, 1, 0x65, seq as u8],
    }
}

async fn read_frame(input: &mut (impl AsyncRead + Unpin)) -> Option<Frame> {
    let mut prefix = [0u8; frame::PREFIX_BYTES];
    input.read_exact(&mut prefix).await.ok()?;
    let mut body = vec![0u8; frame::body_length(prefix).ok()?];
    input.read_exact(&mut body).await.ok()?;
    frame::decode_body(&body).ok()
}

async fn send(output: &mut (impl AsyncWrite + Unpin), message: &ServerMessage) {
    let mut bytes = Vec::new();
    frame::encode_message(&mut bytes, &messages::encode(message)).unwrap();
    output.write_all(&bytes).await.unwrap();
    output.flush().await.unwrap();
}

/// What the stand-in saw: control messages by name, and the media units.
#[derive(Debug)]
enum Seen {
    Message(String),
    Unit(MediaUnit),
}

/// A stand-in ingest on a free port: TLS with a throwaway certificate for
/// `localhost`. `refuse`: it refuses every session instead.
async fn ingest(refuse: bool) -> (u16, Vec<u8>, mpsc::UnboundedReceiver<Seen>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert = certified.cert.der().to_vec();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
    let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone().into()], key)
    .unwrap();
    server.alpn_protocols = vec![ALPN.to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen, events) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(tls) = acceptor.accept(stream).await else {
                continue;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let (mut input, mut output) = tokio::io::split(tls);
                let mut received = 0u64;
                while let Some(frame) = read_frame(&mut input).await {
                    let message = match frame {
                        Frame::Media(unit) => {
                            received = received.max(unit.seq);
                            let track = unit.track;
                            let _ = seen.send(Seen::Unit(unit));
                            if received == 3 {
                                let ack = Ack {
                                    track,
                                    durable: vec![SeqRange(1, 3)],
                                };
                                send(&mut output, &ServerMessage::Ack(ack)).await;
                            }
                            continue;
                        }
                        Frame::Message(json) => messages::decode::<ClientMessage>(&json).unwrap(),
                    };
                    let name = serde_json::to_value(&message).unwrap()["type"]
                        .as_str()
                        .unwrap_or("?")
                        .to_owned();
                    let _ = seen.send(Seen::Message(name));
                    match message {
                        ClientMessage::Hello(_) if refuse => {
                            let refused = Refused {
                                code: RefusalCode::Busy,
                                message: "no plan".into(),
                                retry_after_ms: None,
                            };
                            send(&mut output, &ServerMessage::Refused(refused)).await;
                            return;
                        }
                        ClientMessage::Hello(_) => {
                            let welcome = Welcome {
                                session_id: "s-1".into(),
                                server_time_ms: 1_790_000_000_000,
                                ack_interval_ms: 100,
                                max_tracks: 8,
                                instance_proof: None,
                            };
                            send(&mut output, &ServerMessage::Welcome(welcome)).await;
                        }
                        ClientMessage::OpenTrack(open) => {
                            let opened = TrackOpened {
                                track: open.track,
                                durable: vec![],
                            };
                            send(&mut output, &ServerMessage::TrackOpened(opened)).await;
                        }
                        ClientMessage::Ping(ping) => {
                            send(&mut output, &ServerMessage::Pong(ping)).await
                        }
                        ClientMessage::Bye => return,
                        _ => {}
                    }
                }
            });
        }
    });
    (port, cert, events)
}

fn config(ca: &[u8]) -> Config {
    Config {
        node_key: SigningKey::from_bytes(&[7; 32]),
        recorder: "test".into(),
        tls: tls_config(Some(ca)).unwrap(),
        outage_dir: None,
        memory_bytes_per_track: 1 << 20,
        disk_bytes_per_track: 0,
    }
}

fn assignment(port: u16) -> Arc<Fixed> {
    Arc::new(Fixed(Assignment {
        ticket: ticket(),
        server_name: "localhost".into(),
        quic: None,
        tcp: Some(format!("127.0.0.1:{port}")),
        instance_key: None,
    }))
}

async fn until(backup: &Backup, test: impl Fn(&Status) -> bool) -> Status {
    for _ in 0..400 {
        let status = backup.status();
        if test(&status) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never reached: {:?}", backup.status());
}

#[tokio::test]
async fn a_session_sends_a_cameras_units_until_they_are_confirmed() {
    let (port, ca, mut seen) = ingest(false).await;
    let backup = Backup::start(config(&ca), assignment(port));
    let track = backup.track("cam-1", TrackKind::Video, h264());
    let unplanned = backup.track("cam-2", TrackKind::Video, h264());
    for seq in 1..=3 {
        track.push(unit(seq));
    }
    let connected = until(&backup, |s| s.state == "connected").await;
    assert_eq!(connected.ingest.as_deref(), Some("localhost"));
    assert_eq!(connected.transport, Some("tcp"));
    // Confirmed: nothing left waiting.
    let status = until(&backup, |s| {
        s.tracks
            .iter()
            .any(|t| t.camera_id == "cam-1" && t.unconfirmed_units == 0)
    })
    .await;
    assert_eq!(status.problem, None);
    // A camera the ticket does not name is held back, and says why.
    let _ = until(&backup, |_| unplanned.refusal().is_some()).await;
    assert_eq!(
        unplanned.refusal().as_deref(),
        Some("This camera has no backup plan.")
    );
    backup.stop().await;

    let mut names = Vec::new();
    let mut units = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), seen.recv()).await {
        match event {
            Seen::Message(name) => {
                let done = name == "bye";
                names.push(name);
                if done {
                    break;
                }
            }
            Seen::Unit(unit) => units.push(unit),
        }
    }
    assert_eq!(
        names.first().map(String::as_str),
        Some("hello"),
        "{names:?}"
    );
    assert!(names.iter().any(|n| n == "open_track"), "{names:?}");
    assert!(
        names.iter().any(|n| n == "chunk"),
        "units follow a chunk header: {names:?}"
    );
    assert_eq!(names.last().map(String::as_str), Some("bye"), "{names:?}");
    assert_eq!(units.iter().map(|u| u.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(units[0].keyframe(), "a run starts at a keyframe");
}

#[tokio::test]
async fn a_refused_session_says_why_and_tries_again_later() {
    let (port, ca, _seen) = ingest(true).await;
    let backup = Backup::start(config(&ca), assignment(port));
    let status = until(&backup, |s| s.state == "retrying").await;
    let problem = status.problem.unwrap();
    assert!(
        !problem.is_empty() && problem != "The backup server did not answer.",
        "{problem}"
    );
    backup.stop().await;
}
