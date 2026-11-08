//! Where a plugin listens.
use std::fmt;
use std::path::PathBuf;

/// `unix:/path/to.sock` (same machine) or `tcp:127.0.0.1:port`. TCP is
/// loopback only until the encrypted transport (QUIC) exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Unix(PathBuf),
    Tcp(std::net::SocketAddr),
    Pipe(String),
    Quic { host: String, port: u16 },
    Tls { host: String, port: u16 },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("a plugin address starts with unix:, pipe:, tcp:, tls: or quic:")]
    Scheme,
    #[error("the socket path is empty")]
    EmptyPath,
    #[error("not a host:port address")]
    Socket,
    #[error("plain TCP is only for this machine (127.0.0.1 or ::1)")]
    NotLoopback,
    #[error("a named pipe must be local (\\\\.\\pipe\\NAME)")]
    NotLocalPipe,
}

impl Address {
    pub fn parse(text: &str) -> Result<Self, AddressError> {
        let text = text.trim();
        if let Some(endpoint) = text
            .strip_prefix("quic:")
            .or_else(|| text.strip_prefix("tls:"))
        {
            let endpoint = endpoint.strip_prefix("//").unwrap_or(endpoint);
            let (host, port) = endpoint.rsplit_once(':').ok_or(AddressError::Socket)?;
            let port: u16 = port.parse().map_err(|_| AddressError::Socket)?;
            let host = host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host);
            if port == 0
                || host.is_empty()
                || host.len() > 253
                || !host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
            {
                return Err(AddressError::Socket);
            }
            return Ok(if text.starts_with("tls:") {
                Self::Tls {
                    host: host.to_ascii_lowercase(),
                    port,
                }
            } else {
                Self::Quic {
                    host: host.to_ascii_lowercase(),
                    port,
                }
            });
        }
        if let Some(path) = text.strip_prefix("pipe:") {
            if !path.starts_with(r"\\.\pipe\")
                || path.len() <= 9
                || path[9..].contains(['\\', '/', '\0'])
            {
                return Err(AddressError::NotLocalPipe);
            }
            return Ok(Self::Pipe(path.into()));
        }
        if let Some(path) = text.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(AddressError::EmptyPath);
            }
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        if let Some(socket) = text.strip_prefix("tcp:") {
            let socket: std::net::SocketAddr = socket.parse().map_err(|_| AddressError::Socket)?;
            if !socket.ip().is_loopback() {
                return Err(AddressError::NotLoopback);
            }
            return Ok(Self::Tcp(socket));
        }
        Err(AddressError::Scheme)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
            Self::Tcp(socket) => write!(f, "tcp:{socket}"),
            Self::Pipe(path) => write!(f, "pipe:{path}"),
            Self::Quic { host, port } if host.contains(':') => write!(f, "quic:[{host}]:{port}"),
            Self::Quic { host, port } => write!(f, "quic:{host}:{port}"),
            Self::Tls { host, port } if host.contains(':') => write!(f, "tls:[{host}]:{port}"),
            Self::Tls { host, port } => write!(f, "tls:{host}:{port}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_and_print_back() {
        for text in [
            "unix:/run/user/1000/neurowatch/plugins/m.sock",
            "tcp:127.0.0.1:7690",
            "tls:analytics.example:7690",
            "tls:[::1]:7690",
            r"pipe:\\.\pipe\neurowatch-motion",
        ] {
            assert_eq!(Address::parse(text).unwrap().to_string(), text);
        }
        assert_eq!(Address::parse("unix:"), Err(AddressError::EmptyPath));
        assert_eq!(
            Address::parse(r"pipe:\\remote\pipe\motion"),
            Err(AddressError::NotLocalPipe)
        );
        assert_eq!(
            Address::parse("quic://EXAMPLE.com:443")
                .unwrap()
                .to_string(),
            "quic:example.com:443"
        );
        assert_eq!(Address::parse("tcp:nope"), Err(AddressError::Socket));
        assert_eq!(
            Address::parse("tcp:192.168.1.5:7690"),
            Err(AddressError::NotLoopback)
        );
    }
}
