#![cfg(unix)]
use nw_analytics_sdk::{
    identity::{self, SigningKey},
    protocol::{
        frame::{self, flags, Frame, MediaKind, MediaUnit},
        messages::{Codec, Manifest, StreamFormat},
        method,
        rpc::Message,
        web,
    },
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Server {
    child: Child,
    dir: PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
enum Node {
    Tcp(std::net::TcpStream),
    Quic(nw_analytics_sdk::quic::QuicIo),
    Tls(nw_analytics_sdk::tls::TlsIo),
}
impl Read for Node {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(io) => io.read(bytes),
            Self::Quic(io) => io.read(bytes),
            Self::Tls(io) => io.read(bytes),
        }
    }
}
impl Write for Node {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(io) => io.write(bytes),
            Self::Quic(io) => io.write(bytes),
            Self::Tls(io) => io.write(bytes),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn response(
    node: &mut Node,
    id: u64,
) -> Result<serde_json::Value, nw_analytics_sdk::protocol::rpc::RpcError> {
    loop {
        let Frame::Message(value) = frame::read_frame(node).unwrap() else {
            panic!("message expected")
        };
        if let Message::Response { id: answer, result } = Message::from_value(value).unwrap() {
            if answer == id {
                return result;
            }
        }
    }
}
fn request(node: &mut Node, id: u64, method: &str, params: impl serde::Serialize) {
    frame::write_frame(
        node,
        &Frame::Message(Message::request(id, method, params).to_value()),
    )
    .unwrap();
}
fn run(transport: &str) {
    let dir = std::env::temp_dir().join(format!(
        "nw-python-bridge-{}",
        identity::challenge().unwrap()
    ));
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python/tests/bridge_fixture.py");
    let address = if transport == "quic" {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        format!("quic:{}", socket.local_addr().unwrap())
    } else {
        if transport == "tls" {
            let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("tls:{}", socket.local_addr().unwrap())
        } else {
            "tcp:127.0.0.1:0".into()
        }
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_nw-analytics-bridge"))
        .arg("--ready")
        .arg("--listen")
        .arg(address)
        .arg("--identity")
        .arg(dir.join("identity.key"))
        .arg("--")
        .arg("python3")
        .arg(fixture)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let output = child.stdout.take().unwrap();
    let _server = Server { child, dir };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(output).read_line(&mut line).unwrap();
        tx.send(line).unwrap();
    });
    let ready = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let socket: std::net::SocketAddr = ready
        .trim()
        .strip_prefix("ready ")
        .unwrap()
        .parse()
        .unwrap();
    let mut node = if transport == "quic" {
        Node::Quic(nw_analytics_sdk::quic::QuicIo::connect("127.0.0.1", socket.port()).unwrap())
    } else if transport == "tls" {
        Node::Tls(nw_analytics_sdk::tls::TlsIo::connect("127.0.0.1", socket.port()).unwrap())
    } else {
        let io = std::net::TcpStream::connect(socket).unwrap();
        io.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Node::Tcp(io)
    };
    let channel = match &node {
        Node::Quic(io) => io.channel_binding().to_owned(),
        Node::Tls(io) => io.channel_binding().to_owned(),
        _ => String::new(),
    };
    let nonce = identity::challenge().unwrap();
    request(
        &mut node,
        1,
        method::HELLO,
        serde_json::json!({"protocol":1,"node":{"name":"Test","version":"1"},"challenge":nonce}),
    );
    let manifest: Manifest = serde_json::from_value(response(&mut node, 1).unwrap()).unwrap();
    assert!(identity::verify_manifest(&manifest, &nonce, &channel));
    let plugin = manifest.identity.unwrap();
    match &node {
        Node::Tls(io) => assert_eq!(io.peer_key(), Some(plugin.public_key.as_str())),
        Node::Quic(io) => assert_eq!(io.peer_key(), Some(plugin.public_key.as_str())),
        _ => {}
    }
    let key = SigningKey::from_bytes(&[9; 32]);
    let auth = identity::authenticate(&key, &nonce, &plugin, None);
    let mut bad = auth.clone();
    bad.signature = "00".repeat(64);
    request(&mut node, 2, method::AUTHENTICATE, bad);
    assert!(response(&mut node, 2).is_err());
    request(&mut node, 3, method::AUTHENTICATE, auth);
    response(&mut node, 3).unwrap();
    request(
        &mut node,
        4,
        method::ATTACH,
        serde_json::json!({"binding":1,"camera":{"id":"c","name":"Camera"},"config":{}}),
    );
    response(&mut node, 4).unwrap();
    let format = Frame::Message(
        Message::notification(
            method::STREAM_FORMAT,
            StreamFormat {
                binding: 1,
                codec: Codec::H264,
                width: 640,
                height: 360,
            },
        )
        .to_value(),
    );
    if let Node::Quic(io) = &node {
        io.observe(&format);
    }
    frame::write_frame(&mut node, &format).unwrap();
    let unit = MediaUnit {
        binding: 1,
        media: MediaKind::Video,
        flags: flags::KEYFRAME,
        seq: 1 << 48,
        time_us: 1_700_000_000_123_456,
        pts_ns: None,
        dts_ns: None,
        data: vec![0, 0, 0, 1, 0x65],
    };
    if let Node::Quic(io) = &node {
        io.media(&unit).unwrap();
    } else {
        frame::write_frame(&mut node, &Frame::Media(unit.clone())).unwrap();
    }
    loop {
        if let Frame::Message(value) = frame::read_frame(&mut node).unwrap() {
            if value["method"] == "test.unit" {
                assert_eq!(value["params"]["seq"], unit.seq);
                assert_eq!(value["params"]["time_us"], unit.time_us);
                assert_eq!(value["params"]["data"], "0000000165");
                break;
            }
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let user = identity::sign_user(
        &key,
        web::UserContext {
            user: "viewer".into(),
            role: "viewer".into(),
            cameras: vec![],
            plugin_key: plugin.public_key,
            expires_at: now + 30,
            nonce: identity::challenge().unwrap(),
        },
    );
    let mut forged = user.clone();
    forged.context.role = "admin".into();
    request(
        &mut node,
        5,
        "web.request",
        web::Request {
            path: "/".into(),
            method: "GET".into(),
            body_b64: String::new(),
            content_type: String::new(),
            user: forged,
        },
    );
    assert!(response(&mut node, 5).is_err());
    request(
        &mut node,
        6,
        "web.request",
        web::Request {
            path: "/".into(),
            method: "GET".into(),
            body_b64: String::new(),
            content_type: String::new(),
            user: user.clone(),
        },
    );
    let meta: web::Response = serde_json::from_value(response(&mut node, 6).unwrap()).unwrap();
    request(
        &mut node,
        7,
        "web.read",
        web::Read {
            token: meta.token,
            offset: 0,
            user,
        },
    );
    assert!(response(&mut node, 7).unwrap()["eof"].as_bool().unwrap());
}
#[test]
fn python_stdio_works_over_local_tcp() {
    run("tcp");
}
#[test]
fn python_stdio_works_over_authenticated_quic() {
    run("quic");
}

#[test]
fn python_stdio_works_over_authenticated_tls_tcp() {
    run("tls");
}
