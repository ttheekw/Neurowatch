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
