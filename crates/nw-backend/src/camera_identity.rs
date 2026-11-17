//! Which physical camera is behind each configured camera: its MAC address,
//! with its serial number, maker and model when it says (2026-11-17).
//!
//! A camera slot keeps its id when someone puts another camera at the same
//! address, so data kept per camera (Cloud Backup's footage, a counting
//! plugin's counts) would join two cameras' histories. The recorder reports
//! each camera's hardware in its inventory (`cloud/link/inventory.rs`
//! `CameraSnapshot::hardware`); NeuroWatch Cloud binds the slot to it and asks
//! what to do when it changes.
//!
//! - **MAC:** the camera's own (ONVIF `GetNetworkInterfaces`), else this
//!   computer's neighbour (ARP) table for the camera's IP address, which only
//!   knows cameras on the same network segment (a camera behind a router has
//!   no entry: its IP's packets go to the router's MAC, filed under the
//!   router's IP, so a wrong MAC is never read).
//! - **Serial, maker, model:** ONVIF `GetDeviceInformation`, with the camera's
//!   credentials. Nothing is known about a camera without ONVIF off its
//!   segment, and nothing is then compared.
//!
//! [`run`] reads the neighbour table every minute (a file read, no traffic)
//! and asks ONVIF every 10 minutes, or at once when the address or MAC changed.
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nw_core::camera::CameraConfig;
use serde::Serialize;
use tokio::sync::RwLock;

use crate::app_state::AppState;
use crate::discovery::onvif::{Credentials, SoapClient};

const CHECK_EVERY: Duration = Duration::from_secs(60);
const ONVIF_EVERY: Duration = Duration::from_secs(600);
const ONVIF_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_TEXT: usize = 64;

/// What a camera says it is. Every field is optional; `mac` is lower case
/// `aa:bb:cc:dd:ee:ff`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CameraHardware {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl CameraHardware {
    pub fn is_empty(&self) -> bool {
        self.mac.is_none() && self.serial.is_none()
    }
}

#[derive(Debug, Clone, Default)]
struct Learned {
    host: String,
    /// From the neighbour table.
    arp_mac: Option<String>,
    /// From ONVIF.
    onvif: CameraHardware,
    onvif_at: Option<Instant>,
}

impl Learned {
    fn hardware(&self) -> CameraHardware {
        CameraHardware {
            mac: self.onvif.mac.clone().or_else(|| self.arp_mac.clone()),
            ..self.onvif.clone()
        }
    }
}

/// The hardware learned for each configured camera (by its config id).
#[derive(Default)]
pub struct CameraIdentities {
    known: Mutex<HashMap<String, Learned>>,
}

impl CameraIdentities {
    /// The camera's hardware, if anything identifies it.
    pub fn get(&self, camera_id: &str) -> Option<CameraHardware> {
        let known = self.known.lock().ok()?;
        Some(known.get(camera_id)?.hardware()).filter(|hardware| !hardware.is_empty())
    }

    #[cfg(test)]
    pub fn set(&self, camera_id: &str, hardware: CameraHardware) {
        if let Ok(mut known) = self.known.lock() {
            known.insert(
                camera_id.to_owned(),
                Learned {
                    onvif: hardware,
                    onvif_at: Some(Instant::now()),
                    ..Learned::default()
                },
            );
        }
    }
}

/// A MAC address as `aa:bb:cc:dd:ee:ff`, from any common spelling; None for
/// one that cannot identify a camera (all zero, broadcast, multicast).
pub fn normalize_mac(text: &str) -> Option<String> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12
        || text
            .chars()
            .any(|c| !(c.is_ascii_hexdigit() || ":-. ".contains(c)))
    {
        return None;
    }
    let bytes: Vec<u8> = (0..6)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    if bytes.iter().all(|b| *b == 0) || bytes.iter().all(|b| *b == 0xff) || bytes[0] & 1 == 1 {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// The MAC of `ip` in a Linux `/proc/net/arp`, if complete.
pub fn arp_lookup(table: &str, ip: Ipv4Addr) -> Option<String> {
    const COMPLETE: u32 = 0x2;
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [address, _kind, flags, mac, ..] = fields.as_slice() else {
            return None;
        };
        let flags = u32::from_str_radix(flags.trim_start_matches("0x"), 16).ok()?;
        (address.parse::<Ipv4Addr>().ok()? == ip && flags & COMPLETE != 0)
            .then(|| normalize_mac(mac))
            .flatten()
    })
}

#[cfg(target_os = "linux")]
fn neighbour_mac(ip: Ipv4Addr) -> Option<String> {
    arp_lookup(&std::fs::read_to_string("/proc/net/arp").ok()?, ip)
}

