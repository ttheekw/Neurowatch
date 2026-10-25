//! Which networks a camera search may cover.
//!
//! The default is the LAN of the default-route interface. Container,
//! virtual-bridge and VPN interfaces are never chosen automatically; an
//! operator who needs one types its range instead. A typed range
//! ([`HostSet::parse`]) may be a network (`10.0.10.1/24` → `10.0.10.0/24`),
//! the first three parts of one (`192.168.1`, `192.168.1.x`, `192.168.1.*`:
//! its /24), a range (`192.168.1.10-50`, `192.168.1.10-192.168.1.50`) or one
//! address, several separated by commas; it is bounded to
//! [`MAX_SCAN_ADDRESSES`].

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
    check_bounds(&net.to_string(), first, last, from_portal)
}

/// Can every typed network, range and address of `hosts` be searched?
pub fn check_hosts(hosts: &HostSet, from_portal: bool) -> Result<(), String> {
    if hosts.count() > u64::from(MAX_SCAN_ADDRESSES) {
        return Err(format!(
            "{} covers {} addresses; a search covers at most {MAX_SCAN_ADDRESSES} (for example 192.168.1.x, or a /22). Search a smaller range.",
            hosts.label(),
            hosts.count()
        ));
    }
    for part in &hosts.parts {
        let (first, last) = part.bounds();
        check_bounds(&part.to_string(), first, last, from_portal)?;
    }
    Ok(())
}

/// A block from `first` to `last` is searchable when neither end is a
/// special address. Blocks are at most 1024 addresses, so no special range
/// fits strictly between two ordinary ends.
fn check_bounds(
    label: &str,
    first: Ipv4Addr,
    last: Ipv4Addr,
    from_portal: bool,
) -> Result<(), String> {
    for address in [first, last] {
        if address.is_loopback()
            || address.is_multicast()
            || address.is_unspecified()
            || address.is_broadcast()
            || address.is_link_local()
            || u32::from(address) >= 0xF000_0000
        {
            return Err(format!(
                "{label} cannot be searched: it is not a camera network"
            ));
        }
        if from_portal && !is_private(address) {
            return Err(format!(
                "{label} is not a private network; the web portal can only search private ranges (10.x, 172.16–31.x, 192.168.x)"
            ));
        }
    }
    Ok(())
}

/// Most networks, ranges and addresses one search may list.
const MAX_PARTS: usize = 64;

/// One typed network, range or address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Net(Ipv4Net),
    /// Inclusive.
    Range(Ipv4Addr, Ipv4Addr),
}

impl Part {
    /// The addresses probed, inclusive: a network's hosts, a range as typed.
    fn hosts(&self) -> (u32, u32) {
        match *self {
            Part::Net(net) => {
                let first = u32::from(net.network);
                let last = first | !mask(net.prefix);
                if net.prefix <= 30 {
                    (first + 1, last - 1)
                } else {
                    (first, last)
                }
            }
            Part::Range(start, end) => (u32::from(start), u32::from(end)),
        }
    }

    /// The whole block, network and broadcast addresses included.
    fn bounds(&self) -> (Ipv4Addr, Ipv4Addr) {
        match *self {
            Part::Net(net) => (
                net.network,
                Ipv4Addr::from(u32::from(net.network) | !mask(net.prefix)),
            ),
            Part::Range(start, end) => (start, end),
        }
    }
}

impl std::fmt::Display for Part {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Part::Net(net) if net.prefix == 32 => write!(f, "{}", net.network),
            Part::Net(net) => write!(f, "{net}"),
            Part::Range(start, end) if start == end => write!(f, "{start}"),
            Part::Range(start, end) => {
                let (s, e) = (start.octets(), end.octets());
                if s[..3] == e[..3] {
                    write!(f, "{start}-{}", e[3])
                } else {
                    write!(f, "{start}-{end}")
                }
            }
        }
    }
}

/// The addresses a search probes one by one: typed networks, ranges and
/// single addresses, or an interface's network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSet {
    parts: Vec<Part>,
    /// Sorted, merged, inclusive.
    ranges: Vec<(u32, u32)>,
}

fn octet(text: &str) -> Option<u8> {
    let ok = !text.is_empty()
        && text.len() <= 3
        && text.bytes().all(|b| b.is_ascii_digit())
        && !(text.len() > 1 && text.starts_with('0'));
    ok.then(|| text.parse::<u8>().ok()).flatten()
}

