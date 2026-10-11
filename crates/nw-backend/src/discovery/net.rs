//! Which networks a camera search may cover.
//!
//! The default is the LAN of the default-route interface. Container,
//! virtual-bridge and VPN interfaces are never chosen automatically; an
//! operator who needs one types its range instead. A typed range is
//! normalized to its network (`10.0.10.1/24` → `10.0.10.0/24`) and bounded.

use std::net::Ipv4Addr;

use nw_core::camera_discovery::{DiscoveryInterface, MAX_SCAN_ADDRESSES};

/// An IPv4 network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Net {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Net {
    pub fn new(address: Ipv4Addr, prefix: u8) -> Self {
        let mask = mask(prefix);
        Self {
            network: Ipv4Addr::from(u32::from(address) & mask),
            prefix,
        }
    }

    /// `a.b.c.d/n`, or a bare address (`/32`). Host bits are cleared.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (
                address,
                prefix
                    .parse::<u8>()
                    .ok()
                    .filter(|p| *p <= 32)
                    .ok_or_else(|| {
                        format!("“{text}” is not a valid network (use e.g. 192.168.1.0/24)")
                    })?,
            ),
            None => (text, 32),
        };
        let address: Ipv4Addr = address.parse().map_err(|_| {
            format!("“{text}” is not a valid IPv4 network (use e.g. 192.168.1.0/24)")
        })?;
        Ok(Self::new(address, prefix))
    }

    /// Addresses a search would probe: every address except the network and
    /// broadcast addresses of networks larger than /31.
    pub fn host_count(&self) -> u64 {
        let size = 1u64 << (32 - u32::from(self.prefix));
        if self.prefix <= 30 {
            size - 2
        } else {
            size
        }
    }

    pub fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> {
        let start = u32::from(self.network);
        let size = 1u64 << (32 - u32::from(self.prefix));
        let (first, count) = if self.prefix <= 30 {
            (1, size - 2)
        } else {
            (0, size)
        };
        (0..count).map(move |offset| Ipv4Addr::from(start.wrapping_add((first + offset) as u32)))
    }

    pub fn contains(&self, address: Ipv4Addr) -> bool {
        u32::from(address) & mask(self.prefix) == u32::from(self.network)
    }
}

impl std::fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

/// RFC 1918 private ranges.
pub fn is_private(address: Ipv4Addr) -> bool {
    address.is_private()
}

/// Can `net` be searched? `from_portal` searches stay inside private
/// networks; a local operator may also search other unicast ranges.
pub fn check_scope(net: &Ipv4Net, from_portal: bool) -> Result<(), String> {
    if net.host_count() > u64::from(MAX_SCAN_ADDRESSES) {
        return Err(format!(
            "{net} has {} addresses; a search covers at most {MAX_SCAN_ADDRESSES} (a /22 or smaller). Search a smaller range.",
            net.host_count()
        ));
    }
    let first = net.network;
    let last = Ipv4Addr::from(u32::from(first) | !mask(net.prefix));
    for address in [first, last] {
        if address.is_loopback()
            || address.is_multicast()
            || address.is_unspecified()
            || address.is_broadcast()
            || address.is_link_local()
            || u32::from(address) >= 0xF000_0000
        {
            return Err(format!(
                "{net} cannot be searched: it is not a camera network"
            ));
        }
        if from_portal && !is_private(address) {
            return Err(format!(
                "{net} is not a private network; the web portal can only search private ranges (10.x, 172.16–31.x, 192.168.x)"
            ));
        }
    }
    Ok(())
}

/// Interface names that are never a camera LAN on their own.
const VIRTUAL_PREFIXES: &[&str] = &[
    "lo",
    "docker",
    "br-",
    "veth",
    "virbr",
    "vmnet",
    "vboxnet",
    "tun",
    "tap",
    "wg",
    "tailscale",
    "zt",
    "cni",
    "flannel",
    "kube",
    "lxc",
    "lxd",
    "podman",
    "utun",
    "ppp",
    "nordlynx",
    "proton",
];

/// An up IPv4 interface as the operating system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawInterface {
    pub name: String,
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub up: bool,
    pub loopback: bool,
}

/// Interfaces a search may use, the default-route one marked primary.
pub fn eligible_interfaces(
    raw: &[RawInterface],
    default_route: Option<&str>,
) -> Vec<DiscoveryInterface> {
    let mut eligible: Vec<DiscoveryInterface> = raw
        .iter()
        .filter(|iface| {
            iface.up
                && !iface.loopback
                && is_private(iface.address)
                && !iface.address.is_link_local()
                && (1..=30).contains(&iface.prefix)
                && !VIRTUAL_PREFIXES
                    .iter()
                    .any(|prefix| iface.name.starts_with(prefix))
        })
        .map(|iface| {
            let net = Ipv4Net::new(iface.address, iface.prefix);
            DiscoveryInterface {
                id: iface.name.clone(),
                name: iface.name.clone(),
                address: iface.address.to_string(),
                prefix: iface.prefix,
                cidr: net.to_string(),
                primary: false,
            }
        })
        .collect();
    eligible.dedup_by(|a, b| a.id == b.id);
    let primary = default_route
        .and_then(|name| eligible.iter().position(|iface| iface.id == name))
        .unwrap_or(0);
    if let Some(iface) = eligible.get_mut(primary) {
        iface.primary = true;
    }
    eligible
}

