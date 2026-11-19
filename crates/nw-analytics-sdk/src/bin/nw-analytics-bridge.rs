//! The Python SDK's transport process. It shares the Rust TLS/authentication
//! implementation; plugin business logic communicates over framed stdio.
use nw_analytics_sdk::{
    identity::{self, SigningKey},
    protocol::{
        address::Address,
        error_code,
        frame::{read_frame, write_frame, Frame},
        messages::{Hello, Manifest},
        method,
        rpc::{params, Message, RpcError},
    },
};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{mpsc, Arc},
    time::Duration,
};
#[derive(Clone)]
enum Socket {
    #[cfg(unix)]
    Unix(Arc<std::os::unix::net::UnixStream>),
    Tcp(Arc<std::net::TcpStream>),
    #[cfg(windows)]
    Pipe(nw_analytics_sdk::pipe::Pipe),
    Quic(nw_analytics_sdk::quic::QuicIo),
    Tls(nw_analytics_sdk::tls::TlsIo),
}
impl Socket {
    fn binding(&self) -> &str {
        match self {
            Self::Quic(io) => io.channel_binding(),
            Self::Tls(io) => io.channel_binding(),
            _ => "",
        }
    }
    fn shutdown(&self) {
        match self {
            #[cfg(unix)]
            Self::Unix(socket) => {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
            Self::Tcp(socket) => {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
            #[cfg(windows)]
            Self::Pipe(socket) => socket.shutdown(),
            Self::Quic(socket) => socket.shutdown(),
            Self::Tls(socket) => socket.shutdown(),
        }
    }
}
impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(socket) => (&**socket).read(bytes),
            Self::Tcp(socket) => (&**socket).read(bytes),
            #[cfg(windows)]
            Self::Pipe(socket) => socket.read(bytes),
            Self::Quic(socket) => socket.read(bytes),
            Self::Tls(socket) => socket.read(bytes),
        }
    }
}
impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(socket) => (&**socket).write(bytes),
            Self::Tcp(socket) => (&**socket).write(bytes),
            #[cfg(windows)]
            Self::Pipe(socket) => socket.write(bytes),
            Self::Quic(socket) => socket.write(bytes),
            Self::Tls(socket) => socket.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