fn address(text: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (slot, part) in octets.iter_mut().zip(&parts) {
        *slot = octet(part)?;
    }
    Some(Ipv4Addr::from(octets))
}

/// The pieces of a typed search, en dashes read as hyphens and spaces around
/// a hyphen dropped (`192.168.1.10 - 50`).
fn pieces(text: &str) -> Vec<String> {
    let text: String = text
        .chars()
        .map(|c| if matches!(c, '–' | '—') { '-' } else { c })
        .collect();
    let chars: Vec<char> = text.chars().collect();
    let mut joined = String::with_capacity(text.len());
    for (index, c) in chars.iter().enumerate() {
        if c.is_whitespace() {
            let before = chars[..index].iter().rev().find(|c| !c.is_whitespace());
            let after = chars[index + 1..].iter().find(|c| !c.is_whitespace());
            if before == Some(&'-') || after == Some(&'-') {
                continue;
            }
        }
        joined.push(*c);
    }
    joined
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|piece| !piece.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_part(piece: &str) -> Result<Part, String> {
    let bad = || {
        format!(
            "“{piece}” is not a network, range or address. Try 192.168.1.x, 192.168.1.0/24, 192.168.1.10-50 or 192.168.1.64."
        )
    };
    if let Some((addr, prefix)) = piece.split_once('/') {
        let addr = address(addr).ok_or_else(bad)?;
        let prefix = prefix
            .parse::<u8>()
            .ok()
            .filter(|p| *p <= 32 && prefix.len() <= 2 && prefix.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(bad)?;
        return Ok(Part::Net(Ipv4Net::new(addr, prefix)));
    }
    if let Some((from, to)) = piece.split_once('-') {
        let start = address(from).ok_or_else(bad)?;
        let end = match address(to) {
            Some(end) => end,
            None => {
                let [a, b, c, _] = start.octets();
                Ipv4Addr::new(a, b, c, octet(to).ok_or_else(bad)?)
            }
        };
        if end < start {
            return Err(format!(
                "“{piece}” ends before it starts. Put the lower address first, e.g. 192.168.1.10-50."
            ));
        }
        return Ok(Part::Range(start, end));
    }
    let octets: Vec<&str> = piece.split('.').collect();
    let network = |a: &str, b: &str, c: &str| -> Result<Part, String> {
        let (a, b, c) = (
            octet(a).ok_or_else(bad)?,
            octet(b).ok_or_else(bad)?,
            octet(c).ok_or_else(bad)?,
        );
        Ok(Part::Net(Ipv4Net::new(Ipv4Addr::new(a, b, c, 0), 24)))
    };
    match octets.as_slice() {
        [a, b, c] => network(a, b, c),
        [a, b, c, last] if matches!(*last, "*" | "x" | "X") => network(a, b, c),
        // `192.168.1.0` is not a camera's address: it means the network.
        [a, b, c, "0"] => network(a, b, c),
        [_, _, _, _] => Ok(Part::Net(Ipv4Net::new(address(piece).ok_or_else(bad)?, 32))),
        _ => Err(bad()),
    }
}

impl HostSet {
    /// A typed search: networks, ranges and addresses separated by commas.
    pub fn parse(text: &str) -> Result<Self, String> {
        let pieces = pieces(text);
        if pieces.is_empty() {
            return Err(
                "Enter a network, range or address to search, e.g. 192.168.1.x or 192.168.1.10-50."
                    .into(),
            );
        }
        if pieces.len() > MAX_PARTS {
            return Err(format!(
                "Enter at most {MAX_PARTS} networks, ranges or addresses at a time."
            ));
        }
        let parts = pieces
            .iter()
            .map(|piece| parse_part(piece))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::of(parts))
    }

    /// Every host of `net`.
    pub fn from_net(net: Ipv4Net) -> Self {
        Self::of(vec![Part::Net(net)])
    }

    fn of(parts: Vec<Part>) -> Self {
        let mut spans: Vec<(u32, u32)> = parts.iter().map(Part::hosts).collect();
        spans.sort_unstable();
        let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            match ranges.last_mut() {
                Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                _ => ranges.push((start, end)),
            }
        }
        Self { parts, ranges }
    }

    /// Addresses probed, each counted once.
    pub fn count(&self) -> u64 {
        self.ranges
            .iter()
            .map(|(start, end)| u64::from(end - start) + 1)
            .sum()
    }

    pub fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> + '_ {
        self.ranges
            .iter()
            .flat_map(|&(start, end)| (start..=end).map(Ipv4Addr::from))
    }

    pub fn contains(&self, address: Ipv4Addr) -> bool {
        let address = u32::from(address);
        self.ranges
            .iter()
            .any(|&(start, end)| (start..=end).contains(&address))
    }

    pub fn first(&self) -> Option<Ipv4Addr> {
        self.ranges.first().map(|&(start, _)| Ipv4Addr::from(start))
    }

    /// What was asked, normalized: `192.168.1.0/24, 10.0.0.5-20`.
    pub fn label(&self) -> String {
        const SHOWN: usize = 4;
        let mut shown: Vec<String> = self.parts.iter().take(SHOWN).map(Part::to_string).collect();
        if self.parts.len() > SHOWN {
            shown.push(format!("and {} more", self.parts.len() - SHOWN));
        }
        shown.join(", ")
    }
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

