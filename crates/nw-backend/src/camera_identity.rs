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
        .is_none_or(|at| at.elapsed() >= ONVIF_EVERY);
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

    /// A loopback ONVIF device answering from `reply(body)`.
    async fn device(
        reply: impl Fn(&str) -> (u16, String) + Send + Sync + 'static,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let reply = Arc::new(reply);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let reply = Arc::clone(&reply);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let body = loop {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buffer.len() >= end + 4 + length {
                            break String::from_utf8_lossy(&buffer[end + 4..end + 4 + length])
                                .into_owned();
                        }
                    };
                    let (status, text) = reply(&body);
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        text.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(text.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (port, task)
    }

    fn answers(body: &str) -> (u16, String) {
        let reply = if body.contains("<tds:GetDeviceInformation") {
            "<Envelope><Body><GetDeviceInformationResponse><Manufacturer>ACME</Manufacturer><Model>C1</Model><SerialNumber>SN-1</SerialNumber></GetDeviceInformationResponse></Body></Envelope>"
        } else if body.contains("<tds:GetNetworkInterfaces") {
            "<Envelope><Body><GetNetworkInterfacesResponse><NetworkInterfaces><Info><HwAddress>BC:BA:C2:01:02:03</HwAddress></Info></NetworkInterfaces></GetNetworkInterfacesResponse></Body></Envelope>"
        } else {
            "<Envelope><Body/></Envelope>"
        };
        (200, reply.to_owned())
    }

    fn target_for(port: u16) -> Target {
        Target {
            id: "cam".into(),
            host: "127.0.0.1".into(),
            device_url: Some(format!("http://127.0.0.1:{port}/onvif/device_service")),
            credentials: None,
        }
    }

    #[tokio::test]
    async fn hosts_resolve_to_ipv4_only() {
        assert_eq!(
            resolve("192.168.1.9").await,
            Some(Ipv4Addr::new(192, 168, 1, 9))
        );
        assert_eq!(resolve("::1").await, None);
        assert_eq!(resolve("localhost").await, Some(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn asking_a_device_gives_its_identity_and_survives_a_missing_interface_list() {
        let (port, server) = device(answers).await;
        let url = format!("http://127.0.0.1:{port}/onvif/device_service");
        let hardware = ask_onvif(&url, None).await.unwrap();
        assert_eq!(hardware.serial.as_deref(), Some("SN-1"));
        assert_eq!(hardware.manufacturer.as_deref(), Some("ACME"));
        assert_eq!(hardware.model.as_deref(), Some("C1"));
        assert_eq!(hardware.mac.as_deref(), Some("bc:ba:c2:01:02:03"));
        server.abort();

        // A device without GetNetworkInterfaces still names itself.
        let (port, server) = device(|body| {
            if body.contains("<tds:GetNetworkInterfaces") {
                (500, String::new())
            } else {
                answers(body)
            }
        })
        .await;
        let url = format!("http://127.0.0.1:{port}/onvif/device_service");
        let hardware = ask_onvif(&url, None).await.unwrap();
        assert_eq!(hardware.serial.as_deref(), Some("SN-1"));
        assert_eq!(hardware.mac, None);
        server.abort();

        // One that refuses everything tells nothing.
        let (port, server) = device(|_| (500, String::new())).await;
        assert!(ask_onvif(&format!("http://127.0.0.1:{port}/x"), None)
            .await
            .is_none());
        server.abort();
    }

    #[tokio::test]
    async fn a_refresh_asks_onvif_once_and_starts_over_when_the_host_changes() {
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (port, server) = {
            let asked = Arc::clone(&asked);
            device(move |body| {
                if body.contains("<tds:GetDeviceInformation") {
                    asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                answers(body)
            })
            .await
        };
        let identities = CameraIdentities::default();
        assert!(identities.get("cam").is_none());
        let camera = target_for(port);
        refresh(&identities, &camera).await;
        let hardware = identities.get("cam").unwrap();
        assert_eq!(hardware.serial.as_deref(), Some("SN-1"));
        refresh(&identities, &camera).await;
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a fresh answer is reused for ten minutes"
        );
        // The same slot at another address is a new camera: asked again.
        let moved = Target {
            host: "localhost".into(),
            ..target_for(port)
        };
        refresh(&identities, &moved).await;
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Without ONVIF nothing is learned (and nothing is a hardware).
        refresh(
            &identities,
            &Target {
                id: "plain".into(),
                device_url: None,
                ..target_for(port)
            },
        )
        .await;
        assert!(identities.get("plain").is_none());
        server.abort();
    }

    #[test]
    fn only_cameras_with_a_readable_address_are_targets() {
        let mut camera = CameraConfig::new("Cam", "rtsp://user:pw@10.0.0.5:554/live");
        camera.onvif = Some(nw_core::camera::OnvifEndpoint {
            device_url: "http://10.0.0.5/onvif/device_service".into(),
            events_url: None,
            ptz_url: None,
        });
        let found = target(&camera).unwrap();
        assert_eq!(found.host, "10.0.0.5");
        assert_eq!(
            found.device_url.as_deref(),
            Some("http://10.0.0.5/onvif/device_service")
        );
        assert_eq!(found.credentials.unwrap().username, "user");
        assert!(target(&CameraConfig::new("Bad", "not a url")).is_none());
        // The learned hardware needs a MAC or a serial to count.
        let identities = CameraIdentities::default();
        identities.set(
            "only-model",
            CameraHardware {
                model: Some("M".into()),
                ..CameraHardware::default()
            },
        );
        assert!(identities.get("only-model").is_none());
        assert!(!CameraHardware {
            mac: Some("a".into()),
            ..Default::default()
        }
        .is_empty());
    }

    #[tokio::test]
    async fn the_run_loop_learns_configured_cameras_forgets_removed_ones_and_stops() {
        let (port, server) = device(answers).await;
        let mut config = crate::config::AppConfig::default();
        let mut camera = CameraConfig::new("Cam", format!("rtsp://127.0.0.1:{port}/live"));
        camera.id = "cam".into();
        camera.onvif = Some(nw_core::camera::OnvifEndpoint {
            device_url: format!("http://127.0.0.1:{port}/onvif/device_service"),
            events_url: None,
            ptz_url: None,
        });
        config.cameras.push(camera);
        let dir = std::env::temp_dir().join(format!("nw-ident-{}", uuid::Uuid::new_v4()));
        let state = Arc::new(RwLock::new(AppState::with_database(
            config,
            dir.join("config.json"),
            crate::pipeline::hwaccel::HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )));
        let identities = Arc::new(CameraIdentities::default());
        // A camera that has since been removed is dropped from the table.
        identities.set(
            "gone",
            CameraHardware {
                serial: Some("OLD".into()),
                ..CameraHardware::default()
            },
        );
        let (shutdown, shutdown_rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(run(
            Arc::clone(&state),
            Arc::clone(&identities),
            shutdown_rx,
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            while identities.get("cam").is_none() || identities.get("gone").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the camera's hardware is learned");
        assert_eq!(
            identities.get("cam").unwrap().serial.as_deref(),
            Some("SN-1")
        );
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }
}