enum Input {
    Node(Frame),
    Plugin(Frame),
    Closed,
}
fn read<R: Read>(mut reader: R, tx: mpsc::SyncSender<Input>, node: bool) {
    while let Ok(frame) = read_frame(&mut reader) {
        if tx
            .send(if node {
                Input::Node(frame)
            } else {
                Input::Plugin(frame)
            })
            .is_err()
        {
            return;
        }
    }
    let _ = tx.send(Input::Closed);
}
fn session(mut socket: Socket, key: Arc<SigningKey>, command: Arc<Vec<String>>) -> io::Result<()> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .env("NW_ANALYTICS_SESSION", "1")
        .env("NW_ANALYTICS_BRIDGE_PID", std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let result = (|| {
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child input missing"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child output missing"))?;
        let (child_tx, child_rx) = mpsc::sync_channel(4);
        let child_writer = std::thread::spawn(move || {
            let mut input = input;
            while let Ok(frame) = child_rx.recv() {
                if write_frame(&mut input, &frame)
                    .and_then(|_| input.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let (tx, rx) = mpsc::sync_channel(4);
        let reader = {
            let tx = tx.clone();
            let socket = socket.clone();
            std::thread::spawn(move || read(socket, tx, true))
        };
        let plugin = std::thread::spawn(move || read(output, tx, false));
        let mut hello: Option<(u64, String)> = None;
        let mut signed = None;
        let mut auth_id = None;
        let mut authorized = false;
        let mut node_key: Option<String> = None;
        let result = (|| loop {
            let received = match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(input) => input,
                Err(_) => break Ok(()),
            };
            match received {
                Input::Closed => break Ok(()),
                Input::Node(Frame::Message(value)) => {
                    let message = Message::from_value(value.clone()).map_err(io::Error::other)?;
                    if let Message::Request {
                        id,
                        method,
                        params: values,
                    } = &message
                    {
                        if method == method::HELLO {
                            if hello.is_some() {
                                break Err(io::Error::other("duplicate hello"));
                            }
                            let greeting: Hello = params(values.clone())
                                .map_err(|_| io::Error::other("invalid hello"))?;
                            if identity::unhex::<32>(&greeting.challenge).is_none() {
                                break Err(io::Error::other("invalid challenge"));
                            }
                            hello = Some((*id, greeting.challenge));
                        } else if method == method::AUTHENTICATE {
                            let authentication = params(values.clone())
                                .map_err(|_| io::Error::other("invalid authentication"))?;
                            if !signed.as_ref().zip(hello.as_ref()).is_some_and(
                                |(identity, (_, nonce))| {
                                    identity::verify_authenticate(&authentication, nonce, identity)
                                },
                            ) {
                                write_frame(
                                    &mut socket,
                                    &Frame::Message(
                                        Message::reply(
                                            *id,
                                            Err::<(), _>(RpcError::new(
                                                error_code::REJECTED,
                                                "Invalid node signature",
                                            )),
                                        )
                                        .to_value(),
                                    ),
                                )?;
                                continue;
                            }
                            auth_id = Some(*id);
                            node_key = Some(authentication.public_key);
                        } else if !authorized && method != method::PING {
                            write_frame(
                                &mut socket,
                                &Frame::Message(
                                    Message::reply(
                                        *id,
                                        Err::<(), _>(RpcError::new(
                                            error_code::REJECTED,
                                            "Authenticate first",
                                        )),
                                    )
                                    .to_value(),
                                ),
                            )?;
                            continue;
                        }
                        if method == "web.request" || method == "web.read" {
                            let user = values.get("user").cloned().and_then(|value| {
                                serde_json::from_value::<
                                        nw_analytics_sdk::protocol::web::SignedUser,
                                    >(value)
                                    .ok()
                            });
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |time| time.as_secs() as i64);
                            if !authorized
                                || !user.as_ref().zip(node_key.as_deref()).is_some_and(
                                    |(user, node)| {
                                        identity::verify_user(
                                            node,
                                            &identity::public_key(&key),
                                            user,
                                            now,
                                        )
                                    },
                                )
                            {
                                write_frame(
                                    &mut socket,
                                    &Frame::Message(
                                        Message::reply(
                                            *id,
                                            Err::<(), _>(RpcError::new(
                                                error_code::REJECTED,
                                                "Invalid viewer context",
                                            )),
                                        )
                                        .to_value(),
                                    ),
                                )?;
                                continue;
                            }
                        }
                    }
                    if !authorized && matches!(message, Message::Notification { .. }) {
                        continue;
                    }
                    child_tx
                        .try_send(Frame::Message(value))
                        .map_err(|_| io::Error::other("Python session stopped reading"))?;
                }
                Input::Node(frame) => {
                    if authorized {
                        child_tx
                            .try_send(frame)
                            .map_err(|_| io::Error::other("Python session stopped reading"))?;
                    }
                }
                Input::Plugin(Frame::Message(value)) => {
                    let mut message = Message::from_value(value).map_err(io::Error::other)?;
                    if let Message::Response { id, result } = &mut message {
                        if hello.as_ref().is_some_and(|(hello, _)| id == hello) {
                            if let Ok(value) = result {
                                let mut manifest: Manifest = params(value.clone())
                                    .map_err(|_| io::Error::other("invalid plugin manifest"))?;
                                // The Python transport delivers compressed media. Raw-picture
                                // capability forwarding is available in the native Rust SDK.
                                manifest.locality_proof = None;
                                manifest.media.decoded = None;
                                manifest
                                    .permissions
                                    .retain(|permission| permission != "decoded_frames");
                                identity::sign_manifest(
                                    &mut manifest,
                                    &key,
                                    &hello
                                        .as_ref()
                                        .ok_or_else(|| io::Error::other("hello missing"))?
                                        .1,
                                    socket.binding(),
                                )
                                .map_err(io::Error::other)?;
                                signed = manifest.identity.clone();
                                *value = serde_json::to_value(manifest)?;
                            }
                        }
                        if auth_id == Some(*id) {
                            authorized = result.is_ok();
                            auth_id = None;
                        }
                    } else if !authorized {
                        continue;
                    }
                    write_frame(&mut socket, &Frame::Message(message.to_value()))?;
                    socket.flush()?;
                }
                Input::Plugin(Frame::Media(_)) => {
                    break Err(io::Error::other("a plugin cannot send camera media"))
                }
            }
        })();
        socket.shutdown();
        drop(child_tx);
        drop(rx);
        let _ = child.kill();
        let _ = child.wait();
        let _ = child_writer.join();
        let _ = reader.join();
        let _ = plugin.join();
        result
    })();
    socket.shutdown();
    let _ = child.kill();
    let _ = child.wait();
    result
}
fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut address = None;
    let mut key = None;
    let mut command = vec![];
    let mut ready = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ready" => ready = true,
            "--listen" => address = args.next(),
            "--identity" => key = args.next().map(PathBuf::from),
            "--" => {
                command.extend(args);
                break;
            }
            _ => {
                return Err(io::Error::other(
                    "Usage: nw-analytics-bridge --listen ADDRESS --identity PATH -- COMMAND [ARGS]",
                ))
            }
        }
    }
    let address = Address::parse(&address.ok_or_else(|| io::Error::other("--listen is required"))?)
        .map_err(io::Error::other)?;
    let key = Arc::new(nw_analytics_sdk::secrets::signing_key(
        &key.ok_or_else(|| io::Error::other("--identity is required"))?,
    )?);
    if command.is_empty() {
        return Err(io::Error::other("a plugin command is required"));
    }
    let command = Arc::new(command);
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let serve = |socket: Socket| {
        use std::sync::atomic::Ordering;
        if active.fetch_add(1, Ordering::AcqRel) >= 64 {
            active.fetch_sub(1, Ordering::AcqRel);
            socket.shutdown();
            return;
        }
        let key = Arc::clone(&key);
        let command = Arc::clone(&command);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            let _ = session(socket, key, command);
            active.fetch_sub(1, Ordering::AcqRel);
        });
    };
    match address {
        Address::Tls { host, port } => {
            use std::net::ToSocketAddrs;
            let address = (host.as_str(), port)
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| io::Error::other("no address"))?;
            let listener = nw_analytics_sdk::tls::Listener::bind(address, &key)?;
            if ready {
                println!("ready {}", listener.local_addr()?);
            }
            loop {
                serve(Socket::Tls(listener.accept()?));
            }
        }
        Address::Quic { host, port } => {
            use std::net::ToSocketAddrs;
            let address = (host.as_str(), port)
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| io::Error::other("no address"))?;
            let listener = nw_analytics_sdk::quic::Listener::bind(address, &key)?;
            if ready {
                println!("ready {}", listener.local_addr()?);
            }
            loop {
                serve(Socket::Quic(listener.accept()?));
            }
        }
        Address::Tcp(address) => {
            let listener = std::net::TcpListener::bind(address)?;
            if ready {
                println!("ready {}", listener.local_addr()?);
            }
            for socket in listener.incoming() {
                let socket = socket?;
                socket.set_nodelay(true)?;
                socket.set_write_timeout(Some(Duration::from_secs(5)))?;
                serve(Socket::Tcp(Arc::new(socket)));
            }
        }
        #[cfg(unix)]
        Address::Unix(path) => {
            use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
            if let Some(parent) = path.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)?;
                let metadata = std::fs::symlink_metadata(parent)?;
                if !metadata.is_dir()
                    || metadata.mode() & 0o077 != 0
                    || metadata.uid() != unsafe { libc::geteuid() }
                {
                    return Err(io::Error::other("socket directory must be private"));
                }
            }
            if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                if !metadata.file_type().is_socket()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || std::os::unix::net::UnixStream::connect(&path).is_ok()
                {
                    return Err(io::Error::other(
                        "socket is in use or is not owned by this user",
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            if ready {
                println!("ready {}", path.display());
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            for socket in listener.incoming() {
                let socket = socket?;
                socket.set_write_timeout(Some(Duration::from_secs(5)))?;
                serve(Socket::Unix(Arc::new(socket)));
            }
        }
        #[cfg(not(unix))]
        Address::Unix(_) => return Err(io::Error::other("Unix sockets are unavailable")),
        #[cfg(windows)]
        Address::Pipe(path) => {
            let mut listener = nw_analytics_sdk::pipe::Listener::bind(&path)?;
            if ready {
                println!("ready {path}");
            }
            loop {
                serve(Socket::Pipe(listener.accept()?));
            }
        }
        #[cfg(not(windows))]
        Address::Pipe(_) => return Err(io::Error::other("Windows pipes are unavailable")),
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nw_analytics_sdk::protocol::{
        frame::{flags, MediaUnit},
        messages::Manifest,
        web::{SignedUser, UserContext},
    };
    use std::os::unix::net::UnixStream;

    /// A bridge session over a socket pair whose "plugin" is `cat`: whatever
    /// the node sends through comes straight back as the plugin's own frame,
    /// so a node-sent response stands in for the plugin's answer.
    struct Bridge {
        node: UnixStream,
        session: std::thread::JoinHandle<io::Result<()>>,
        key: Arc<SigningKey>,
    }

    impl Bridge {
        fn start() -> Self {
            let (node, bridge) = UnixStream::pair().unwrap();
            node.set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            let key = Arc::new(SigningKey::from_bytes(&[9; 32]));
            let session = {
                let key = Arc::clone(&key);
                std::thread::spawn(move || {
                    session(
                        Socket::Unix(Arc::new(bridge)),
                        key,
                        Arc::new(vec!["cat".to_owned()]),
                    )
                })
            };
            Self { node, session, key }
        }

        fn send(&mut self, message: Message) {
            write_frame(&mut self.node, &Frame::Message(message.to_value())).unwrap();
        }

        fn next(&mut self) -> Frame {
            read_frame(&mut self.node).unwrap()
        }

        fn message(&mut self) -> Message {
            let Frame::Message(value) = self.next() else {
                panic!("a message was expected");
            };
            Message::from_value(value).unwrap()
        }

        fn rejection(&mut self) -> (u64, i64, String) {
            match self.message() {
                Message::Response {
                    id,
                    result: Err(error),
                } => (id, error.code, error.message),
                other => panic!("a rejection was expected: {other:?}"),
            }
        }

        /// Hello, and the plugin's manifest signed by the bridge.
        fn greet(&mut self, challenge: &str) -> Manifest {
            self.send(Message::request(
                1,
                method::HELLO,
                serde_json::json!({
                    "protocol": 1,
                    "node": {"name": "Test", "version": "0"},
                    "challenge": challenge,
                }),
            ));
            let manifest = serde_json::json!({
                "id": "nw.bridge-test", "name": "Bridge test", "vendor": "Test", "version": "1",
                "protocol": 1, "permissions": ["video", "decoded_frames"], "locality_proof": "x",
                "media": {"video": {"stream": "main", "codecs": ["h264"]},
                          "decoded": {"formats": ["rgb24"], "max_fps": 5, "min_width": 1, "min_height": 1}},
            });
            self.send(Message::reply(1, Ok::<_, RpcError>(manifest)));
            let Message::Response {
                id: 1,
                result: Ok(value),
            } = self.message()
            else {
                panic!("the signed manifest was expected");
            };
            serde_json::from_value(value).unwrap()
        }

        fn finish(self) -> io::Result<()> {
            let _ = self.node.shutdown(std::net::Shutdown::Both);
            self.session.join().unwrap()
        }
    }

    #[test]
    fn nothing_is_forwarded_before_the_node_proves_itself() {
        let mut bridge = Bridge::start();
        bridge.send(Message::request(5, "attach", serde_json::json!({})));
        let (id, code, text) = bridge.rejection();
        assert_eq!(
            (id, code, text.as_str()),
            (5, error_code::REJECTED, "Authenticate first")
        );
        // Notifications before authentication are dropped; a ping is relayed
        // (and its echo, being no answer, is not passed back).
        bridge.send(Message::notification("event", serde_json::json!({})));
        bridge.send(Message::request(6, method::PING, serde_json::json!({})));
        // An authentication before any hello cannot verify.
        let forged = identity::authenticate(
            &SigningKey::from_bytes(&[4; 32]),
            &identity::challenge().unwrap(),
            &nw_analytics_sdk::protocol::auth::PluginIdentity {
                public_key: String::new(),
                challenge: String::new(),
                signature: String::new(),
                channel_binding: String::new(),
            },
            None,
        );
        bridge.send(Message::request(7, method::AUTHENTICATE, forged));
        let (id, code, text) = bridge.rejection();
        assert_eq!(
            (id, code, text.as_str()),
            (7, error_code::REJECTED, "Invalid node signature")
        );
        bridge.finish().unwrap();
    }

    #[test]
    fn hello_is_answered_with_a_manifest_signed_by_the_bridge_without_raw_pictures() {
        let mut bridge = Bridge::start();
        let challenge = identity::challenge().unwrap();
        let manifest = bridge.greet(&challenge);
        assert!(identity::verify_manifest(&manifest, &challenge, ""));
        assert_eq!(
            manifest.identity.as_ref().unwrap().public_key,
            identity::public_key(&bridge.key)
        );
        assert!(manifest.locality_proof.is_none());
        assert!(manifest.media.decoded.is_none());
        assert_eq!(manifest.permissions, vec!["video".to_owned()]);
        // A signature from some other key is refused.
        let identity_of_plugin = manifest.identity.clone().unwrap();
        let stranger = identity::authenticate(
            &SigningKey::from_bytes(&[4; 32]),
            "00",
            &identity_of_plugin,
            None,
        );
        bridge.send(Message::request(2, method::AUTHENTICATE, stranger));
        assert_eq!(bridge.rejection().2, "Invalid node signature");
        bridge.finish().unwrap();
    }

    #[test]
    fn an_authorized_node_reaches_the_plugin_and_only_viewers_signed_by_it_get_pages() {
        let mut bridge = Bridge::start();
        let node = SigningKey::from_bytes(&[3; 32]);
        let challenge = identity::challenge().unwrap();
        let manifest = bridge.greet(&challenge);
        let plugin_identity = manifest.identity.clone().unwrap();
        let authentication = identity::authenticate(&node, &challenge, &plugin_identity, None);
        bridge.send(Message::request(2, method::AUTHENTICATE, authentication));
        // The plugin accepts (stood in for by the node's own response).
        bridge.send(Message::reply(2, Ok::<_, RpcError>(serde_json::json!({}))));
        assert!(matches!(
            bridge.message(),
            Message::Response {
                id: 2,
                result: Ok(_)
            }
        ));

        // A page request without a node-signed viewer is refused by the bridge.
        let page =
            serde_json::json!({"path": "/", "method": "GET", "content_type": "", "body_b64": ""});
        bridge.send(Message::request(3, "web.request", page.clone()));
        assert_eq!(bridge.rejection().2, "Invalid viewer context");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let viewer: SignedUser = identity::sign_user(
            &node,
            UserContext {
                user: "ann".into(),
                role: "viewer".into(),
                cameras: vec![],
                plugin_key: identity::public_key(&bridge.key),
                expires_at: now + 30,
                nonce: identity::challenge().unwrap(),
            },
        );
        let mut signed = page;
        signed["user"] = serde_json::json!(viewer);
        bridge.send(Message::request(4, "web.read", signed.clone()));
        // Accepted, it reaches the plugin (cat) and comes back as its own request.
        assert!(matches!(
            bridge.message(),
            Message::Request { id: 4, method, .. } if method == "web.read"
        ));
        // A viewer meant for another plugin is refused.
        let mut other = signed;
        other["user"]["context"]["plugin_key"] = serde_json::json!(identity::public_key(&node));
        bridge.send(Message::request(5, "web.request", other));
        assert_eq!(bridge.rejection().2, "Invalid viewer context");
        // Ordinary requests pass through.
        bridge.send(Message::request(6, "attach", serde_json::json!({"a": 1})));
        assert!(matches!(
            bridge.message(),
            Message::Request { id: 6, method, .. } if method == "attach"
        ));
        // A plugin cannot send camera media: the session ends.
        let unit = MediaUnit {
            media: nw_analytics_sdk::protocol::frame::MediaKind::Video,
            flags: flags::KEYFRAME,
            binding: 1,
            seq: 1,
            time_us: 1,
            pts_ns: None,
            dts_ns: None,
            data: vec![1, 2, 3],
        };
        write_frame(&mut bridge.node, &Frame::Media(unit)).unwrap();
        let ended = bridge.session.join().unwrap().unwrap_err();
        assert!(ended.to_string().contains("cannot send camera media"));
    }

    #[test]
    fn a_second_hello_or_a_bad_challenge_ends_the_session() {
        let mut bridge = Bridge::start();
        bridge.send(Message::request(
            1,
            method::HELLO,
            serde_json::json!({"protocol": 1, "node": {"name": "T", "version": "0"}, "challenge": "nothex"}),
        ));
        let ended = bridge.session.join().unwrap().unwrap_err();
        assert!(ended.to_string().contains("invalid challenge"));

        let mut bridge = Bridge::start();
        let challenge = identity::challenge().unwrap();
        let hello = || {
            Message::request(
                1,
                method::HELLO,
                serde_json::json!({"protocol": 1, "node": {"name": "T", "version": "0"}, "challenge": challenge}),
            )
        };
        bridge.send(hello());
        bridge.send(hello());
        let ended = bridge.session.join().unwrap().unwrap_err();
        assert!(ended.to_string().contains("duplicate hello"));

        let mut bridge = Bridge::start();
        bridge.send(Message::request(
            1,
            method::HELLO,
            serde_json::json!({"nope": 1}),
        ));
        let ended = bridge.session.join().unwrap().unwrap_err();
        assert!(ended.to_string().contains("invalid hello"));
    }

    #[test]
    fn a_command_that_cannot_start_fails_the_session() {
        let (_node, bridge) = UnixStream::pair().unwrap();
        let result = session(
            Socket::Unix(Arc::new(bridge)),
            Arc::new(SigningKey::from_bytes(&[9; 32])),
            Arc::new(vec!["/nonexistent/nw-plugin".to_owned()]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn frames_are_read_until_the_stream_ends_and_tagged_by_origin() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Frame::Message(serde_json::json!({"a": 1}))).unwrap();
        for node in [true, false] {
            let (tx, rx) = mpsc::sync_channel(4);
            read(&bytes[..], tx, node);
            match (rx.recv().unwrap(), node) {
                (Input::Node(Frame::Message(_)), true)
                | (Input::Plugin(Frame::Message(_)), false) => {}
                _ => panic!("frame tagged with the wrong origin"),
            }
            assert!(matches!(rx.recv().unwrap(), Input::Closed));
        }
        // A reader whose receiver is gone stops quietly.
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        read(&bytes[..], tx, true);
    }

    #[test]
    fn tcp_sockets_read_write_and_shut_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut near = Socket::Tcp(Arc::new(client));
        let mut far = Socket::Tcp(Arc::new(server));
        assert_eq!(near.binding(), "");
        near.write_all(b"ping").unwrap();
        near.flush().unwrap();
        let mut got = [0u8; 4];
        far.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");
        near.shutdown();
        let mut rest = Vec::new();
        far.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }
}