/// An address a camera LAN may be searched from: private, not link-local,
/// on a network of at least four addresses.
fn usable_address(address: Ipv4Addr, prefix: u8) -> bool {
    is_private(address) && !address.is_link_local() && (1..=30).contains(&prefix)
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
                && usable_address(iface.address, iface.prefix)
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

/// One network adapter as Windows reports it (`GetAdaptersAddresses`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsAdapter {
    /// `AdapterName`, a GUID: stable while the adapter exists.
    pub id: String,
    /// What Windows shows, e.g. "Wi-Fi" or "Ethernet 2".
    pub friendly_name: String,
    /// The driver's name, e.g. "Hyper-V Virtual Ethernet Adapter".
    pub description: String,
    /// `IF_TYPE_*`.
    pub if_type: u32,
    /// `OperStatus == IfOperStatusUp`.
    pub up: bool,
    /// IPv4 unicast addresses with their on-link prefix lengths.
    pub addresses: Vec<(Ipv4Addr, u8)>,
    /// Has an IPv4 default gateway.
    pub has_gateway: bool,
    pub metric: u32,
}

const IF_TYPE_PPP: u32 = 23;
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_TUNNEL: u32 = 131;

/// Words in an adapter's name or driver that mark a virtual, VPN or
/// loopback adapter: never a camera LAN on its own.
const VIRTUAL_ADAPTER_WORDS: &[&str] = &[
    "vethernet",
    "virtual ethernet",
    "virtual adapter",
    "virtual miniport",
    "virtualbox",
    "vmware",
    "wsl",
    "tap-",
    "wireguard",
    "tailscale",
    "zerotier",
    "vpn",
    "docker",
    "loopback",
    "bluetooth",
    "hamachi",
    "anyconnect",
    "pangp",
];

fn is_virtual_adapter(adapter: &WindowsAdapter) -> bool {
    let names = format!("{} {}", adapter.friendly_name, adapter.description).to_lowercase();
    VIRTUAL_ADAPTER_WORDS
        .iter()
        .any(|word| names.contains(word))
}

/// Interfaces a search may use on Windows. Primary: the adapter with a
/// default gateway and the lowest IPv4 metric (the one Windows routes
/// through), else the first.
pub fn windows_interfaces(adapters: &[WindowsAdapter]) -> Vec<DiscoveryInterface> {
    let lans = |allow_virtual: bool| -> Vec<(&WindowsAdapter, Ipv4Addr, u8)> {
        adapters
            .iter()
            .filter(|a| {
                a.up && ![IF_TYPE_PPP, IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL]
                    .contains(&a.if_type)
                    && (if allow_virtual {
                        a.has_gateway
                    } else {
                        !is_virtual_adapter(a)
                    })
            })
            .filter_map(|a| {
                // An adapter's first usable address is its LAN.
                a.addresses
                    .iter()
                    .find(|(address, prefix)| usable_address(*address, *prefix))
                    .map(|&(address, prefix)| (a, address, prefix))
            })
            .collect()
    };
    // A host whose LAN runs through a virtual switch (Hyper-V's external
    // "vEthernet") has only virtual adapters: then those with a gateway.
    let mut usable = lans(false);
    if usable.is_empty() {
        usable = lans(true);
    }
    let primary = usable
        .iter()
        .enumerate()
        .filter(|(_, (a, _, _))| a.has_gateway)
        .min_by_key(|(_, (a, _, _))| a.metric)
        .map_or(0, |(index, _)| index);
    usable
        .iter()
        .enumerate()
        .map(|(index, (adapter, address, prefix))| {
            let name = [&adapter.friendly_name, &adapter.description, &adapter.id]
                .into_iter()
                .find(|n| !n.trim().is_empty())
                .cloned()
                .unwrap_or_default();
            DiscoveryInterface {
                id: adapter.id.clone(),
                name,
                address: address.to_string(),
                prefix: *prefix,
                cidr: Ipv4Net::new(*address, *prefix).to_string(),
                primary: index == primary,
            }
        })
        .collect()
}

