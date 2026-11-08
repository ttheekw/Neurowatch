//! Unix descriptor capabilities alongside the ordinary length-prefixed wire.
//! Descriptors are matched by an immutable random ring id, not arrival order:
//! buffered reads may receive the next notification's descriptor early.
use crate::ring::descriptor_id;
use nw_analytics_protocol::frame::{write_frame, Frame};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

const MAX_HANDLES: usize = 128;

#[derive(Clone, Default)]
pub struct DescriptorInbox {
    files: Arc<Mutex<VecDeque<File>>>,
}

impl DescriptorInbox {
    fn received(&self, files: Vec<File>) -> io::Result<()> {
        let mut pending = self
            .files
            .lock()
            .map_err(|_| io::Error::other("descriptor inbox unavailable"))?;
        if pending.len() + files.len() > MAX_HANDLES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many shared-memory offers",
            ));
        }
        pending.extend(files);
        Ok(())
    }
    pub fn take(&self, id: &str) -> Option<File> {
        let mut pending = self.files.lock().ok()?;
        let mut found = None;
        let mut retained = VecDeque::new();
        while let Some(file) = pending.pop_front() {
            match descriptor_id(&file) {
                Ok(known) if known == id && found.is_none() => found = Some(file),
                Ok(_) => retained.push_back(file),
                Err(_) => {} // reject non-ring descriptors without ever reading arbitrary devices
            }
        }
        *pending = retained;
        found
    }
}

pub struct LocalReader {
    socket: UnixStream,
    inbox: DescriptorInbox,
}

impl LocalReader {
    pub fn new(socket: UnixStream) -> (Self, DescriptorInbox) {
        let inbox = DescriptorInbox::default();
        (
            Self {
                socket,
                inbox: inbox.clone(),
            },
            inbox,
        )
    }
}

impl Read for LocalReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut vector = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // usize alignment meets cmsghdr's alignment. Enough ancillary room
        // for the full bounded inbox, even if the kernel coalesces sendmsg calls.
        let mut control = [0usize; 256];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control) as _;
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
        let flags = 0;
        let read = unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut message, flags) };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut files = Vec::new();
        let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        while !header.is_null() {
            let ancillary = unsafe { &*header };
            if ancillary.cmsg_level == libc::SOL_SOCKET && ancillary.cmsg_type == libc::SCM_RIGHTS {
                let base = unsafe { libc::CMSG_LEN(0) } as usize;
                if ancillary.cmsg_len as usize >= base {
                    let count = (ancillary.cmsg_len as usize - base) / std::mem::size_of::<RawFd>();
                    let descriptors = unsafe {
                        std::slice::from_raw_parts(libc::CMSG_DATA(header).cast::<RawFd>(), count)
                    };
                    for descriptor in descriptors {
                        // recvmsg transferred each descriptor exactly once to us.
                        let file = unsafe { File::from_raw_fd(*descriptor) };
                        files.push(file);
                    }
                }
            }
            header = unsafe { libc::CMSG_NXTHDR(&message, header) };
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated descriptor capabilities",
            ));
        }
        // Own every received fd before any fallible operation: an error
        // closes the whole batch, including descriptors after the failed one.
        for file in &files {
            let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        self.inbox.received(files)?;
        Ok(read as usize)
    }
}

/// Write one frame carrying a read-only descriptor. Flush buffered bytes before
/// calling this, so the descriptor cannot be attached to another frame.
pub fn write_descriptor(socket: &UnixStream, frame: &Frame, file: &File) -> io::Result<()> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame)?;
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0usize; 8];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as _;
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null() {
        return Err(io::Error::other("descriptor header unavailable"));
    }
    unsafe {
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write(libc::CMSG_DATA(header).cast::<RawFd>(), file.as_raw_fd());
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    let flags = 0;
    let sent = loop {
        let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &message, flags) };
        if sent >= 0 {
            break sent as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    if sent == 0 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "shared-memory connection closed",
        ));
    }
    let mut remaining = socket;
    remaining.write_all(&bytes[sent..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{PictureIdentity, RawFormat, RawLayout, RingReader, RingWriter};
    use nw_analytics_protocol::frame::read_frame;
    use serde_json::json;

    #[test]
    fn a_readonly_capability_can_prove_locality_and_survives_buffered_notification_reads() {
        let directory = std::env::temp_dir().join(format!("nw-sdk-local-{}", std::process::id()));
        let layout = RawLayout {
            width: 4,
            height: 1,
            format: RawFormat::Gray8,
            color: None,
        };
        let mut a = RingWriter::create(&directory, layout).unwrap();
        let mut b = RingWriter::create(&directory, layout).unwrap();
        a.write(
            PictureIdentity {
                seq: 1,
                time_us: 10,
                pts_ns: None,
            },
            [&[1, 2, 3, 4], &[]],
            [4, 0],
        )
        .unwrap();
        b.write(
            PictureIdentity {
                seq: 2,
                time_us: 20,
                pts_ns: None,
            },
            [&[5, 6, 7, 8], &[]],
            [4, 0],
        )
        .unwrap();
        let (sender, receiver) = UnixStream::pair().unwrap();
        let (reader, inbox) = LocalReader::new(receiver);
        let mut reader = std::io::BufReader::new(reader);
        for ring in [&a, &b] {
            write_descriptor(
                &sender,
                &Frame::Message(json!({"ring":ring.id()})),
                &ring.descriptor().unwrap(),
            )
            .unwrap();
        }
        assert!(matches!(
            read_frame(&mut reader).unwrap(),
            Frame::Message(_)
        ));
        assert!(matches!(
            read_frame(&mut reader).unwrap(),
            Frame::Message(_)
        ));
        // Match by id in reverse order, not by the order recvmsg delivered fds.
        let mut other = RingReader::open(inbox.take(b.id()).unwrap(), b.id(), b.bytes()).unwrap();
        assert_eq!(other.latest().unwrap().data, [5, 6, 7, 8]);
        let mut first = RingReader::open(inbox.take(a.id()).unwrap(), a.id(), a.bytes()).unwrap();
        assert_eq!(first.latest().unwrap().data, [1, 2, 3, 4]);
        assert!(inbox.take(a.id()).is_none());
        let _ = std::fs::remove_dir(directory);
    }
}
