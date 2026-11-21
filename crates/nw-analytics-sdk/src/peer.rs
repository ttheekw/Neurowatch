//! Who is at the other end of a plugin connection, and how many wrong access
//! codes each such peer may try.
//!
//! A daemon that counted wrong codes for itself as a whole let one noisy
//! machine lock every recorder out, and let a quiet one keep guessing. A
//! [`Peer`] names the party behind a connection: its network address (TCP/TLS
//! and QUIC), or the user id of a local socket's other end (`SO_PEERCRED`).
//! [`Attempts`] counts failures per peer, with a total across all peers as a
//! backstop for an attacker who has many addresses.
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The party at the other end of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Peer {
    /// A network peer. IPv4-mapped IPv6 addresses count as IPv4, and an IPv6
    /// address as its /64, so one host cannot change address to start again.
    Ip(IpAddr),
    /// The user id of a local (Unix) socket's other end.
    Uid(u32),
    /// A local transport that does not say (Windows named pipes; the
    /// connection is machine-local by construction).
    Local,
    /// Unknown: all such peers share one count.
    #[default]
    Unknown,
}

impl Peer {
    /// The peer behind a socket address (the port does not matter).
    #[must_use]
    pub fn from_socket(address: SocketAddr) -> Self {
        let ip = match address.ip() {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => {
                    let mut octets = v6.octets();
                    octets[8..].fill(0);
                    IpAddr::V6(octets.into())
                }
            },
            other => other,
        };
        Self::Ip(ip)
    }

    /// The user at the other end of a Unix socket; [`Peer::Unknown`] when the
    /// system does not tell.
    #[cfg(unix)]
    #[must_use]
    pub fn of_unix(socket: &std::os::unix::net::UnixStream) -> Self {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let mut credentials = libc::ucred {
                pid: 0,
                uid: 0,
                gid: 0,
            };
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            // SAFETY: `credentials` and `length` are valid for the call and
            // `length` is its size; `fd` is open for the borrow of `socket`.
            let result = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    std::ptr::from_mut(&mut credentials).cast(),
                    &raw mut length,
                )
            };
            if result == 0 {
                return Self::Uid(credentials.uid);
            }
            Self::Unknown
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let (mut uid, mut gid) = (0, 0);
            // SAFETY: both out-pointers are valid; `fd` is open.
            if unsafe { libc::getpeereid(fd, &raw mut uid, &raw mut gid) } == 0 {
                return Self::Uid(uid);
            }
            Self::Unknown
        }
    }
}

/// Failure counts after one more is recorded, or as they stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// This peer's failures in the window.
    pub peer: u32,
    /// Every peer's failures in the window (the backstop).
    pub total: u32,
}

/// Peers whose failures are remembered at once; more share [`Peer::Unknown`]'s
/// count, so a flood of addresses cannot grow the table.
pub const MAX_PEERS: usize = 4096;

/// Failures per peer inside a sliding-start window, plus the total.
#[derive(Debug)]
pub struct Attempts {
    window: Duration,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    peers: HashMap<Peer, (Instant, u32)>,
    total: Option<(Instant, u32)>,
}