/// The machine's network adapters with their IPv4 addresses.
#[cfg(windows)]
pub fn system_adapters() -> Vec<WindowsAdapter> {
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR, SOCKADDR_IN};

    /// The IPv4 address of a socket address, if it is one.
    ///
    /// SAFETY: `sockaddr` is null or points to a socket address owned by the
    /// adapter list.
    unsafe fn ipv4(sockaddr: *const SOCKADDR) -> Option<Ipv4Addr> {
        if sockaddr.is_null() || (*sockaddr).sa_family != AF_INET {
            return None;
        }
        let sin = &*(sockaddr as *const SOCKADDR_IN);
        // In network byte order in memory.
        Some(Ipv4Addr::from(sin.sin_addr.S_un.S_addr.to_ne_bytes()))
    }

    /// SAFETY: `text` is null or a NUL-terminated UTF-16 string.
    unsafe fn wide(text: *const u16) -> String {
        if text.is_null() {
            return String::new();
        }
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }

    let flags = GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_INCLUDE_GATEWAYS;
    let mut size: u32 = 16 * 1024;
    // u64 words: the list's structures need 8-byte alignment.
    let mut buffer: Vec<u64> = Vec::new();
    let mut filled = false;
    for _ in 0..4 {
        buffer = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: the buffer holds `size` bytes, as the call is told.
        let status = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                flags,
                std::ptr::null(),
                buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut size,
            )
        };
        if status == NO_ERROR {
            filled = true;
            break;
        }
        if status != ERROR_BUFFER_OVERFLOW {
            tracing::debug!(status, "GetAdaptersAddresses failed");
            return Vec::new();
        }
    }
    if !filled {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !cursor.is_null() {
        // SAFETY: `cursor` is a node of the list GetAdaptersAddresses wrote
        // into `buffer`, which outlives this loop.
        let adapter = unsafe { &*cursor };
        cursor = adapter.Next;
        let id = if adapter.AdapterName.is_null() {
            String::new()
        } else {
            // SAFETY: AdapterName is a NUL-terminated ANSI string in the list.
            unsafe { std::ffi::CStr::from_ptr(adapter.AdapterName.cast()) }
                .to_string_lossy()
                .into_owned()
        };
        let mut addresses = Vec::new();
        let mut unicast = adapter.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: a node of the adapter's unicast list.
            let entry = unsafe { &*unicast };
            unicast = entry.Next;
            // SAFETY: the entry's socket address is owned by the list.
            if let Some(address) = unsafe { ipv4(entry.Address.lpSockaddr) } {
                addresses.push((address, entry.OnLinkPrefixLength));
            }
        }
        let mut has_gateway = false;
        let mut gateway = adapter.FirstGatewayAddress;
        while !gateway.is_null() {
            // SAFETY: a node of the adapter's gateway list.
            let entry = unsafe { &*gateway };
            gateway = entry.Next;
            // SAFETY: the entry's socket address is owned by the list.
            if unsafe { ipv4(entry.Address.lpSockaddr) }.is_some_and(|g| !g.is_unspecified()) {
                has_gateway = true;
            }
        }
        out.push(WindowsAdapter {
            id,
            // SAFETY: both are NUL-terminated UTF-16 strings in the list.
            friendly_name: unsafe { wide(adapter.FriendlyName) },
            description: unsafe { wide(adapter.Description) },
            if_type: adapter.IfType,
            up: adapter.OperStatus == IfOperStatusUp,
            addresses,
            has_gateway,
            metric: adapter.Ipv4Metric,
        });
    }
    out
}