#[cfg(windows)]
fn neighbour_mac(ip: Ipv4Addr) -> Option<String> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIpNetTable2, MIB_IPNET_TABLE2,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    let mut table: *mut MIB_IPNET_TABLE2 = std::ptr::null_mut();
    // SAFETY: GetIpNetTable2 allocates the table, freed below with FreeMibTable.
    if unsafe { GetIpNetTable2(AF_INET, &mut table) } != 0 || table.is_null() {
        return None;
    }
    let mut found = None;
    // SAFETY: the table holds NumEntries rows, valid until FreeMibTable.
    unsafe {
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        for row in rows {
            let address = Ipv4Addr::from(u32::from_be(row.Address.Ipv4.sin_addr.S_un.S_addr));
            // NL_NEIGHBOR_STATE: 0 unreachable, 1 incomplete; probe, delay,
            // stale, reachable and permanent all know the address.
            let reachable = row.State >= 2;
            if address == ip && row.PhysicalAddressLength == 6 && reachable {
                let mac = &row.PhysicalAddress[..6];
                found = normalize_mac(
                    &mac.iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(":"),
                );
                break;
            }
        }
        FreeMibTable(table as *const _);
    }
    found
}

#[cfg(not(any(target_os = "linux", windows)))]
fn neighbour_mac(_ip: Ipv4Addr) -> Option<String> {
    None
}

/// The host a camera's stream URL names (no credentials, no port).
fn url_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    parsed
        .host_str()
        .map(|host| host.trim_matches(['[', ']']).to_owned())
}

async fn resolve(host: &str) -> Option<Ipv4Addr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        };
    }
    let found = tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host, 0)))
        .await
        .ok()?
        .ok()?;
    found.into_iter().find_map(|address| match address.ip() {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    })
}

fn element_texts(reply: &str, name: &str) -> Vec<String> {
    let Ok(doc) = roxmltree::Document::parse(reply) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == name)
        .filter_map(|node| node.text())
        .map(|text| {
            text.trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_TEXT)
                .collect::<String>()
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// Serial, maker and model from a `GetDeviceInformation` reply.
pub fn parse_device_identity(reply: &str) -> CameraHardware {
    let first = |name: &str| element_texts(reply, name).into_iter().next();
    CameraHardware {
        mac: None,
        serial: first("SerialNumber"),
        manufacturer: first("Manufacturer"),
        model: first("Model"),
    }
}

/// The first enabled interface's MAC from a `GetNetworkInterfaces` reply.
pub fn parse_interface_mac(reply: &str) -> Option<String> {
    element_texts(reply, "HwAddress")
        .iter()
        .find_map(|mac| normalize_mac(mac))
}

async fn ask_onvif(device_url: &str, credentials: Option<&Credentials>) -> Option<CameraHardware> {
    let mut client = SoapClient::new().ok()?;
    client.sync_clock(device_url).await;
    let info = tokio::time::timeout(
        ONVIF_TIMEOUT,
        client.call(device_url, "<tds:GetDeviceInformation/>", credentials),
    )
    .await
    .ok()?
    .ok()?;
    let mut hardware = parse_device_identity(&info);
    if let Ok(Ok(reply)) = tokio::time::timeout(
        ONVIF_TIMEOUT,
        client.call(device_url, "<tds:GetNetworkInterfaces/>", credentials),
    )
    .await
    {
        hardware.mac = parse_interface_mac(&reply);
    }
    Some(hardware)
}

struct Target {
    id: String,
    host: String,
    device_url: Option<String>,
    credentials: Option<Credentials>,
}

fn target(camera: &CameraConfig) -> Option<Target> {
    Some(Target {
        id: camera.id.clone(),
        host: url_host(&camera.url)?,
        device_url: camera.onvif.as_ref().map(|onvif| onvif.device_url.clone()),
        credentials: crate::onvif_events::credentials(camera),
    })
}

/// Learn the cameras' hardware until shutdown (see the module).
pub async fn run(
    state: Arc<RwLock<AppState>>,
    identities: Arc<CameraIdentities>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        let targets: Vec<Target> = {
            let st = state.read().await;
            st.config.cameras.iter().filter_map(target).collect()
        };
        for camera in &targets {
            refresh(&identities, camera).await;
        }
        if let Ok(mut known) = identities.known.lock() {
            known.retain(|id, _| targets.iter().any(|camera| &camera.id == id));
        }
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(CHECK_EVERY) => {}
        }
    }
}

