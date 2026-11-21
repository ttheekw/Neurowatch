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
/// Time a node has to authenticate before its session (and the plugin process
/// started for it) is dropped.
const AUTH_WINDOW: Duration = Duration::from_secs(15);
/// Sessions that started a plugin process and have not authenticated yet. A
/// plugin is started only by a well-formed hello, and only this many at once.
const MAX_UNAUTHENTICATED: usize = 8;
static UNAUTHENTICATED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Count one more unauthenticated session unless `max` already are.
fn admit(counter: &std::sync::atomic::AtomicUsize, max: usize) -> bool {
    use std::sync::atomic::Ordering;
    if counter.fetch_add(1, Ordering::AcqRel) >= max {
        counter.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    true
}
/// The plugin process of one session, with its writer and reader threads.
struct Plugin {
    child: std::process::Child,
    tx: mpsc::SyncSender<Frame>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// Still counted in [`UNAUTHENTICATED`].
    pending: bool,
}
impl Plugin {
    fn start(command: &[String], events: &mpsc::SyncSender<Input>) -> io::Result<Self> {
        use std::sync::atomic::Ordering;
        if !admit(&UNAUTHENTICATED, MAX_UNAUTHENTICATED) {
            return Err(io::Error::other("too many unauthenticated sessions"));
        }
        let started = Self::spawn(command, events, true);
        if started.is_err() {
            UNAUTHENTICATED.fetch_sub(1, Ordering::AcqRel);
        }
        started
    }
    /// Start the process and its threads, counted nowhere.
    fn spawn(
        command: &[String],
        events: &mpsc::SyncSender<Input>,
        pending: bool,
    ) -> io::Result<Self> {
        (|| {
            let mut child = Command::new(&command[0])
                .args(&command[1..])
                .env("NW_ANALYTICS_SESSION", "1")
                .env("NW_ANALYTICS_BRIDGE_PID", std::process::id().to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("child pipes missing"));
            };
            let (tx, child_rx) = mpsc::sync_channel(4);
            let writer = std::thread::spawn(move || {
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
            let events = events.clone();
            let reader = std::thread::spawn(move || read(output, events, false));
            Ok(Self {
                child,
                tx,
                threads: vec![writer, reader],
                pending,
            })
        })()
    }
    /// The node proved itself: the plugin no longer counts against the limit.
    fn authenticated(&mut self) {
        if std::mem::take(&mut self.pending) {
            UNAUTHENTICATED.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    fn stop(mut self) {
        self.authenticated();
        let _ = self.child.kill();
        let _ = self.child.wait();
        drop(self.tx);
        for thread in self.threads {
            let _ = thread.join();
        }
    }
}
/// What the bridge tells every node about the plugin: its manifest without
/// the pieces the Python transport cannot deliver (raw pictures, locality).
fn prepare(mut manifest: Manifest) -> Manifest {
    // The Python transport delivers compressed media. Raw-picture capability
    // forwarding is available in the native Rust SDK.
    manifest.locality_proof = None;
    manifest.identity = None;
    manifest.media.decoded = None;
    manifest
        .permissions
        .retain(|permission| permission != "decoded_frames");
    manifest
}
/// Time the plugin has to answer the bridge's own hello at start-up.
const MANIFEST_WAIT: Duration = Duration::from_secs(30);
/// Start the plugin once and learn its manifest, so that answering a node's
/// `hello` never needs a plugin process: only an authenticated node does. The
/// answer does not depend on the node's challenge (the bridge signs it per
/// connection with its own key, and drops the plugin's identity and locality
/// proof), so it can be cached for the bridge's life.
fn learn_manifest(command: &[String]) -> io::Result<Manifest> {
    let (tx, rx) = mpsc::sync_channel(8);
    let plugin = Plugin::spawn(command, &tx, false)?;
    let learned = (|| {
        let hello = serde_json::json!({
            "protocol": nw_analytics_sdk::protocol::PROTOCOL_VERSION,
            "node": {"name": "bridge", "version": env!("CARGO_PKG_VERSION")},
            "challenge": identity::challenge().map_err(io::Error::other)?,
        });
        plugin
            .tx
            .send(Frame::Message(
                Message::request(1, method::HELLO, hello).to_value(),
            ))
            .map_err(|_| io::Error::other("the plugin stopped before it answered"))?;
        let until = std::time::Instant::now() + MANIFEST_WAIT;
        loop {
            let left = until
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| io::Error::other("the plugin did not answer the hello"))?;
            match rx.recv_timeout(left) {
                Ok(Input::Plugin(Frame::Message(value))) => {
                    if let Message::Response { id: 1, result } =
                        Message::from_value(value).map_err(io::Error::other)?
                    {
                        let value = result.map_err(|error| {
                            io::Error::other(format!("hello refused: {error:?}"))
                        })?;
                        return params::<Manifest>(value)
                            .map_err(|_| io::Error::other("invalid plugin manifest"));
                    }
                }
                Ok(Input::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("the plugin stopped before it answered"))
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::other("the plugin did not answer the hello"))
                }
            }
        }
    })();
    drop(rx);
    plugin.stop();
    learned.map(prepare)
}
/// Pass a frame to the plugin, when one was started (a ping before the hello
/// has nothing to reach and is dropped).
fn forward(plugin: &Option<Plugin>, frame: Frame) -> io::Result<()> {
    match plugin {
        Some(plugin) => plugin
            .tx
            .try_send(frame)
            .map_err(|_| io::Error::other("Python session stopped reading")),
        None => Ok(()),
    }
}
fn session(
    socket: Socket,
    key: Arc<SigningKey>,
    command: Arc<Vec<String>>,
    manifest: Arc<Manifest>,
) -> io::Result<()> {
    session_within(socket, key, command, manifest, AUTH_WINDOW)
}
fn session_within(
    mut socket: Socket,
    key: Arc<SigningKey>,
    command: Arc<Vec<String>>,
    manifest: Arc<Manifest>,
    auth_window: Duration,
) -> io::Result<()> {
    let opened = std::time::Instant::now();
    let (tx, rx) = mpsc::sync_channel(4);
    let reader = {
        let tx = tx.clone();
        let socket = socket.clone();
        std::thread::spawn(move || read(socket, tx, true))
    };
    let mut plugin: Option<Plugin> = None;
    {
        let mut hello: Option<(u64, String)> = None;
        // The hello frame, handed to the plugin once the node has authenticated.
        let mut hello_frame: Option<serde_json::Value> = None;
        let mut plugin_hello: Option<u64> = None;
        let mut signed = None;
        let mut auth_id = None;
        let mut authorized = false;
        let mut node_key: Option<String> = None;
        let result = (|| loop {
            // A node that has not proven itself in time is dropped, and with it
            // the plugin process its hello started.
            let wait = if authorized {
                Duration::from_secs(30)
            } else {
                match auth_window.checked_sub(opened.elapsed()) {
                    Some(left) => left.min(Duration::from_secs(30)),
                    None => break Err(io::Error::other("not authenticated in time")),
                }
            };
            let received = match rx.recv_timeout(wait) {
                Ok(input) => input,
                Err(mpsc::RecvTimeoutError::Timeout) if !authorized => continue,
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
                            if greeting.protocol != manifest.protocol {
                                write_frame(
                                    &mut socket,
                                    &Frame::Message(
                                        Message::reply(
                                            *id,
                                            Err::<(), _>(RpcError::new(
                                                error_code::UNSUPPORTED_PROTOCOL,
                                                "Unsupported protocol version",
                                            )),
                                        )
                                        .to_value(),
                                    ),
                                )?;
                                continue;
                            }
                            // Answered from what the bridge learned at start-up:
                            // no plugin process runs for a node that has not
                            // authenticated.
                            let mut answer = (*manifest).clone();
                            identity::sign_manifest(
                                &mut answer,
                                &key,
                                &greeting.challenge,
                                socket.binding(),
                            )
                            .map_err(io::Error::other)?;
                            signed = answer.identity.clone();
                            hello = Some((*id, greeting.challenge));
                            hello_frame = Some(value.clone());
                            write_frame(
                                &mut socket,
                                &Frame::Message(
                                    Message::reply(*id, Ok::<_, RpcError>(answer)).to_value(),
                                ),
                            )?;
                            socket.flush()?;
                            continue;
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
                            // Only now does the plugin start, and it hears the
                            // hello the node already had answered.
                            if plugin.is_none() {
                                let started = Plugin::start(&command, &tx)?;
                                if let Some((hello_id, _)) = &hello {
                                    plugin_hello = Some(*hello_id);
                                }
                                if let Some(frame) = hello_frame.take() {
                                    started
                                        .tx
                                        .try_send(Frame::Message(frame))
                                        .map_err(|_| io::Error::other("plugin did not start"))?;
                                }
                                plugin = Some(started);
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
                    forward(&plugin, Frame::Message(value))?;
                }
                Input::Node(frame) => {
                    if authorized {
                        forward(&plugin, frame)?;
                    }
                }
                Input::Plugin(Frame::Message(value)) => {
                    let mut message = Message::from_value(value).map_err(io::Error::other)?;
                    if let Message::Response { id, result } = &mut message {
                        // The plugin's own answer to the replayed hello was
                        // answered for it already.
                        if plugin_hello == Some(*id) {
                            plugin_hello = None;
                            if result.is_err() {
                                break Err(io::Error::other("the plugin refused the hello"));
                            }
                            continue;
                        }
                        if auth_id == Some(*id) {
                            authorized = result.is_ok();
                            auth_id = None;
                            if authorized {
                                if let Some(plugin) = plugin.as_mut() {
                                    plugin.authenticated();
                                }
                            }
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
        drop(rx);
        if let Some(plugin) = plugin.take() {
            plugin.stop();
        }
        let _ = reader.join();
        result
    }
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
    let manifest = Arc::new(learn_manifest(&command)?);
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
        let manifest = Arc::clone(&manifest);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            let _ = session(socket, key, command, manifest);
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

    /// What the bridge learned from the plugin at start-up (raw, before `prepare`).
    fn learned() -> Arc<Manifest> {
        Arc::new(prepare(
            serde_json::from_value(serde_json::json!({
                "id": "nw.bridge-test", "name": "Bridge test", "vendor": "Test", "version": "1",
                "protocol": 1, "permissions": ["video", "decoded_frames"], "locality_proof": "x",
                "media": {"video": {"stream": "main", "codecs": ["h264"]},
                          "decoded": {"formats": ["rgb24"], "max_fps": 5, "min_width": 1, "min_height": 1}},
            }))
            .unwrap(),
        ))
    }

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
                        learned(),
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

        /// Hello, answered by the bridge itself with the manifest it learned,
        /// signed for this connection.
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
        // Notifications before authentication are dropped; a ping before the
        // hello has no plugin to reach (none is running yet).
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

    /// A node says hello over `node` and reads the bridge's own answer.
    fn say_hello(node: &mut UnixStream, challenge: &str) -> Manifest {
        write_frame(
            node,
            &Frame::Message(
                Message::request(
                    1,
                    method::HELLO,
                    serde_json::json!({"protocol": 1, "node": {"name": "T", "version": "0"},
                        "challenge": challenge}),
                )
                .to_value(),
            ),
        )
        .unwrap();
        node.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let Frame::Message(value) = read_frame(node).unwrap() else {
            panic!("a message was expected");
        };
        let Message::Response {
            id: 1,
            result: Ok(value),
        } = Message::from_value(value).unwrap()
        else {
            panic!("the signed manifest was expected");
        };
        serde_json::from_value(value).unwrap()
    }

    /// A correctly signed `authenticate` for the manifest the bridge sent.
    fn authenticate(node: &mut UnixStream, challenge: &str, manifest: &Manifest) {
        let proof = identity::authenticate(
            &SigningKey::from_bytes(&[3; 32]),
            challenge,
            manifest.identity.as_ref().unwrap(),
            None,
        );
        write_frame(
            node,
            &Frame::Message(Message::request(2, method::AUTHENTICATE, proof).to_value()),
        )
        .unwrap();
    }

    #[test]
    fn a_command_that_cannot_start_fails_the_session() {
        let (mut node, bridge) = UnixStream::pair().unwrap();
        let session = std::thread::spawn(move || {
            session(
                Socket::Unix(Arc::new(bridge)),
                Arc::new(SigningKey::from_bytes(&[9; 32])),
                Arc::new(vec!["/nonexistent/nw-plugin".to_owned()]),
                learned(),
            )
        });
        let challenge = identity::challenge().unwrap();
        let manifest = say_hello(&mut node, &challenge);
        authenticate(&mut node, &challenge, &manifest);
        assert!(session.join().unwrap().is_err());
    }

    /// Vulnerability checklist, Motion/Detector/SDK Low: a peer that has not
    /// authenticated (even one that says hello) never starts a plugin process.
    #[test]
    fn the_plugin_starts_only_when_a_node_has_authenticated() {
        let marker = std::env::temp_dir().join(format!("nw-bridge-start-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let command = Arc::new(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("touch {}; exec cat", marker.display()),
        ]);
        let (mut node, bridge) = UnixStream::pair().unwrap();
        let session = std::thread::spawn(move || {
            session(
                Socket::Unix(Arc::new(bridge)),
                Arc::new(SigningKey::from_bytes(&[9; 32])),
                command,
                learned(),
            )
        });
        write_frame(
            &mut node,
            &Frame::Message(Message::request(5, "attach", serde_json::json!({})).to_value()),
        )
        .unwrap();
        let _ = read_frame(&mut node).unwrap();
        let challenge = identity::challenge().unwrap();
        let manifest = say_hello(&mut node, &challenge);
        // A forged authentication does not start it either.
        let forged = identity::authenticate(
            &SigningKey::from_bytes(&[4; 32]),
            "00",
            manifest.identity.as_ref().unwrap(),
            None,
        );
        write_frame(
            &mut node,
            &Frame::Message(Message::request(3, method::AUTHENTICATE, forged).to_value()),
        )
        .unwrap();
        let _ = read_frame(&mut node).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists(), "no plugin before the node authenticated");
        authenticate(&mut node, &challenge, &manifest);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists(), "an authenticated node starts the plugin");
        let _ = node.shutdown(std::net::Shutdown::Both);
        let _ = session.join().unwrap();
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn the_manifest_is_learned_once_from_the_plugin() {
        // A plugin that answers the hello with a manifest (here `sh` writing a frame).
        let manifest = serde_json::json!({
            "id": "nw.learned", "name": "L", "vendor": "T", "version": "1", "protocol": 1,
            "permissions": ["video", "decoded_frames"], "locality_proof": "x",
            "media": {"video": {"codecs": ["h264"]}},
        });
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            &Frame::Message(Message::reply(1, Ok::<_, RpcError>(manifest)).to_value()),
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("nw-bridge-manifest-{}", std::process::id()));
        std::fs::write(&path, &frame).unwrap();
        let learned = learn_manifest(&[
            "sh".to_owned(),
            "-c".to_owned(),
            format!("cat {}; exec cat >/dev/null", path.display()),
        ])
        .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(learned.id, "nw.learned");
        assert!(learned.locality_proof.is_none());
        assert_eq!(learned.permissions, vec!["video".to_owned()]);
        // A plugin that cannot start is an error at start-up.
        assert!(learn_manifest(&["/nonexistent/nw-plugin".to_owned()]).is_err());
    }

    #[test]
    fn a_node_that_never_authenticates_is_dropped() {
        let (node, bridge) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        let ended = session_within(
            Socket::Unix(Arc::new(bridge)),
            Arc::new(SigningKey::from_bytes(&[9; 32])),
            Arc::new(vec!["cat".to_owned()]),
            learned(),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(ended.to_string().contains("not authenticated in time"));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(node);
    }

    #[test]
    fn unauthenticated_sessions_are_limited() {
        let counter = std::sync::atomic::AtomicUsize::new(0);
        assert!((0..3).all(|_| admit(&counter, 3)));
        assert!(!admit(&counter, 3));
        assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 3);
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