/// Interfaces a search may use on this machine.
#[cfg(windows)]
pub fn interfaces() -> Vec<DiscoveryInterface> {
    windows_interfaces(&system_adapters())
}

/// Interfaces a search may use on this machine.
#[cfg(not(windows))]
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

    fn hosts_of(text: &str) -> (String, u64, Option<Ipv4Addr>, Option<Ipv4Addr>) {
        let set = HostSet::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        (set.label(), set.count(), set.first(), set.hosts().last())
    }

    #[test]
    fn a_search_may_be_typed_as_a_network_range_or_address() {
        let lan = (
            "192.168.1.0/24".to_owned(),
            254,
            Some(Ipv4Addr::new(192, 168, 1, 1)),
            Some(Ipv4Addr::new(192, 168, 1, 254)),
        );
        for text in [
            "192.168.1.0/24",
            "192.168.1.77/24",
            "192.168.1",
            "192.168.1.*",
            "192.168.1.x",
            " 192.168.1.X ",
            "192.168.1.0",
        ] {
            assert_eq!(hosts_of(text), lan, "{text}");
        }
        let range = hosts_of("192.168.1.10-50");
        assert_eq!(range.0, "192.168.1.10-50");
        assert_eq!(range.1, 41);
        assert_eq!(hosts_of("192.168.1.10 – 192.168.1.50"), range);
        assert_eq!(hosts_of("192.168.1.10 - 50").1, 41);
        let across = hosts_of("10.0.0.250-10.0.1.5");
        assert_eq!((across.0.as_str(), across.1), ("10.0.0.250-10.0.1.5", 12));
        let one = hosts_of("192.168.1.64");
        assert_eq!((one.0.as_str(), one.1), ("192.168.1.64", 1));
        assert_eq!(one.2, Some(Ipv4Addr::new(192, 168, 1, 64)));

        // Several, overlapping ones counted once.
        let many = HostSet::parse("192.168.1.x, 192.168.1.64; 10.0.0.5-6").expect("parses");
        assert_eq!(many.label(), "192.168.1.0/24, 192.168.1.64, 10.0.0.5-6");
        assert_eq!(many.count(), 256);
        assert!(many.contains(Ipv4Addr::new(10, 0, 0, 6)));
        assert!(!many.contains(Ipv4Addr::new(10, 0, 0, 7)));
        assert!(!many.contains(Ipv4Addr::new(192, 168, 1, 0)));
        assert_eq!(many.first(), Some(Ipv4Addr::new(10, 0, 0, 5)));

        for bad in [
            "",
            " , ",
            "cameras",
            "192.168.1.256",
            "192.168.01.5",
            "192.168.1.0/33",
            "192.168.*.*",
            "192.168.1.50-10",
            "192.168.1.10-300",
            "10.0.0.5-10.0.0.1",
        ] {
            let error = HostSet::parse(bad).expect_err(bad);
            assert!(error.contains("192.168.1."), "{bad}: {error}");
        }
    }

    #[test]
    fn typed_searches_are_bounded_and_the_portal_stays_private() {
        let ok = |text: &str, portal: bool| check_hosts(&HostSet::parse(text).unwrap(), portal);
        assert!(ok("192.168.1.x, 192.168.0.x", true).is_ok());
        assert!(ok("10.0.0.1-10.0.4.0", true).is_ok());
        let big = ok("10.0.0.1-10.0.4.1", true).unwrap_err();
        assert!(big.contains("1025") && big.contains("1024"), "{big}");
        assert!(ok("10.0.0.0/22, 10.0.4.1", true).is_ok());
        assert!(ok("10.0.0.0/22, 10.0.4.1-3", true).is_err());
        assert!(ok("8.8.8.8", true).unwrap_err().contains("private"));
        assert!(ok("8.8.8.8", false).is_ok());
        assert!(ok("192.168.1.5, 127.0.0.1", false).is_err());
        assert!(ok("169.254.0.1-20", false).is_err());
        assert!(ok("172.15.255.250-172.16.0.3", true).is_err());
    }

    fn adapter(
        name: &str,
        description: &str,
        address: &str,
        gateway: bool,
        metric: u32,
    ) -> WindowsAdapter {
        WindowsAdapter {
            id: format!("{{{name}}}"),
            friendly_name: name.into(),
            description: description.into(),
            if_type: 71,
            up: true,
            addresses: vec![(address.parse().expect("address"), 24)],
            has_gateway: gateway,
            metric,
        }
    }

    #[test]
    fn windows_wifi_is_found_and_virtual_adapters_are_skipped() {
        let mut down = adapter(
            "Ethernet",
            "Intel(R) Ethernet Connection",
            "192.168.50.2",
            true,
            5,
        );
        down.up = false;
        let mut loopback = adapter("Loopback Pseudo-Interface 1", "", "127.0.0.1", false, 75);
        loopback.if_type = IF_TYPE_SOFTWARE_LOOPBACK;
        let mut tunnel = adapter(
            "Teredo",
            "Teredo Tunneling Pseudo-Interface",
            "10.9.0.2",
            false,
            1,
        );
        tunnel.if_type = IF_TYPE_TUNNEL;
        let mut link_local = adapter("Ethernet 3", "Realtek USB GbE", "169.254.10.4", false, 25);
        link_local
            .addresses
            .insert(0, ("169.254.10.4".parse().unwrap(), 16));
        let adapters = [
            down,
            loopback,
            tunnel,
            adapter(
                "vEthernet (WSL)",
                "Hyper-V Virtual Ethernet Adapter",
                "172.29.80.1",
                false,
                15,
            ),
            adapter(
                "Ethernet 2",
                "VirtualBox Host-Only Ethernet Adapter",
                "192.168.56.1",
                false,
                25,
            ),
            adapter(
                "VMware Network Adapter VMnet8",
                "VMware Virtual Ethernet Adapter",
                "192.168.163.1",
                false,
                35,
            ),
            adapter("Tailscale", "Tailscale Tunnel", "100.64.1.2", false, 5),
            adapter(
                "Local Area Connection",
                "TAP-Windows Adapter V9",
                "10.8.0.6",
                true,
                1,
            ),
            adapter(
                "Ethernet 4",
                "Cisco AnyConnect Secure Mobility Client Virtual Miniport",
                "10.20.0.4",
                true,
                1,
            ),
            link_local,
            adapter("Ethernet 5", "Realtek PCIe GbE", "10.0.10.5", true, 50),
            adapter(
                "Wi-Fi",
                "Intel(R) Wi-Fi 6 AX201 160MHz",
                "192.168.1.23",
                true,
                35,
            ),
        ];
        let found = windows_interfaces(&adapters);
        let shown: Vec<_> = found
            .iter()
            .map(|i| (i.name.as_str(), i.cidr.as_str(), i.primary))
            .collect();
        assert_eq!(
            shown,
            [
                ("Ethernet 5", "10.0.10.0/24", false),
                ("Wi-Fi", "192.168.1.0/24", true),
            ]
        );
        assert_eq!(found[1].id, "{Wi-Fi}");
        assert_eq!(found[1].address, "192.168.1.23");

        // A Hyper-V guest's own adapter is not the host's virtual switch.
        let guest = [adapter(
            "Ethernet",
            "Microsoft Hyper-V Network Adapter",
            "10.1.2.3",
            true,
            15,
        )];
        assert_eq!(windows_interfaces(&guest).len(), 1);
        // Only a virtual switch with the gateway (LAN bridged through Hyper-V).
        let bridged = [
            adapter(
                "vEthernet (Default Switch)",
                "Hyper-V Virtual Ethernet Adapter",
                "172.20.0.1",
                false,
                5000,
            ),
            adapter(
                "vEthernet (External)",
                "Hyper-V Virtual Ethernet Adapter #2",
                "192.168.1.30",
                true,
                25,
            ),
        ];
        let found = windows_interfaces(&bridged);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].cidr.as_str(), found[0].primary),
            ("192.168.1.0/24", true)
        );

        // No adapter with a gateway: the first usable one.
        let lone = [adapter("Ethernet", "Realtek", "192.168.0.10", false, 10)];
        assert!(windows_interfaces(&lone)[0].primary);
        assert!(windows_interfaces(&[]).is_empty());
    }
}
