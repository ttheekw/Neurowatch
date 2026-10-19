//! Receiving cameras' events pushed to NeuroWatch (WS-BaseNotification).
//!
//! A camera subscribed in push mode sends each event as an HTTP POST
//! (`wsnt:Notify`) to the address NeuroWatch gave it. One small listener on
//! `events.push_port` serves every camera. Each subscription gets its own
//! unguessable path, and a delivery is accepted only on that path and only
//! from that camera's address, so nobody else on the network can inject
//! events. Bodies are size-capped and read with a deadline.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Largest delivery read (headers and body).
const MAX_REQUEST: usize = 256 * 1024;
/// A delivery must arrive whole within this time.
const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Deliveries queued per subscription before more are dropped.
const QUEUE: usize = 64;
const PATH_PREFIX: &str = "/onvif-events/";

struct Route {
    camera: IpAddr,
    deliveries: mpsc::Sender<String>,
}

/// The listener and the subscriptions it delivers to.
pub struct PushListener {
    port: u16,
    routes: Mutex<HashMap<String, Route>>,
}

/// A subscription's address and the deliveries made to it. Dropping it
/// stops accepting deliveries for it.
pub struct PushRoute {
    listener: Arc<PushListener>,
    token: String,
    pub url: String,
    pub deliveries: mpsc::Receiver<String>,
}

impl Drop for PushRoute {
    fn drop(&mut self) {
        if let Ok(mut routes) = self.listener.routes.lock() {
            routes.remove(&self.token);
        }
    }
}

impl PushListener {
    /// Listen on every interface at `port`; `None` when the port cannot be
    /// used (taken, or not allowed), and cameras' events are then pulled.
    pub async fn start(port: u16) -> Option<Arc<Self>> {
        let listener = match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::warn!(port, %error, "Camera events: cannot listen for pushed events; asking cameras for them instead");
                return None;
            }
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        tracing::info!(port, "Camera events: listening for events cameras push");
        let this = Arc::new(Self {
            port,
            routes: Mutex::new(HashMap::new()),
        });
        let accepting = Arc::clone(&this);
        tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(_) => {
                        // Out of file handles or similar: don't spin.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let this = Arc::clone(&accepting);
                tokio::spawn(async move {
                    let _ =
                        tokio::time::timeout(READ_DEADLINE, this.serve(stream, peer.ip())).await;
                });
            }
        });
        Some(this)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// A new address for one camera's subscription, reachable from `camera`
    /// (`None` if no local address routes to it).
    pub fn route(self: &Arc<Self>, camera: IpAddr) -> Option<PushRoute> {
        let local = local_address_towards(camera)?;
        let mut token = [0u8; 16];
        getrandom::getrandom(&mut token).ok()?;
        let token: String = token.iter().map(|b| format!("{b:02x}")).collect();
        let (deliver, deliveries) = mpsc::channel(QUEUE);
        self.routes.lock().ok()?.insert(
            token.clone(),
            Route {
                camera,
                deliveries: deliver,
            },
        );
        let host = match local {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        Some(PushRoute {
            listener: Arc::clone(self),
            url: format!("http://{host}:{}{PATH_PREFIX}{token}", self.port),
            token,
            deliveries,
        })
    }

    async fn serve(&self, mut stream: TcpStream, peer: IpAddr) {
        let status = match read_request(&mut stream).await {
            Err(status) => status,
            Ok((path, body)) => self.deliver(&path, peer, body),
        };
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            413 => "Payload Too Large",
            _ => "Service Unavailable",
        };
        let reply =
            format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    fn deliver(&self, path: &str, peer: IpAddr, body: String) -> u16 {
        let Some(token) = path.strip_prefix(PATH_PREFIX) else {
            return 404;
        };
        let Ok(routes) = self.routes.lock() else {
            return 503;
        };
        let Some(route) = routes.get(token) else {
            return 404;
        };
        if canonical(route.camera) != canonical(peer) {
            tracing::warn!(%peer, "Camera events: a delivery came from another address; refused");
            return 403;
        }
        match route.deliveries.try_send(body) {
            Ok(()) => 200,
            // Behind: the camera retries or the next event carries the state.
            Err(_) => 503,
        }
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// The local address the system would use to reach `camera`. No packet is
/// sent: connecting a UDP socket only picks the route.
pub fn local_address_towards(camera: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = match camera {
        IpAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        IpAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = std::net::UdpSocket::bind(bind).ok()?;
    socket.connect((camera, 9)).ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// `(path, body)` of a POST, or the HTTP status to answer.
async fn read_request(stream: &mut TcpStream) -> Result<(String, String), u16> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if read == 0 {
            return Err(400);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            break end;
        }
        if buffer.len() > MAX_REQUEST {
            return Err(413);
        }
    };
    let head = std::str::from_utf8(&buffer[..header_end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or("").split(' ');
    if request.next() != Some("POST") {
        return Err(400);
    }
    let path = request.next().ok_or(400u16)?.to_owned();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .ok_or(400u16)?;
    if length > MAX_REQUEST {
        return Err(413);
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if read == 0 {
            return Err(400);
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok((path, String::from_utf8_lossy(&body).into_owned()))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    async fn post(port: u16, path: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.expect("read");
        reply
    }

    #[tokio::test]
    async fn deliveries_reach_only_their_route_from_their_camera() {
        let listener = PushListener::start(0).await.expect("listening");
        let port = listener.port();
        let mut route = listener
            .route(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .expect("route");
        let path = route
            .url
            .split_once(&format!(":{port}"))
            .unwrap()
            .1
            .to_owned();
        assert!(route.url.starts_with("http://127.0.0.1:"), "{}", route.url);

        assert!(post(port, &path, "<Notify/>")
            .await
            .starts_with("HTTP/1.1 200"));
        assert_eq!(route.deliveries.recv().await.as_deref(), Some("<Notify/>"));
        assert!(post(port, "/onvif-events/nope", "x")
            .await
            .starts_with("HTTP/1.1 404"));
        assert!(post(port, "/other", "x").await.starts_with("HTTP/1.1 404"));

        // A route for another camera refuses this address.
        let other = listener
            .route(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)))
            .expect("route");
        let other_path = other
            .url
            .split_once(&format!(":{port}"))
            .unwrap()
            .1
            .to_owned();
        assert!(post(port, &other_path, "x")
            .await
            .starts_with("HTTP/1.1 403"));

        drop(route);
        assert!(
            post(port, &path, "x").await.starts_with("HTTP/1.1 404"),
            "gone with its route"
        );
    }

    #[tokio::test]
    async fn oversized_and_malformed_requests_are_refused() {
        let listener = PushListener::start(0).await.expect("listening");
        let port = listener.port();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(b"POST /onvif-events/x HTTP/1.1\r\nContent-Length: 999999999\r\n\r\n")
            .await
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    }

    #[test]
    fn the_local_address_towards_loopback_is_loopback() {
        assert_eq!(
            local_address_towards(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }
}
