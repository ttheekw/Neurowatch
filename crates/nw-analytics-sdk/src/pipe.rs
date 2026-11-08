//! Local-only Windows pipes. Overlapped I/O lets shutdown cancel a blocked
//! reader/writer; handle capabilities are duplicated from the OS-verified peer.
use std::{
    fs::File,
    io::{self, Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    runtime::Runtime,
    sync::Notify,
};
use windows_sys::Win32::{
    Foundation::DuplicateHandle,
    Storage::FileSystem::{FILE_GENERIC_READ, SECURITY_IDENTIFICATION},
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE},
    },
};

enum Io {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}
impl Io {
    fn handle(&self) -> RawHandle {
        match self {
            Self::Client(p) => p.as_raw_handle(),
            Self::Server(p) => p.as_raw_handle(),
        }
    }
    async fn readable(&self) -> io::Result<()> {
        match self {
            Self::Client(p) => p.readable().await,
            Self::Server(p) => p.readable().await,
        }
    }
    async fn writable(&self) -> io::Result<()> {
        match self {
            Self::Client(p) => p.writable().await,
            Self::Server(p) => p.writable().await,
        }
    }
    fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Client(p) => p.try_read(bytes),
            Self::Server(p) => p.try_read(bytes),
        }
    }
    fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Client(p) => p.try_write(bytes),
            Self::Server(p) => p.try_write(bytes),
        }
    }
}
struct Shared {
    pipe: Io,
    runtime: Arc<Runtime>,
    peer: OwnedHandle,
    closed: AtomicBool,
    wake: Notify,
}
#[derive(Clone)]
pub struct Pipe(Arc<Shared>);
impl Pipe {
    fn new(pipe: Io, runtime: Arc<Runtime>) -> io::Result<Self> {
        let mut pid = 0;
        let ok = unsafe {
            match &pipe {
                Io::Client(_) => GetNamedPipeServerProcessId(pipe.handle(), &mut pid),
                Io::Server(_) => GetNamedPipeClientProcessId(pipe.handle(), &mut pid),
            }
        };
        if ok == 0 || pid == 0 {
            return Err(io::Error::last_os_error());
        }
        let peer = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
        if peer.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Arc::new(Shared {
            pipe,
            runtime,
            peer: unsafe { OwnedHandle::from_raw_handle(peer) },
            closed: AtomicBool::new(false),
            wake: Notify::new(),
        })))
    }
    pub fn connect(name: &str) -> io::Result<Self> {
        validate(name)?;
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?,
        );
        let pipe = {
            let _guard = runtime.enter();
            ClientOptions::new()
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(name)?
        };
        Self::new(Io::Client(pipe), runtime)
    }
    pub fn shutdown(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.wake.notify_waiters();
    }
    /// The number is a handle in the peer process, never an untrusted handle
    /// in this process. Request only read rights when duplicating it.
    pub fn descriptor(&self, number: u64) -> io::Result<File> {
        let mut local = std::ptr::null_mut();
        let remote =
            usize::try_from(number).map_err(|_| io::Error::other("invalid handle"))? as RawHandle;
        if unsafe {
            DuplicateHandle(
                self.0.peer.as_raw_handle(),
                remote,
                GetCurrentProcess(),
                &mut local,
                FILE_GENERIC_READ,
                0,
                0,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(local) })
    }
}
fn validate(name: &str) -> io::Result<()> {
    match nw_analytics_protocol::address::Address::parse(&format!("pipe:{name}")) {
        Ok(nw_analytics_protocol::address::Address::Pipe(_)) => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a local pipe",
        )),
    }
}
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.0.runtime.block_on(async {
            loop {
                let notified = self.0.wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.0.closed.load(Ordering::Acquire) { return Ok(0); }
                tokio::select! { _ = &mut notified => return Ok(0), result = self.0.pipe.readable() => result? }
                match self.0.pipe.read(bytes) { Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue, result => return result }
            }
        })
    }
}
impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let notified = self.0.wake.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.0.closed.load(Ordering::Acquire) { return Err(io::Error::from(io::ErrorKind::BrokenPipe)); }
                    tokio::select! { _ = &mut notified => return Err(io::Error::from(io::ErrorKind::BrokenPipe)), result = self.0.pipe.writable() => result? }
                    match self.0.pipe.write(bytes) { Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue, result => return result }
                }
            }).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub struct Listener {
    name: String,
    next: NamedPipeServer,
    runtime: Arc<Runtime>,
}
impl Listener {
    pub fn bind(name: &str) -> io::Result<Self> {
        validate(name)?;
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?,
        );
        let next = {
            let _guard = runtime.enter();
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(name)?
        };
        Ok(Self {
            name: name.into(),
            next,
            runtime,
        })
    }
    pub fn accept(&mut self) -> io::Result<Pipe> {
        self.runtime.block_on(self.next.connect())?;
        let next = {
            let _guard = self.runtime.enter();
            ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?
        };
        Pipe::new(
            Io::Server(std::mem::replace(&mut self.next, next)),
            Arc::clone(&self.runtime),
        )
    }
}
