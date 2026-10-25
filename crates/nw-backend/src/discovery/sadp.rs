//! Hikvision SADP: the inquiry Hikvision's own tools use to list their
//! cameras and recorders on the local network, also those with ONVIF turned
//! off (the factory setting of many models) or not yet activated.
//!
//! An inquiry is one small XML datagram to 239.255.255.250:37020; devices
//! answer with a `<ProbeMatch>` either to the sender or to the group. Only
//! what is needed to list and add a device is read; nothing is sent back.

use std::net::{Ipv4Addr, SocketAddr};

pub const SADP_PORT: u16 = 37020;
pub const SADP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// Largest reply read; real ones are about 2 KiB.
const MAX_REPLY_BYTES: usize = 16 * 1024;

/// A Hikvision device that answered an inquiry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SadpDevice {
    pub address: Option<Ipv4Addr>,
    /// `DeviceDescription`, the model, e.g. `DS-2CD2143G2-I`.
    pub model: Option<String>,
    pub serial: Option<String>,
    /// `aa:bb:cc:dd:ee:ff`.
    pub mac: Option<String>,
    pub http_port: Option<u16>,
    pub firmware: Option<String>,
    /// False for a device whose admin password was never set: it answers
    /// nothing else until it is activated.
    pub activated: Option<bool>,
}

/// An inquiry: `uuid` is upper-case, as Hikvision's tools send it.
pub fn inquiry(uuid: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><Probe><Uuid>{uuid}</Uuid><Types>inquiry</Types></Probe>"#
    )
}

/// A random upper-case UUID for an inquiry.
pub fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn bounded(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// A `<ProbeMatch>` reply. Our own inquiries (and anything else) are `None`.
pub fn parse_reply(bytes: &[u8]) -> Option<SadpDevice> {
    if bytes.len() > MAX_REPLY_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let doc = roxmltree::Document::parse(text).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "ProbeMatch" {
        return None;
    }
    let field = |tag: &str| {
        root.children()
            .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(tag))
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|t| !t.is_empty())
    };
    let address = field("IPv4Address").and_then(|a| a.parse::<Ipv4Addr>().ok());
    address?;
    Some(SadpDevice {
        address,
        model: field("DeviceDescription")
            .or_else(|| field("DeviceType"))
            .map(|m| bounded(m, 64)),
        serial: field("DeviceSN").map(|s| bounded(s, 64)),
        mac: field("MAC").map(|m| bounded(&m.replace('-', ":").to_ascii_lowercase(), 17)),
        http_port: field("HttpPort")
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p > 0),
        firmware: field("SoftwareVersion").map(|f| bounded(f, 64)),
        activated: field("Activated").and_then(|a| match a.to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }),
    })
}

/// A socket on `port` of every address that receives the replies devices
/// send to the group, joined on `iface`. Hikvision's own tools may hold the
/// port too, so it is shared.
pub fn listener(iface: Ipv4Addr, port: u16) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    socket.join_multicast_v4(&SADP_GROUP, &iface)?;
    tokio::net::UdpSocket::from_std(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ProbeMatch>
<Uuid>6F3A2C1B-0000-4000-8000-000000000001</Uuid>
<Types>inquiry</Types>
<DeviceType>139913</DeviceType>
<DeviceDescription>DS-2CD2143G2-I</DeviceDescription>
<DeviceSN>DS-2CD2143G2-I20220101AAWRJ12345678</DeviceSN>
<CommandPort>8000</CommandPort>
<HttpPort>80</HttpPort>
<MAC>bc-ba-c2-01-02-03</MAC>
<IPv4Address>192.168.1.64</IPv4Address>
<IPv4SubnetMask>255.255.255.0</IPv4SubnetMask>
<IPv4Gateway>192.168.1.1</IPv4Gateway>
<DHCP>false</DHCP>
<SoftwareVersion>V5.7.3build 220112</SoftwareVersion>
<Activated>false</Activated>
</ProbeMatch>"#;

    #[test]
    fn a_probe_match_lists_the_device() {
        let device = parse_reply(REPLY.as_bytes()).expect("device");
        assert_eq!(device.address, Some(Ipv4Addr::new(192, 168, 1, 64)));
        assert_eq!(device.model.as_deref(), Some("DS-2CD2143G2-I"));
        assert_eq!(
            device.serial.as_deref(),
            Some("DS-2CD2143G2-I20220101AAWRJ12345678")
        );
        assert_eq!(device.mac.as_deref(), Some("bc:ba:c2:01:02:03"));
        assert_eq!(device.http_port, Some(80));
        assert_eq!(device.firmware.as_deref(), Some("V5.7.3build 220112"));
        assert_eq!(device.activated, Some(false));
    }

    #[test]
    fn inquiries_and_other_datagrams_are_not_devices() {
        let uuid = new_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid, uuid.to_ascii_uppercase());
        assert!(parse_reply(inquiry(&uuid).as_bytes()).is_none());
        assert!(parse_reply(b"<ProbeMatch><DeviceSN>x</DeviceSN></ProbeMatch>").is_none());
        assert!(parse_reply(b"\xff\xfe").is_none());
        assert!(parse_reply(&vec![b' '; MAX_REPLY_BYTES + 1]).is_none());
    }
}
