//! The RTSP port check: finds cameras that answer neither WS-Discovery nor
//! SADP (ONVIF off, multicast filtered) by a TCP connection to their RTSP
//! port, and reads the `Server` header of one `OPTIONS` reply for a hint of
//! the maker. Nothing else is sent and no stream is opened.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const RTSP_PORT: u16 = 554;
/// Port checks running at the same time.
pub const CONCURRENCY: usize = 64;
/// A LAN device answers a connection within a few milliseconds; an address
/// without a device does not answer at all.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const REPLY_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_REPLY_BYTES: usize = 4096;

/// What a port check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspDevice {
    pub address: Ipv4Addr,
    pub port: u16,
    /// The `Server` header of its `OPTIONS` reply, if it sent one.
    pub server: Option<String>,
}

/// Does `address` accept a connection on `port`, and answer like an RTSP
/// server (or not at all, within a short time)?
pub async fn check(address: Ipv4Addr, port: u16) -> Option<RtspDevice> {
    let target = SocketAddr::from((address, port));
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(target))
        .await
        .ok()?
        .ok()?;
    let request = format!(
        "OPTIONS rtsp://{address}:{port}/ RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: NeuroWatch\r\n\r\n"
    );
    let mut reply = Vec::new();
    let read = async {
        stream.write_all(request.as_bytes()).await?;
        let mut buf = [0u8; 1024];
        while reply.len() < MAX_REPLY_BYTES {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            reply.extend_from_slice(&buf[..n]);
            if reply.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        std::io::Result::Ok(())
    };
    let _ = tokio::time::timeout(REPLY_TIMEOUT, read).await;
    let text = String::from_utf8_lossy(&reply);
    if !text.is_empty() && !text.starts_with("RTSP/") {
        // Something else listens on the port.
        return None;
    }
    let server = text
        .lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("server")
                .then(|| value.trim())
        })
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().filter(|c| !c.is_control()).take(64).collect());
    Some(RtspDevice {
        address,
        port,
        server,
    })
}

/// The maker an RTSP `Server` header names, when NeuroWatch knows its
/// stream paths.
pub fn vendor_of(server: &str) -> Option<&'static str> {
    let lowered = server.to_ascii_lowercase();
    if lowered.contains("hikvision") {
        Some("Hikvision")
    } else if lowered.contains("dahua") {
        Some("Dahua")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn server(reply: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        port
    }

    #[tokio::test]
    async fn an_rtsp_server_is_found_with_its_server_header() {
        let port = server("RTSP/1.0 200 OK\r\nCSeq: 1\r\nServer: Dahua Rtsp Server/3.0\r\nPublic: OPTIONS, DESCRIBE\r\n\r\n").await;
        let found = check(Ipv4Addr::LOCALHOST, port).await.expect("found");
        assert_eq!(found.server.as_deref(), Some("Dahua Rtsp Server/3.0"));
        assert_eq!(vendor_of("Dahua Rtsp Server/3.0"), Some("Dahua"));
        assert_eq!(vendor_of("HIKVISION RTSP"), Some("Hikvision"));
        assert_eq!(vendor_of("GStreamer RTSP server"), None);
    }

    #[tokio::test]
    async fn a_web_server_or_a_closed_port_is_not_an_rtsp_device() {
        let web = server("HTTP/1.1 400 Bad Request\r\n\r\n").await;
        assert!(check(Ipv4Addr::LOCALHOST, web).await.is_none());
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        assert!(check(Ipv4Addr::LOCALHOST, closed).await.is_none());
    }
}