async fn refresh(identities: &CameraIdentities, camera: &Target) {
    let mut learned = identities
        .known
        .lock()
        .ok()
        .and_then(|known| known.get(&camera.id).cloned())
        .filter(|learned| learned.host == camera.host)
        .unwrap_or_else(|| Learned {
            host: camera.host.clone(),
            ..Learned::default()
        });
    let ip = resolve(&camera.host).await;
    let arp = ip.and_then(neighbour_mac);
    let moved = arp.is_some() && arp != learned.arp_mac;
    if arp.is_some() {
        learned.arp_mac = arp;
    }
    let due = learned
        .onvif_at
        .map_or(true, |at| at.elapsed() >= ONVIF_EVERY);
    if let Some(device_url) = camera.device_url.as_deref().filter(|_| due || moved) {
        if let Some(hardware) = ask_onvif(device_url, camera.credentials.as_ref()).await {
            if hardware != learned.onvif {
                tracing::info!(
                    camera = %camera.id,
                    mac = hardware.mac.as_deref().unwrap_or("-"),
                    model = hardware.model.as_deref().unwrap_or("-"),
                    "Camera hardware learned"
                );
            }
            learned.onvif = hardware;
        }
        learned.onvif_at = Some(Instant::now());
    }
    if let Ok(mut known) = identities.known.lock() {
        known.insert(camera.id.clone(), learned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_are_written_one_way_and_unusable_ones_are_refused() {
        assert_eq!(
            normalize_mac("BC-BA-C2-01-02-03").as_deref(),
            Some("bc:ba:c2:01:02:03")
        );
        assert_eq!(
            normalize_mac("bcba.c201.0203").as_deref(),
            Some("bc:ba:c2:01:02:03")
        );
        assert_eq!(normalize_mac("00:00:00:00:00:00"), None);
        assert_eq!(normalize_mac("ff:ff:ff:ff:ff:ff"), None);
        assert_eq!(normalize_mac("01:00:5e:00:00:01"), None, "multicast");
        assert_eq!(normalize_mac("bc:ba:c2:01:02"), None);
        assert_eq!(normalize_mac("zz:ba:c2:01:02:03"), None);
    }

    #[test]
    fn the_neighbour_table_gives_only_complete_entries_for_that_address() {
        let table = "IP address       HW type     Flags       HW address            Mask     Device\n\
                     192.168.1.64     0x1         0x2         bc:ba:c2:01:02:03     *        eth0\n\
                     192.168.1.65     0x1         0x0         00:00:00:00:00:00     *        eth0\n\
                     192.168.1.1      0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0\n";
        assert_eq!(
            arp_lookup(table, Ipv4Addr::new(192, 168, 1, 64)).as_deref(),
            Some("bc:ba:c2:01:02:03")
        );
        assert_eq!(
            arp_lookup(table, Ipv4Addr::new(192, 168, 1, 65)),
            None,
            "incomplete"
        );
        assert_eq!(
            arp_lookup(table, Ipv4Addr::new(10, 0, 0, 5)),
            None,
            "off the segment"
        );
    }

    #[test]
    fn onvif_replies_give_the_serial_model_and_mac() {
        let info = r#"<Envelope><Body><GetDeviceInformationResponse><Manufacturer>HIKVISION</Manufacturer><Model>DS-2CD1221-I5</Model><FirmwareVersion>V5.5</FirmwareVersion><SerialNumber>DS-2CD1221-I520190101AAWR123</SerialNumber></GetDeviceInformationResponse></Body></Envelope>"#;
        let hardware = parse_device_identity(info);
        assert_eq!(
            hardware.serial.as_deref(),
            Some("DS-2CD1221-I520190101AAWR123")
        );
        assert_eq!(hardware.model.as_deref(), Some("DS-2CD1221-I5"));
        assert_eq!(hardware.manufacturer.as_deref(), Some("HIKVISION"));
        let interfaces = r#"<Envelope><Body><GetNetworkInterfacesResponse><NetworkInterfaces token="eth0"><Enabled>true</Enabled><Info><Name>eth0</Name><HwAddress>BC:BA:C2:01:02:03</HwAddress></Info></NetworkInterfaces></GetNetworkInterfacesResponse></Body></Envelope>"#;
        assert_eq!(
            parse_interface_mac(interfaces).as_deref(),
            Some("bc:ba:c2:01:02:03")
        );
        assert_eq!(parse_interface_mac("<x/>"), None);
    }

    #[test]
    fn a_camera_url_gives_its_host_without_credentials() {
        assert_eq!(
            url_host("rtsp://admin:secret@192.168.1.64:554/Streaming/Channels/101").as_deref(),
            Some("192.168.1.64")
        );
        assert_eq!(
            url_host("rtsp://cam.local/stream").as_deref(),
            Some("cam.local")
        );
        assert_eq!(url_host("/dev/video0"), None);
    }

    #[test]
    fn the_onvif_mac_wins_over_the_neighbour_table() {
        let learned = Learned {
            arp_mac: Some("aa:aa:aa:aa:aa:aa".into()),
            onvif: CameraHardware {
                mac: Some("bc:ba:c2:01:02:03".into()),
                serial: Some("X1".into()),
                ..CameraHardware::default()
            },
            ..Learned::default()
        };
        assert_eq!(learned.hardware().mac.as_deref(), Some("bc:ba:c2:01:02:03"));
        let arp_only = Learned {
            arp_mac: Some("aa:aa:aa:aa:aa:aa".into()),
            ..Learned::default()
        };
        assert_eq!(
            arp_only.hardware().mac.as_deref(),
            Some("aa:aa:aa:aa:aa:aa")
        );
    }
}