impl Attempts {
    /// Failures are forgotten `window` after a peer's first one.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            state: Mutex::new(State::default()),
        }
    }

    /// The counts now, without recording anything.
    pub fn counts(&self, peer: &Peer) -> Counts {
        self.counts_at(peer, Instant::now())
    }

    fn counts_at(&self, peer: &Peer, now: Instant) -> Counts {
        let Ok(state) = self.state.lock() else {
            // Poisoned: act as if everything failed.
            return Counts {
                peer: u32::MAX,
                total: u32::MAX,
            };
        };
        let live = |(since, count): &(Instant, u32)| {
            if now.saturating_duration_since(*since) < self.window {
                *count
            } else {
                0
            }
        };
        let key = if state.peers.contains_key(peer) {
            *peer
        } else if state.peers.len() >= MAX_PEERS {
            Peer::Unknown
        } else {
            *peer
        };
        Counts {
            peer: state.peers.get(&key).map_or(0, live),
            total: state.total.as_ref().map_or(0, live),
        }
    }

    /// Record a wrong code from `peer`.
    pub fn failed(&self, peer: &Peer) -> Counts {
        self.failed_at(peer, Instant::now())
    }

    fn failed_at(&self, peer: &Peer, now: Instant) -> Counts {
        let Ok(mut state) = self.state.lock() else {
            return Counts {
                peer: u32::MAX,
                total: u32::MAX,
            };
        };
        if state.peers.len() >= MAX_PEERS && !state.peers.contains_key(peer) {
            let window = self.window;
            state
                .peers
                .retain(|_, (since, _)| now.saturating_duration_since(*since) < window);
        }
        let key = if state.peers.len() >= MAX_PEERS && !state.peers.contains_key(peer) {
            Peer::Unknown
        } else {
            *peer
        };
        let window = self.window;
        let bump = |entry: &mut (Instant, u32)| {
            if now.saturating_duration_since(entry.0) >= window {
                *entry = (now, 0);
            }
            entry.1 = entry.1.saturating_add(1);
            entry.1
        };
        let for_peer = bump(state.peers.entry(key).or_insert((now, 0)));
        let total = bump(state.total.get_or_insert((now, 0)));
        Counts {
            peer: for_peer,
            total,
        }
    }

    /// A correct code: this peer's failures are forgotten (the total is not).
    pub fn succeeded(&self, peer: &Peer) {
        if let Ok(mut state) = self.state.lock() {
            state.peers.remove(peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> Peer {
        Peer::from_socket(SocketAddr::new(text.parse().unwrap(), 4000))
    }

    #[test]
    fn addresses_are_grouped_by_host_not_port_or_mapping() {
        assert_eq!(
            Peer::from_socket("10.0.0.1:1".parse().unwrap()),
            Peer::from_socket("10.0.0.1:2".parse().unwrap())
        );
        assert_eq!(ip("::ffff:10.0.0.1"), ip("10.0.0.1"));
        assert_eq!(ip("2001:db8::1"), ip("2001:db8::ffff:2"));
        assert_ne!(ip("2001:db8::1"), ip("2001:db8:0:1::1"));
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_peer_is_its_user() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        assert_eq!(Peer::of_unix(&a), Peer::Uid(uid));
        assert_eq!(Peer::of_unix(&b), Peer::Uid(uid));
    }

    #[test]
    fn failures_count_per_peer_with_a_total_and_expire() {
        let attempts = Attempts::new(Duration::from_secs(60));
        let (a, b) = (ip("10.0.0.1"), ip("10.0.0.2"));
        let start = Instant::now();
        for n in 1..=3 {
            assert_eq!(attempts.failed_at(&a, start), Counts { peer: n, total: n });
        }
        assert_eq!(attempts.failed_at(&b, start), Counts { peer: 1, total: 4 });
        assert_eq!(attempts.counts_at(&b, start).peer, 1);
        // After the window, both are forgotten.
        let later = start + Duration::from_secs(61);
        assert_eq!(attempts.counts_at(&a, later), Counts::default());
        // A right code clears only that peer.
        attempts.succeeded(&a);
        assert_eq!(attempts.counts_at(&a, start).peer, 0);
        assert_eq!(attempts.counts_at(&b, start).peer, 1);
    }

    #[test]
    fn a_flood_of_addresses_cannot_grow_the_table_without_bound() {
        let attempts = Attempts::new(Duration::from_secs(60));
        let now = Instant::now();
        for n in 0..(MAX_PEERS as u32 + 100) {
            attempts.failed_at(&Peer::Ip(IpAddr::V4(n.into())), now);
        }
        let state = attempts.state.lock().unwrap();
        assert!(state.peers.len() <= MAX_PEERS + 1);
        assert!(state.peers.contains_key(&Peer::Unknown));
    }
}