/// The interface of the lowest-metric default route, from `/proc/net/route`.
pub fn default_route_interface(route_table: &str) -> Option<String> {
    route_table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (name, destination, mask, metric) = (
                fields.first()?,
                fields.get(1)?,
                fields.get(7)?,
                fields.get(6)?,
            );
            (*destination == "00000000" && *mask == "00000000").then(|| {
                (
                    metric.parse::<u32>().unwrap_or(u32::MAX),
                    (*name).to_owned(),
                )
            })
        })
        .min()
        .map(|(_, name)| name)
}

/// The machine's up IPv4 interfaces.
#[cfg(unix)]
pub fn system_interfaces() -> Vec<RawInterface> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we walk read-only and free once.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a node of the list getifaddrs returned and is not yet freed.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_netmask.is_null() {
            continue;
        }
        // SAFETY: both pointers are non-null sockaddrs owned by the list.
        let (family, addr, netmask) = unsafe {
            (
                i32::from((*entry.ifa_addr).sa_family),
                &*(entry.ifa_addr as *const libc::sockaddr_in),
                &*(entry.ifa_netmask as *const libc::sockaddr_in),
            )
        };
        if family != libc::AF_INET {
            continue;
        }
        // SAFETY: ifa_name is a NUL-terminated string owned by the list.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let flags = entry.ifa_flags;
        out.push(RawInterface {
            name,
            address: Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
            prefix: u32::from_be(netmask.sin_addr.s_addr).count_ones() as u8,
            up: flags & (libc::IFF_UP as u32) != 0 && flags & (libc::IFF_RUNNING as u32) != 0,
            loopback: flags & (libc::IFF_LOOPBACK as u32) != 0,
        });
    }
    // SAFETY: `head` came from a successful getifaddrs and is freed exactly once.
    unsafe { libc::freeifaddrs(head) };
    out
}

#[cfg(not(unix))]
pub fn system_interfaces() -> Vec<RawInterface> {
    Vec::new()
}

/// Interfaces a search may use on this machine.
pub fn interfaces() -> Vec<DiscoveryInterface> {
    let route = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    eligible_interfaces(
        &system_interfaces(),
        default_route_interface(&route).as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, address: &str, prefix: u8) -> RawInterface {
        RawInterface {
            name: name.into(),
            address: address.parse().expect("address"),
            prefix,
            up: true,
            loopback: false,
        }
    }

    #[test]
    fn a_typed_range_is_normalized_to_its_network() {
        let net = Ipv4Net::parse("10.0.10.1/24").expect("parses");
        assert_eq!(net.to_string(), "10.0.10.0/24");
        assert_eq!(net.host_count(), 254);
        assert_eq!(net.hosts().next(), Some(Ipv4Addr::new(10, 0, 10, 1)));
        assert_eq!(net.hosts().last(), Some(Ipv4Addr::new(10, 0, 10, 254)));
        assert_eq!(
            Ipv4Net::parse("192.168.1.64").expect("bare").host_count(),
            1
        );
        assert!(Ipv4Net::parse("10.0.0.0/33").is_err());
        assert!(Ipv4Net::parse("cameras").is_err());
    }

    #[test]
    fn searches_are_bounded_and_the_portal_stays_in_private_networks() {
        assert!(check_scope(&Ipv4Net::parse("10.0.0.0/22").unwrap(), true).is_ok());
        let too_big = check_scope(&Ipv4Net::parse("10.0.0.0/21").unwrap(), true).unwrap_err();
        assert!(too_big.contains("1024"), "{too_big}");
        for bad in [
            "127.0.0.0/24",
            "224.0.0.0/24",
            "169.254.1.0/24",
            "0.0.0.0/24",
            "255.255.255.255/32",
        ] {
            assert!(
                check_scope(&Ipv4Net::parse(bad).unwrap(), false).is_err(),
                "{bad}"
            );
        }
        assert!(check_scope(&Ipv4Net::parse("8.8.8.0/24").unwrap(), true).is_err());
        assert!(check_scope(&Ipv4Net::parse("8.8.8.0/24").unwrap(), false).is_ok());
    }

    #[test]
    fn the_default_route_lan_is_primary_and_virtual_interfaces_are_skipped() {
        let raw = [
            iface("lo", "127.0.0.1", 8),
            iface("docker0", "172.17.0.1", 16),
            iface("wlp3s0", "192.168.1.20", 24),
            iface("enp2s0", "10.0.10.5", 24),
            iface("tailscale0", "100.101.1.2", 32),
        ];
        let route = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     wlp3s0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\n\
                     enp2s0\t00000000\t010A000A\t0003\t0\t0\t100\t00000000\n";
        let found = eligible_interfaces(&raw, default_route_interface(route).as_deref());
        let names: Vec<_> = found
            .iter()
            .map(|i| (i.id.as_str(), i.cidr.as_str(), i.primary))
            .collect();
        assert_eq!(
            names,
            [
                ("wlp3s0", "192.168.1.0/24", false),
                ("enp2s0", "10.0.10.0/24", true)
            ]
        );
        assert!(eligible_interfaces(&[], None).is_empty());
    }
}
