//! Camera discovery: search the network for cameras and add one.
//!
//! The node that owns the cameras does all of it; the desktop and the web
//! portal only ask. A search sends WS-Discovery probes (multicast on a local
//! interface, and one by one to each address of the network or typed
//! range), a Hikvision SADP inquiry (multicast; finds Hikvision cameras with
//! ONVIF off), and checks the RTSP port of every address nothing answered
//! for. It asks every ONVIF device that answers who it is, merges what each
//! method learned about an address into one result, and keeps the results
//! for a few minutes under opaque ids. Credentials are asked for one chosen device at a time
//! and kept only until it is added or the cache expires. Nothing here
//! persists a camera: `camera.discovery.add` builds the configuration and the
//! RPC handler adds it through the same validation as `camera.add`.

pub mod matchers;
pub mod net;
pub mod onvif;
pub mod rtsp;
pub mod sadp;

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use nw_core::camera::{CameraConfig, CameraStream};
use nw_core::camera_discovery::{
    CameraRef, CandidateState, DiscoveryCandidate, DiscoveryIdentity, ScanResults, ScanStatus,
    StreamChoice,
};

use matchers::{match_camera, Match};
use net::{HostSet, Ipv4Net};
use onvif::{Credentials, DeviceInfo, MediaProfile, OnvifError, ProbeMatch, SoapClient};
use rtsp::RtspDevice;
use sadp::SadpDevice;

/// How long a search, its candidates and any credentials given for one stay.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Listening goes on this long after the last probe was sent.
const LISTEN_AFTER_LAST_PROBE: Duration = Duration::from_secs(3);
/// No search runs longer than this.
const MAX_SCAN_DURATION: Duration = Duration::from_secs(30);
/// Unicast probes per second.
const PROBES_PER_SECOND: u32 = 400;
/// Devices asked who they are at the same time.
const ENRICH_CONCURRENCY: usize = 8;
/// The RTSP port check starts this long after the last WS-Discovery probe,
/// so devices that answered it are not checked again.
const RTSP_SWEEP_GRACE: Duration = Duration::from_millis(800);
const MAX_CANDIDATES: usize = 256;
const MAX_FINISHED_SCANS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryError(pub String);

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, DiscoveryError>;

fn err<T>(message: impl Into<String>) -> Result<T> {
    Err(DiscoveryError(message.into()))
}

/// What to search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The default-route LAN.
    Current,
    /// The LAN of the named interface.
    Interface(String),
    /// Typed networks, ranges or addresses (see [`HostSet::parse`]). The
    /// wire name stays `cidr`.
    Cidr(String),
}

/// A planned search: where to multicast, which addresses to probe one by one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    label: String,
    multicast_from: Option<Ipv4Addr>,
    /// The network of the interface multicast goes out on.
    local_net: Option<Ipv4Net>,
    /// Addresses whose answers are kept.
    accept: HostSet,
    /// Addresses probed one by one: WS-Discovery, then the RTSP port.
    unicast: HostSet,
}

/// Where a search sends. Tests point every method at loopback responders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ports {
    ws_discovery: u16,
    /// Where SADP inquiries go; none: no SADP.
    sadp: Option<SocketAddr>,
    /// The port SADP replies to the group arrive on.
    sadp_listen: Option<u16>,
    /// The RTSP port checked; none: no port check.
    rtsp: Option<u16>,
    /// The HTTP port an RTSP-only device is asked over ONVIF on.
    onvif_http: u16,
}

impl Ports {
    const REAL: Ports = Ports {
        ws_discovery: onvif::WS_DISCOVERY_PORT,
        sadp: Some(SocketAddr::V4(std::net::SocketAddrV4::new(
            sadp::SADP_GROUP,
            sadp::SADP_PORT,
        ))),
        sadp_listen: Some(sadp::SADP_PORT),
        rtsp: Some(rtsp::RTSP_PORT),
        onvif_http: 80,
    };
}

/// Which method found a device; when several did, the higher one's
/// details win (ONVIF knows the most).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Rtsp,
    Sadp,
    Onvif,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Ready,
    CredentialsNeeded,
    Unavailable,
}

impl Reach {
    fn rank(self) -> u8 {
        match self {
            Reach::Unavailable => 0,
            Reach::CredentialsNeeded => 1,
            Reach::Ready => 2,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedStream {
    stream_id: String,
    profile: MediaProfile,
    url: String,
}

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    address: Ipv4Addr,
    endpoint_reference: Option<String>,
    device_urls: Vec<String>,
    name: Option<String>,
    hardware: Option<String>,
    info: DeviceInfo,
    reach: Reach,
    /// A typed port, or SADP, means a Hikvision camera with ONVIF off may answer.
    hikvision_hint: bool,
    streams: Vec<CachedStream>,
    credentials: Option<Credentials>,
    expires: Instant,
    origin: Origin,
    /// Its ONVIF device service answered (with or without a password).
    onvif_answered: bool,
    sadp: Option<SadpDevice>,
    rtsp: Option<RtspDevice>,
    /// SADP found it with an address outside the searched interface's network.
    outside_network: bool,
}

struct Scan {
    id: String,
    scope: String,
    status: ScanStatus,
    probed: u32,
    total: u32,
    candidates: Vec<String>,
    error: Option<String>,
    cancelled: Arc<AtomicBool>,
    started: Instant,
}

#[derive(Default)]
struct Inner {
    scans: HashMap<String, Scan>,
    candidates: HashMap<String, Candidate>,
}

/// The discovery service of one node. Cheap to share; every UI uses the same one.
pub struct DiscoveryService {
    inner: Arc<Mutex<Inner>>,
    ports: Ports,
    /// Tests search loopback, which real searches refuse.
    allow_loopback: bool,
}

impl Default for DiscoveryService {
    fn default() -> Self {
        Self::new()
    }
}

fn opaque_id() -> String {
    let mut bytes = [0u8; 12];
    let _ = getrandom::getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn label_of(candidate: &Candidate) -> Option<String> {
    candidate.name.clone().or_else(
        || match (&candidate.info.manufacturer, &candidate.info.model) {
            (Some(m), Some(model)) => Some(format!("{m} {model}")),
            (_, Some(model)) => Some(model.clone()),
            _ => candidate.hardware.clone(),
        },
    )
}

impl DiscoveryService {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            ports: Ports::REAL,
            allow_loopback: false,
        }
    }

    /// WS-Discovery only, on loopback.
    #[cfg(test)]
    fn for_test(ws_discovery_port: u16) -> Self {
        Self::with_ports(Ports {
            ws_discovery: ws_discovery_port,
            sadp: None,
            sadp_listen: None,
            rtsp: None,
            onvif_http: 80,
        })
    }

    #[cfg(test)]
    fn with_ports(ports: Ports) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            ports,
            allow_loopback: true,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn plan(&self, scope: &Scope, from_portal: bool) -> Result<Plan> {
        let interfaces = net::interfaces();
        let on_interface = |iface: &nw_core::camera_discovery::DiscoveryInterface| -> Result<Plan> {
            let address: Ipv4Addr = iface
                .address
                .parse()
                .map_err(|_| DiscoveryError("interface has no IPv4 address".into()))?;
            Ok(interface_plan(address, iface.prefix))
        };
        match scope {
            Scope::Current => match interfaces.iter().find(|i| i.primary) {
                Some(iface) => on_interface(iface),
                None => err("No local network was found to search (no active private IPv4 network). Enter a range such as 192.168.1.x instead."),
            },
            Scope::Interface(id) => match interfaces.iter().find(|i| &i.id == id) {
                Some(iface) => on_interface(iface),
                None => err("That network interface is not available for searching."),
            },
            Scope::Cidr(text) => {
                let hosts = HostSet::parse(text).map_err(DiscoveryError)?;
                let loopback = hosts.first().is_some_and(|a| a.is_loopback());
                if !(self.allow_loopback && loopback) {
                    net::check_hosts(&hosts, from_portal).map_err(DiscoveryError)?;
                }
                // A range on a local interface is also searched by multicast,
                // which finds cameras whose unicast probe is filtered.
                let local = hosts.first().and_then(|first| {
                    interfaces.iter().find_map(|iface| {
                        let address: Ipv4Addr = iface.address.parse().ok()?;
                        let net = Ipv4Net::new(address, iface.prefix);
                        net.contains(first).then_some((address, net))
                    })
                });
                Ok(Plan {
                    label: hosts.label(),
                    multicast_from: local.map(|(address, _)| address),
                    local_net: local.map(|(_, net)| net),
                    accept: hosts.clone(),
                    unicast: hosts,
                })
            }
        }
    }

    /// Start a search. One search runs at a time on a node.
    pub fn start(&self, scope: Scope, from_portal: bool) -> Result<ScanResults> {
        let plan = self.plan(&scope, from_portal)?;
        let scan_id = opaque_id();
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut inner = self.lock();
            prune(&mut inner);
            if inner
                .scans
                .values()
                .any(|s| s.status == ScanStatus::Running)
            {
                return err("A network search is already running on this recorder. Wait for it to finish or cancel it.");
            }
            inner.scans.insert(
                scan_id.clone(),
                Scan {
                    id: scan_id.clone(),
                    scope: plan.label.clone(),
                    status: ScanStatus::Running,
                    probed: 0,
                    total: u32::try_from(plan.unicast.count()).unwrap_or(u32::MAX),
                    candidates: Vec::new(),
                    error: None,
                    cancelled: Arc::clone(&cancelled),
                    started: Instant::now(),
                },
            );
        }
        tracing::info!(scan_id = %scan_id, scope = %plan.label, "Camera search started");
        let inner = Arc::clone(&self.inner);
        let ports = self.ports;
        let id = scan_id.clone();
        tokio::spawn(async move {
            let outcome = run_scan(&inner, &id, &plan, ports, &cancelled).await;
            let mut guard = lock(&inner);
            if let Some(scan) = guard.scans.get_mut(&id) {
                scan.status = match (&outcome, cancelled.load(Ordering::SeqCst)) {
                    (_, true) => ScanStatus::Cancelled,
                    (Ok(()), false) => ScanStatus::Complete,
                    (Err(_), false) => ScanStatus::Failed,
                };
                scan.error = outcome.err().map(|e| e.0);
                tracing::info!(scan_id = %id, found = scan.candidates.len(), status = ?scan.status, "Camera search finished");
            }
        });
        self.results(&scan_id, &[])
    }

    pub fn cancel(&self, scan_id: &str) -> Result<()> {
        let inner = self.lock();
        match inner.scans.get(scan_id) {
            Some(scan) => {
                scan.cancelled.store(true, Ordering::SeqCst);
                Ok(())
            }
            None => err("That search has expired."),
        }
    }

    /// A search and its candidates, matched against `cameras` as they are now.
    pub fn results(&self, scan_id: &str, cameras: &[CameraConfig]) -> Result<ScanResults> {
        let mut inner = self.lock();
        prune(&mut inner);
        let Some(scan) = inner.scans.get(scan_id) else {
            return err("That search has expired. Search again.");
        };
        let candidates = scan
            .candidates
            .iter()
            .filter_map(|id| inner.candidates.get(id))
            .map(|candidate| present(candidate, cameras))
            .collect();
        Ok(ScanResults {
            scan_id: scan.id.clone(),
            status: scan.status,
            scope: scan.scope.clone(),
            probed: scan.probed,
            total: scan.total,
            candidates,
            error: scan.error.clone(),
        })
    }

    /// Look up one device by address, as when an operator types it in.
    pub async fn lookup(
        &self,
        address: &str,
        port: Option<u16>,
        from_portal: bool,
        cameras: &[CameraConfig],
    ) -> Result<DiscoveryCandidate> {
        let ip = resolve(address, from_portal, self.allow_loopback).await?;
        let mut found = probe_one(ip, self.ports.ws_discovery).await;
        let mut device_urls = found
            .as_ref()
            .map(|f| f.device_urls.clone())
            .unwrap_or_default();
        for url in onvif::device_url_candidates(ip, port) {
            if !device_urls.contains(&url) {
                device_urls.push(url);
            }
        }
        let probe = found.take().unwrap_or(ProbeMatch {
            endpoint_reference: None,
            device_urls: Vec::new(),
            name: None,
            hardware: None,
            manufacturer: None,
            video: true,
        });
        let mut candidate = new_candidate(opaque_id(), ip, probe, device_urls);
        candidate.hikvision_hint = port == Some(onvif::HIKVISION_SDK_PORT);
        enrich(&mut candidate).await;
        // ONVIF may be off; known stream paths are offered instead.
        settle(&mut candidate);
        let shown = present(&candidate, cameras);
        let mut inner = self.lock();
        prune(&mut inner);
        inner.candidates.insert(candidate.id.clone(), candidate);
        Ok(shown)
    }

    /// The streams of one candidate, with the credentials the operator gave
    /// for it. Credentials stay with this candidate only.
    pub async fn profiles(
        &self,
        candidate_id: &str,
        credentials: Option<Credentials>,
        include_urls: bool,
        cameras: &[CameraConfig],
    ) -> Result<(
        DiscoveryCandidate,
        Vec<StreamChoice>,
        Option<String>,
        Option<String>,
    )> {
        let mut candidate = self.candidate(candidate_id)?;
        if let Some(reason) = refusal(&candidate) {
            return err(reason);
        }
        let credentials = credentials.filter(|c| !c.username.is_empty());
        match fetch_streams(&candidate, credentials.as_ref()).await {
            Ok((info, streams)) => {
                if info != DeviceInfo::default() {
                    candidate.info = info;
                }
                candidate.streams = streams;
                candidate.credentials = credentials;
                candidate.reach = Reach::Ready;
            }
            Err(OnvifError::Unauthorized) => {
                return err(if credentials.is_some() {
                    "The camera refused that username and password."
                } else {
                    "The camera asks for a username and password."
                });
            }
            Err(error) => return err(error.to_string()),
        }
        candidate.expires = Instant::now() + CACHE_TTL;
        let identity = candidate.endpoint_reference.clone();
        let choices = candidate
            .streams
            .iter()
            .map(|stream| {
                let matched = match match_camera(
                    cameras,
                    identity
                        .as_ref()
                        .map(|epr| DiscoveryIdentity {
                            endpoint_reference: epr.clone(),
                            source: Some(stream.profile.token.clone()),
                        })
                        .as_ref(),
                    Some(&stream.url),
                    None,
                ) {
                    Match::Exact(camera) => Some(camera),
                    _ => None,
                };
                (stream_choice(stream, include_urls), matched)
            })
            .collect::<Vec<_>>();
        let (main, sub) = suggested_streams(&candidate.streams);
        let shown = present(&candidate, cameras);
        self.lock()
            .candidates
            .insert(candidate.id.clone(), candidate);
        let choices = choices
            .into_iter()
            .map(|(mut choice, matched)| {
                if let Some(camera) = matched {
                    choice.name = format!("{} (added as {})", choice.name, camera.name);
                }
                choice
            })
            .collect();
        Ok((shown, choices, main, sub))
    }

    /// The stream URL with credentials, for a connection test.
    pub fn stream_for_test(
        &self,
        candidate_id: &str,
        stream_id: &str,
    ) -> Result<(String, Option<String>, Option<String>)> {
        let candidate = self.candidate(candidate_id)?;
        let stream = candidate
            .streams
            .iter()
            .find(|s| s.stream_id == stream_id)
            .ok_or_else(|| DiscoveryError("Choose a stream first.".into()))?;
        let (username, password_b64) = credentials_fields(candidate.credentials.as_ref());
        Ok((stream.url.clone(), username, password_b64))
    }

    /// The camera configuration for a chosen candidate and streams. The
    /// caller adds it through the normal add validation.
    pub fn build_camera(
        &self,
        candidate_id: &str,
        main_stream_id: &str,
        sub_stream_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<CameraConfig> {
        let candidate = self.candidate(candidate_id)?;
        if candidate.streams.is_empty() {
            return err("Load the camera's streams first.");
        }
        let main = candidate
            .streams
            .iter()
            .find(|s| s.stream_id == main_stream_id)
            .ok_or_else(|| {
                DiscoveryError("That main stream is not one this camera offers.".into())
            })?;
        let sub = match sub_stream_id.filter(|id| !id.is_empty() && *id != main_stream_id) {
            Some(id) => Some(
                candidate
                    .streams
                    .iter()
                    .find(|s| s.stream_id == id)
                    .ok_or_else(|| {
                        DiscoveryError("That grid stream is not one this camera offers.".into())
                    })?,
            ),
            None => None,
        };
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| n.chars().take(64).collect::<String>())
            .or_else(|| label_of(&candidate))
            .unwrap_or_else(|| format!("Camera {}", candidate.address));
        let mut camera = CameraConfig::new(name, main.url.clone());
        camera.sub_url = sub.map(|s| s.url.clone());
        let (username, password_b64) = credentials_fields(candidate.credentials.as_ref());
        camera.username = username;
        camera.password_b64 = password_b64;
        // Main first, then the grid stream, then the rest.
        let mut ordered: Vec<&CachedStream> = vec![main];
        ordered.extend(sub);
        ordered.extend(candidate.streams.iter().filter(|s| {
            s.stream_id != main.stream_id && sub.map_or(true, |sub| s.stream_id != sub.stream_id)
        }));
        camera.streams = ordered
            .into_iter()
            .map(|s| CameraStream {
                id: String::new(),
                name: s.profile.name.clone(),
                url: s.url.clone(),
                width: s.profile.width,
                height: s.profile.height,
            })
            .collect();
        camera.normalize_stream_ids();
        camera.discovery = candidate
            .endpoint_reference
            .clone()
            .map(|epr| DiscoveryIdentity {
                endpoint_reference: epr,
                source: Some(main.profile.token.clone()),
            });
        // The device service that answered is tried first (see `promote`).
        camera.onvif =
            candidate
                .device_urls
                .first()
                .map(|device_url| nw_core::camera::OnvifEndpoint {
                    device_url: device_url.clone(),
                    events_url: None,
                    ptz_url: None,
                });
        Ok(camera)
    }

    /// Drop the credentials held for a candidate (after it was added).
    pub fn forget_credentials(&self, candidate_id: &str) {
        if let Some(candidate) = self.lock().candidates.get_mut(candidate_id) {
            candidate.credentials = None;
        }
    }

    fn candidate(&self, candidate_id: &str) -> Result<Candidate> {
        let mut inner = self.lock();
        prune(&mut inner);
        inner
            .candidates
            .get(candidate_id)
            .cloned()
            .ok_or_else(|| DiscoveryError("That search result has expired. Search again.".into()))
    }
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn prune(inner: &mut Inner) {
    let now = Instant::now();
    inner.candidates.retain(|_, c| c.expires > now);
    inner.scans.retain(|_, s| {
        s.status == ScanStatus::Running || now.duration_since(s.started) < CACHE_TTL
    });
    let mut finished: Vec<(Instant, String)> = inner
        .scans
        .values()
        .filter(|s| s.status != ScanStatus::Running)
        .map(|s| (s.started, s.id.clone()))
        .collect();
    finished.sort();
    let excess = finished.len().saturating_sub(MAX_FINISHED_SCANS);
    for (_, id) in finished.into_iter().take(excess) {
        inner.scans.remove(&id);
    }
}

fn credentials_fields(credentials: Option<&Credentials>) -> (Option<String>, Option<String>) {
    match credentials {
        Some(c) if !c.username.is_empty() => (
            Some(c.username.clone()),
            (!c.password.is_empty()).then(|| STANDARD.encode(c.password.as_bytes())),
        ),
        _ => (None, None),
    }
}

fn stream_choice(stream: &CachedStream, include_url: bool) -> StreamChoice {
    StreamChoice {
        stream_id: stream.stream_id.clone(),
        name: stream.profile.name.clone(),
        width: stream.profile.width,
        height: stream.profile.height,
        encoding: stream.profile.encoding.clone(),
        source: Some(stream.profile.token.clone()),
        url: include_url.then(|| stream.url.clone()),
    }
}

/// Main: the largest picture. Grid: the smallest of the others.
fn suggested_streams(streams: &[CachedStream]) -> (Option<String>, Option<String>) {
    let area = |s: &&CachedStream| {
        u64::from(s.profile.width.unwrap_or(0)) * u64::from(s.profile.height.unwrap_or(0))
    };
    // Ties keep the device's order: its first profile is its main stream, the
    // next its grid stream. Sizes are unknown for maker stream paths (ONVIF
    // off) and some ONVIF profiles: the grid stream was dropped, so the grid
    // decoded every camera's main stream.
    let main = streams.iter().rev().max_by_key(area);
    let sub = streams
        .iter()
        .filter(|s| Some(s.stream_id.as_str()) != main.map(|m| m.stream_id.as_str()))
        .min_by_key(area);
    (
        main.map(|s| s.stream_id.clone()),
        sub.map(|s| s.stream_id.clone()),
    )
}

/// The candidate as a UI sees it, matched against the cameras configured now.
fn present(candidate: &Candidate, cameras: &[CameraConfig]) -> DiscoveryCandidate {
    let address = candidate.address.to_string();
    let device = candidate
        .endpoint_reference
        .as_ref()
        .map(|epr| DiscoveryIdentity {
            endpoint_reference: epr.clone(),
            source: None,
        });
    let mut matched: Option<CameraRef> = None;
    let mut state = match candidate.reach {
        Reach::Ready => CandidateState::Ready,
        Reach::CredentialsNeeded => CandidateState::CredentialsNeeded,
        Reach::Unavailable => CandidateState::Unavailable,
    };
    // A stream already read from the device can prove an exact match.
    let exact = candidate.streams.iter().find_map(|stream| {
        let identity = candidate
            .endpoint_reference
            .as_ref()
            .map(|epr| DiscoveryIdentity {
                endpoint_reference: epr.clone(),
                source: Some(stream.profile.token.clone()),
            });
        match match_camera(cameras, identity.as_ref(), Some(&stream.url), None) {
            Match::Exact(camera) => Some(camera),
            _ => None,
        }
    });
    if let Some(camera) = exact {
        state = CandidateState::AlreadyAdded;
        matched = Some(camera);
    } else {
        match match_camera(cameras, device.as_ref(), None, Some(&address)) {
            Match::Exact(camera) => {
                state = CandidateState::AlreadyAdded;
                matched = Some(camera);
            }
            Match::Possible(camera) => {
                state = CandidateState::PossibleMatch;
                matched = Some(camera);
            }
            Match::None => {}
        }
    }
    let found_by = [
        (
            candidate.origin == Origin::Onvif || candidate.onvif_answered,
            "onvif",
        ),
        (candidate.sadp.is_some(), "sadp"),
        (
            candidate.origin == Origin::Rtsp || candidate.rtsp.is_some(),
            "rtsp",
        ),
    ]
    .into_iter()
    .filter(|(found, _)| *found)
    .map(|(_, how)| how.to_owned())
    .collect();
    let sadp = candidate.sadp.as_ref();
    DiscoveryCandidate {
        candidate_id: candidate.id.clone(),
        found_by,
        serial: sadp.and_then(|d| d.serial.clone()),
        mac: sadp.and_then(|d| d.mac.clone()),
        activated: sadp.and_then(|d| d.activated),
        stream_url_needed: stream_url_needed(candidate),
        note: note_of(candidate),
        address,
        manufacturer: candidate.info.manufacturer.clone(),
        model: candidate
            .info
            .model
            .clone()
            .or_else(|| candidate.hardware.clone()),
        name: candidate.name.clone(),
        firmware: candidate.info.firmware.clone(),
        state,
        matched_camera: matched,
        endpoint_reference: candidate.endpoint_reference.clone(),
    }
}

fn new_candidate(
    id: String,
    address: Ipv4Addr,
    probe: ProbeMatch,
    device_urls: Vec<String>,
) -> Candidate {
    Candidate {
        id,
        address,
        endpoint_reference: probe.endpoint_reference,
        device_urls,
        name: probe.name,
        hardware: probe.hardware,
        info: DeviceInfo {
            manufacturer: probe.manufacturer,
            ..DeviceInfo::default()
        },
        reach: Reach::Unavailable,
        hikvision_hint: false,
        streams: Vec::new(),
        credentials: None,
        expires: Instant::now() + CACHE_TTL,
        origin: Origin::Onvif,
        onvif_answered: false,
        sadp: None,
        rtsp: None,
        outside_network: false,
    }
}

fn blank_probe() -> ProbeMatch {
    ProbeMatch {
        endpoint_reference: None,
        device_urls: Vec::new(),
        name: None,
        hardware: None,
        manufacturer: None,
        video: true,
    }
}

/// The ONVIF device service URL of `address` on HTTP `port`.
fn device_url(address: Ipv4Addr, port: u16) -> String {
    if port == 80 {
        format!("http://{address}/onvif/device_service")
    } else {
        format!("http://{address}:{port}/onvif/device_service")
    }
}

/// A Hikvision device SADP found. Its ONVIF service is asked once, on the
/// HTTP port it reported; without ONVIF it is added with Hikvision's stream
/// paths.
fn sadp_candidate(id: String, address: Ipv4Addr, device: SadpDevice, outside: bool) -> Candidate {
    let mut candidate = new_candidate(id, address, blank_probe(), Vec::new());
    candidate.origin = Origin::Sadp;
    candidate.hikvision_hint = true;
    candidate.outside_network = outside;
    candidate.info = DeviceInfo {
        manufacturer: Some("Hikvision".into()),
        model: device.model.clone(),
        firmware: device.firmware.clone(),
    };
    if !outside && device.activated != Some(false) {
        candidate.device_urls = vec![device_url(address, device.http_port.unwrap_or(80))];
    }
    candidate.sadp = Some(device);
    candidate
}

/// A device whose RTSP port answered. Its ONVIF service is asked once on
/// the usual port, in case only its discovery is off.
fn rtsp_candidate(id: String, device: RtspDevice, onvif_http: u16) -> Candidate {
    let mut candidate = new_candidate(id, device.address, blank_probe(), Vec::new());
    candidate.origin = Origin::Rtsp;
    candidate.info.manufacturer = device
        .server
        .as_deref()
        .and_then(rtsp::vendor_of)
        .map(str::to_owned);
    candidate.device_urls = vec![device_url(device.address, onvif_http)];
    candidate.rtsp = Some(device);
    candidate
}

/// What one address's results from several methods add up to: the details
/// of the method that knows most, completed by the others.
fn merge(existing: Candidate, new: Candidate) -> Candidate {
    let id = existing.id.clone();
    let (mut base, other) = if new.origin >= existing.origin {
        (new, existing)
    } else {
        (existing, new)
    };
    base.id = id;
    base.endpoint_reference = base.endpoint_reference.or(other.endpoint_reference);
    for url in other.device_urls {
        if !base.device_urls.contains(&url) {
            base.device_urls.push(url);
        }
    }
    base.name = base.name.or(other.name);
    base.hardware = base.hardware.or(other.hardware);
    base.info = DeviceInfo {
        manufacturer: base.info.manufacturer.or(other.info.manufacturer),
        model: base.info.model.or(other.info.model),
        firmware: base.info.firmware.or(other.info.firmware),
    };
    base.hikvision_hint |= other.hikvision_hint;
    base.onvif_answered |= other.onvif_answered;
    if base.streams.is_empty() {
        base.streams = other.streams;
    }
    base.credentials = base.credentials.or(other.credentials);
    base.sadp = base.sadp.or(other.sadp);
    base.rtsp = base.rtsp.or(other.rtsp);
    // Only SADP sees devices outside the network; any other answer is from inside.
    base.outside_network &= other.outside_network;
    if other.reach.rank() > base.reach.rank() {
        base.reach = other.reach;
    }
    base.expires = base.expires.max(other.expires);
    settle(&mut base);
    base
}

/// Stream paths known for the maker, for a device whose ONVIF is off.
fn template_streams(candidate: &Candidate) -> Option<Vec<CachedStream>> {
    if candidate.hikvision_hint || is_hikvision(candidate) {
        Some(hikvision_streams(candidate.address))
    } else if is_maker(candidate, "dahua") {
        Some(numbered(onvif::dahua_profiles(candidate.address)))
    } else {
        None
    }
}

/// Answers RTSP only, from a maker whose stream paths are unknown.
fn stream_url_needed(candidate: &Candidate) -> bool {
    candidate.origin == Origin::Rtsp
        && !candidate.onvif_answered
        && candidate.streams.is_empty()
        && template_streams(candidate).is_none()
}

/// Why a candidate cannot be added from a search, if it cannot.
fn refusal(candidate: &Candidate) -> Option<String> {
    let blocked = candidate.sadp.as_ref().and_then(|d| d.activated) == Some(false)
        || candidate.outside_network
        || stream_url_needed(candidate);
    if blocked {
        note_of(candidate)
    } else {
        None
    }
}

/// The reach of a device after every method was asked: a device without
/// ONVIF is still added when its maker's stream paths are known.
fn settle(candidate: &mut Candidate) {
    if refusal(candidate).is_some() {
        candidate.reach = Reach::Unavailable;
    } else if candidate.reach == Reach::Unavailable && template_streams(candidate).is_some() {
        candidate.reach = Reach::CredentialsNeeded;
    }
}

/// A short explanation for the operator, when the result needs one.
fn note_of(candidate: &Candidate) -> Option<String> {
    let address = candidate.address;
    if candidate.sadp.as_ref().and_then(|d| d.activated) == Some(false) {
        return Some(format!(
            "Not activated yet: set its admin password on its web page (http://{address}) or with Hikvision SADP, then search again."
        ));
    }
    if candidate.outside_network {
        return Some(format!(
            "Its address {address} is outside this recorder's network. Give it an address on this network (with Hikvision SADP or its web page), then search again."
        ));
    }
    if stream_url_needed(candidate) {
        return Some(format!(
            "Answers RTSP but not ONVIF, so its stream address is unknown. Turn ONVIF on in the camera, or add it with its stream URL (rtsp://{address}:554/…)."
        ));
    }
    if !candidate.onvif_answered && candidate.streams.is_empty() && candidate.reach != Reach::Ready
    {
        let maker = if candidate.hikvision_hint || is_hikvision(candidate) {
            "Hikvision"
        } else if is_maker(candidate, "dahua") {
            "Dahua"
        } else {
            return None;
        };
        return Some(format!(
            "Its ONVIF service did not answer, so the usual {maker} stream addresses are used. Turn ONVIF on in the camera for exact stream details."
        ));
    }
    None
}

/// Ask a device who it is without credentials, and read its streams when it
/// lets anyone do so (they prove whether it is already added).
async fn enrich(candidate: &mut Candidate) {
    let Ok(mut client) = SoapClient::new() else {
        return;
    };
    for url in candidate.device_urls.clone() {
        match client.call(&url, "<tds:GetDeviceInformation/>", None).await {
            Ok(reply) => {
                if let Ok(info) = onvif::parse_device_information(&reply) {
                    candidate.info = DeviceInfo {
                        manufacturer: info.manufacturer.or(candidate.info.manufacturer.take()),
                        model: info.model.or(candidate.info.model.take()),
                        firmware: info.firmware.or(candidate.info.firmware.take()),
                    };
                }
                candidate.reach = Reach::Ready;
                candidate.onvif_answered = true;
                promote(candidate, &url);
                client.sync_clock(&url).await;
                if let Ok((_, streams)) = fetch_streams(candidate, None).await {
                    candidate.streams = streams;
                }
                return;
            }
            Err(OnvifError::Unauthorized) => {
                candidate.reach = Reach::CredentialsNeeded;
                candidate.onvif_answered = true;
                promote(candidate, &url);
                return;
            }
            Err(OnvifError::Disabled) => {
                candidate.reach = Reach::Unavailable;
                return;
            }
            Err(_) => continue,
        }
    }
}

/// Try the device URL that answered first from now on.
fn promote(candidate: &mut Candidate, url: &str) {
    if let Some(index) = candidate.device_urls.iter().position(|u| u == url) {
        let url = candidate.device_urls.remove(index);
        candidate.device_urls.insert(0, url);
    }
}

/// Device information and every stream of a candidate.
async fn fetch_streams(
    candidate: &Candidate,
    credentials: Option<&Credentials>,
) -> std::result::Result<(DeviceInfo, Vec<CachedStream>), OnvifError> {
    let mut client = SoapClient::new()?;
    let mut last = OnvifError::Unreachable("no device service answered".into());
    for device_url in &candidate.device_urls {
        client.sync_clock(device_url).await;
        let info = match client
            .call(device_url, "<tds:GetDeviceInformation/>", credentials)
            .await
        {
            Ok(reply) => onvif::parse_device_information(&reply).unwrap_or_default(),
            Err(OnvifError::Unreachable(e)) => {
                last = OnvifError::Unreachable(e);
                continue;
            }
            // ONVIF off, or (found without ONVIF) a web page that is not ONVIF.
            Err(OnvifError::Disabled) if template_streams(candidate).is_some() => {
                return Ok((
                    candidate.info.clone(),
                    template_streams(candidate).unwrap_or_default(),
                ));
            }
            Err(OnvifError::Invalid(_))
                if candidate.origin != Origin::Onvif && template_streams(candidate).is_some() =>
            {
                return Ok((
                    candidate.info.clone(),
                    template_streams(candidate).unwrap_or_default(),
                ));
            }
            Err(error) => return Err(error),
        };
        let media_url = match client
            .call(
                device_url,
                "<tds:GetCapabilities><tds:Category>Media</tds:Category></tds:GetCapabilities>",
                credentials,
            )
            .await
        {
            Ok(reply) => onvif::parse_media_url(&reply, candidate.address)
                .unwrap_or_else(|| device_url.clone()),
            Err(OnvifError::Unauthorized) => return Err(OnvifError::Unauthorized),
            Err(_) => device_url.clone(),
        };
        let profiles = onvif::parse_profiles(
            &client
                .call(&media_url, "<trt:GetProfiles/>", credentials)
                .await?,
        )?;
        let mut streams = Vec::new();
        for profile in profiles {
            let Ok(reply) = client
                .call(
                    &media_url,
                    &onvif::stream_uri_body(&profile.token),
                    credentials,
                )
                .await
            else {
                continue;
            };
            if let Some(url) = onvif::parse_stream_uri(&reply, candidate.address) {
                streams.push(CachedStream {
                    stream_id: format!("s{}", streams.len() + 1),
                    profile,
                    url,
                });
            }
        }
        if streams.is_empty() {
            return Err(OnvifError::Invalid(
                "the camera offered no RTSP streams".into(),
            ));
        }
        return Ok((info, streams));
    }
    if let Some(streams) = template_streams(candidate) {
        return Ok((candidate.info.clone(), streams));
    }
    Err(last)
}

fn is_hikvision(candidate: &Candidate) -> bool {
    is_maker(candidate, "hikvision")
}

fn is_maker(candidate: &Candidate, maker: &str) -> bool {
    [&candidate.info.manufacturer, &candidate.name]
        .into_iter()
        .flatten()
        .any(|text| text.to_ascii_lowercase().contains(maker))
}

fn hikvision_streams(address: Ipv4Addr) -> Vec<CachedStream> {
    numbered(onvif::hikvision_profiles(address))
}

fn numbered(profiles: Vec<(MediaProfile, String)>) -> Vec<CachedStream> {
    profiles
        .into_iter()
        .enumerate()
        .map(|(index, (profile, url))| CachedStream {
            stream_id: format!("s{}", index + 1),
            profile,
            url,
        })
        .collect()
}

async fn resolve(address: &str, from_portal: bool, allow_loopback: bool) -> Result<Ipv4Addr> {
    let address = address.trim();
    if address.is_empty() || address.len() > 253 || address.contains(['/', '@', ' ', '?', '#']) {
        return err("Enter the camera's IP address or host name.");
    }
    let ip = match address.parse::<Ipv4Addr>() {
        Ok(ip) => ip,
        Err(_) => tokio::net::lookup_host((address, 80))
            .await
            .ok()
            .and_then(|mut addrs| {
                addrs.find_map(|a| match a {
                    SocketAddr::V4(v4) => Some(*v4.ip()),
                    SocketAddr::V6(_) => None,
                })
            })
            .ok_or_else(|| {
                DiscoveryError(format!("“{address}” could not be found on the network."))
            })?,
    };
    if !(allow_loopback && ip.is_loopback()) {
        net::check_scope(&Ipv4Net::new(ip, 32), from_portal).map_err(DiscoveryError)?;
    }
    Ok(ip)
}

fn probe_socket(multicast_from: Option<Ipv4Addr>) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    if let Some(iface) = multicast_from {
        socket.set_multicast_if_v4(&iface)?;
        socket.set_multicast_ttl_v4(2)?;
        // Keep loopback enabled so a camera simulator on this machine can
        // answer the same multicast search used for physical cameras. Our own
        // Probe packets are harmless: the receiver accepts only ProbeMatch.
        socket.set_multicast_loop_v4(true)?;
    }
    tokio::net::UdpSocket::from_std(socket.into())
}

/// A unicast probe of one address, for a typed-in device.
async fn probe_one(ip: Ipv4Addr, port: u16) -> Option<ProbeMatch> {
    let socket = probe_socket(None).ok()?;
    let ids = vec![opaque_id(), opaque_id()];
    for (id, video) in ids.iter().zip([true, false]) {
        let _ = socket
            .send_to(onvif::probe_message(id, video).as_bytes(), (ip, port))
            .await;
    }
    let mut buf = vec![0u8; 65_536];
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    loop {
        let received = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await;
        match received {
            Ok(Ok((len, SocketAddr::V4(from)))) if *from.ip() == ip => {
                if let Some(found) = onvif::parse_probe_match(&buf[..len], ip, &ids) {
                    return Some(found);
                }
            }
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
}

/// The search of an interface's network: multicast on it, and one by one
/// the whole network when it is small enough, else the /24 around the
/// interface's address.
fn interface_plan(address: Ipv4Addr, prefix: u8) -> Plan {
    let net = Ipv4Net::new(address, prefix);
    let max = u64::from(nw_core::camera_discovery::MAX_SCAN_ADDRESSES);
    let (unicast, label) = if net.host_count() <= max {
        (HostSet::from_net(net), net.to_string())
    } else {
        let near = Ipv4Net::new(address, 24);
        (
            HostSet::from_net(near),
            format!("{near} (the part of {net} around this recorder)"),
        )
    };
    Plan {
        label,
        multicast_from: Some(address),
        local_net: Some(net),
        accept: HostSet::from_net(net),
        unicast,
    }
}

/// What one discovery method heard.
#[derive(Debug)]
enum Heard {
    Onvif(Ipv4Addr, ProbeMatch),
    Sadp(Ipv4Addr, SadpDevice),
    Rtsp(RtspDevice),
}

/// The methods that reported an address so far, under its one candidate id.
struct Claim {
    id: String,
    onvif: bool,
    sadp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Sending,
    /// Every probe and inquiry sent.
    Sent,
    /// The RTSP port check finished too.
    Done,
}

fn set_probed(inner: &Mutex<Inner>, scan_id: &str, probed: u64) {
    if let Some(scan) = lock(inner).scans.get_mut(scan_id) {
        scan.probed = u32::try_from(probed).unwrap_or(u32::MAX).min(scan.total);
    }
}

/// Keep a candidate of a search, merged with what the search already knows
/// about its address (same candidate id).
fn upsert(inner: &Mutex<Inner>, scan_id: &str, candidate: Candidate) {
    let mut guard = lock(inner);
    let inner = &mut *guard;
    let Some(scan) = inner.scans.get_mut(scan_id) else {
        return;
    };
    let id = candidate.id.clone();
    let candidate = match inner.candidates.get(&id) {
        Some(existing) => merge(existing.clone(), candidate),
        None => candidate,
    };
    if !scan.candidates.contains(&id) {
        scan.candidates.push(id.clone());
    }
    inner.candidates.insert(id, candidate);
}

fn claim(claims: &mut HashMap<Ipv4Addr, Claim>, ip: Ipv4Addr) -> Option<&mut Claim> {
    if !claims.contains_key(&ip) && claims.len() >= MAX_CANDIDATES {
        return None;
    }
    Some(claims.entry(ip).or_insert_with(|| Claim {
        id: opaque_id(),
        onvif: false,
        sadp: false,
    }))
}

/// What to keep of an answer: a candidate, and whether to ask its ONVIF
/// service who it is first. One candidate per address, whichever methods
/// found it.
fn admit(
    heard: Heard,
    plan: &Plan,
    ports: &Ports,
    claims: &mut HashMap<Ipv4Addr, Claim>,
) -> Option<(Candidate, bool)> {
    match heard {
        Heard::Onvif(ip, found) => {
            if !plan.accept.contains(ip) && !ip.is_loopback() {
                return None;
            }
            let claim = claim(claims, ip)?;
            if claim.onvif {
                return None;
            }
            claim.onvif = true;
            let urls = found.device_urls.clone();
            Some((new_candidate(claim.id.clone(), ip, found, urls), true))
        }
        Heard::Sadp(ip, device) => {
            // SADP is heard across the whole link: a device configured for
            // another network is listed (it cannot be added until moved).
            let outside = if plan.accept.contains(ip) {
                false
            } else if plan.local_net.is_some_and(|net| !net.contains(ip)) && ip.is_private() {
                true
            } else {
                return None;
            };
            let claim = claim(claims, ip)?;
            if claim.sadp {
                return None;
            }
            claim.sadp = true;
            let candidate = sadp_candidate(claim.id.clone(), ip, device, outside);
            let ask = !candidate.device_urls.is_empty() && !claim.onvif;
            Some((candidate, ask))
        }
        Heard::Rtsp(device) => {
            if claims.contains_key(&device.address) || !plan.accept.contains(device.address) {
                return None;
            }
            let claim = claim(claims, device.address)?;
            Some((
                rtsp_candidate(claim.id.clone(), device, ports.onvif_http),
                true,
            ))
        }
    }
}

async fn listen_ws_discovery(
    socket: Arc<tokio::net::UdpSocket>,
    ids: Vec<String>,
    heard: tokio::sync::mpsc::Sender<Heard>,
) {
    let mut buf = vec![0u8; 65_536];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, SocketAddr::V4(from))) => {
                let ip = *from.ip();
                if let Some(found) = onvif::parse_probe_match(&buf[..len], ip, &ids) {
                    if heard.send(Heard::Onvif(ip, found)).await.is_err() {
                        return;
                    }
                }
            }
            Ok(_) => {}
            // Windows reports an ICMP "port unreachable" from a probed
            // address as a receive error: skip it.
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

async fn listen_sadp(socket: Arc<tokio::net::UdpSocket>, heard: tokio::sync::mpsc::Sender<Heard>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, SocketAddr::V4(from))) => {
                if let Some(device) = sadp::parse_reply(&buf[..len]) {
                    let ip = device.address.unwrap_or(*from.ip());
                    if heard.send(Heard::Sadp(ip, device)).await.is_err() {
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

/// Everything a search sends, in order: multicast WS-Discovery and SADP,
/// a WS-Discovery probe to each address, then (after the answers had time
/// to arrive) the RTSP port of each address nothing answered for.
struct Sender {
    inner: Arc<Mutex<Inner>>,
    scan_id: String,
    plan: Plan,
    ports: Ports,
    socket: Arc<tokio::net::UdpSocket>,
    ids: Vec<String>,
    sadp: Option<(Arc<tokio::net::UdpSocket>, SocketAddr)>,
    claimed: Arc<Mutex<HashSet<Ipv4Addr>>>,
    heard: tokio::sync::mpsc::Sender<Heard>,
    cancelled: Arc<AtomicBool>,
    phase: tokio::sync::watch::Sender<Phase>,
    rtsp_until: tokio::time::Instant,
}

impl Sender {
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Progress: the probes count half, the port checks the other half.
    fn report(&self, sent: u64, checked: u64) {
        let probed = if self.ports.rtsp.is_some() {
            (sent + checked) / 2
        } else {
            sent
        };
        set_probed(&self.inner, &self.scan_id, probed);
    }

    async fn run(self) {
        let inquiry = sadp::inquiry(&sadp::new_uuid());
        for _ in 0..2 {
            if self.plan.multicast_from.is_some() {
                for (id, video) in self.ids.iter().zip([true, false]) {
                    let _ = self
                        .socket
                        .send_to(
                            onvif::probe_message(id, video).as_bytes(),
                            (onvif::WS_DISCOVERY_GROUP, self.ports.ws_discovery),
                        )
                        .await;
                }
            }
            if let Some((socket, target)) = &self.sadp {
                let _ = socket.send_to(inquiry.as_bytes(), target).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let total = self.plan.unicast.count();
        let pause = Duration::from_secs(1) / PROBES_PER_SECOND;
        let probe = onvif::probe_message(&self.ids[0], true);
        for (index, host) in self.plan.unicast.hosts().enumerate() {
            if self.cancelled() {
                break;
            }
            let _ = self
                .socket
                .send_to(probe.as_bytes(), (host, self.ports.ws_discovery))
                .await;
            if index % 16 == 15 {
                self.report(index as u64 + 1, 0);
            }
            tokio::time::sleep(pause).await;
        }
        self.report(total, 0);
        let _ = self.phase.send(Phase::Sent);

        if let Some(port) = self.ports.rtsp {
            tokio::time::sleep(RTSP_SWEEP_GRACE).await;
            self.rtsp_sweep(port, total).await;
        }
        self.report(total, total);
        let _ = self.phase.send(Phase::Done);
    }

    async fn rtsp_sweep(&self, port: u16, total: u64) {
        let permits = Arc::new(tokio::sync::Semaphore::new(rtsp::CONCURRENCY));
        let checked = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut checks = tokio::task::JoinSet::new();
        for (index, host) in self.plan.unicast.hosts().enumerate() {
            if self.cancelled() || tokio::time::Instant::now() >= self.rtsp_until {
                break;
            }
            let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                break;
            };
            let answered = lock_set(&self.claimed).contains(&host);
            if answered {
                checked.fetch_add(1, Ordering::Relaxed);
            } else {
                let heard = self.heard.clone();
                let checked = Arc::clone(&checked);
                checks.spawn(async move {
                    let _permit = permit;
                    if let Some(device) = rtsp::check(host, port).await {
                        let _ = heard.send(Heard::Rtsp(device)).await;
                    }
                    checked.fetch_add(1, Ordering::Relaxed);
                });
            }
            if index % 8 == 7 {
                self.report(total, checked.load(Ordering::Relaxed));
            }
        }
        while checks.join_next().await.is_some() {
            self.report(total, checked.load(Ordering::Relaxed));
        }
    }
}

fn lock_set(set: &Mutex<HashSet<Ipv4Addr>>) -> MutexGuard<'_, HashSet<Ipv4Addr>> {
    set.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn run_scan(
    inner: &Arc<Mutex<Inner>>,
    scan_id: &str,
    plan: &Plan,
    ports: Ports,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let socket = Arc::new(
        probe_socket(plan.multicast_from)
            .map_err(|e| DiscoveryError(format!("Could not open a search socket: {e}")))?,
    );
    let ids: Vec<String> = (0..2).map(|_| opaque_id()).collect();
    let started = tokio::time::Instant::now();
    let hard_deadline = started + MAX_SCAN_DURATION;
    let (heard_tx, mut heard_rx) = tokio::sync::mpsc::channel::<Heard>(256);
    let mut listeners = tokio::task::JoinSet::new();
    listeners.spawn(listen_ws_discovery(
        Arc::clone(&socket),
        ids.clone(),
        heard_tx.clone(),
    ));

    // SADP: multicast only on a local network (or a test's loopback
    // responder). Best effort: a port that cannot be opened skips it.
    let sadp_target = ports
        .sadp
        .filter(|target| !target.ip().is_multicast() || plan.multicast_from.is_some());
    let sadp = sadp_target.and_then(|target| match probe_socket(plan.multicast_from) {
        Ok(socket) => Some((Arc::new(socket), target)),
        Err(error) => {
            tracing::debug!(%error, "SADP inquiry socket could not be opened; skipping SADP");
            None
        }
    });
    if let Some((socket, _)) = &sadp {
        listeners.spawn(listen_sadp(Arc::clone(socket), heard_tx.clone()));
        if let (Some(port), Some(iface)) = (ports.sadp_listen, plan.multicast_from) {
            match sadp::listener(iface, port) {
                Ok(listener) => {
                    listeners.spawn(listen_sadp(Arc::new(listener), heard_tx.clone()));
                }
                Err(error) => {
                    tracing::debug!(%error, port, "SADP group port could not be opened; hearing direct replies only");
                }
            }
        }
    }

    let claimed = Arc::new(Mutex::new(HashSet::new()));
    let (phase_tx, mut phase_rx) = tokio::sync::watch::channel(Phase::Sending);
    let sender = tokio::spawn(
        Sender {
            inner: Arc::clone(inner),
            scan_id: scan_id.to_owned(),
            plan: plan.clone(),
            ports,
            socket: Arc::clone(&socket),
            ids,
            sadp,
            claimed: Arc::clone(&claimed),
            heard: heard_tx,
            cancelled: Arc::clone(cancelled),
            phase: phase_tx,
            // Leave time for the devices found last to be asked who they are.
            rtsp_until: hard_deadline - Duration::from_secs(4),
        }
        .run(),
    );

    // Receive until a little after the last probe, asking each new device who it is.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(ENRICH_CONCURRENCY));
    let mut enrichers = tokio::task::JoinSet::new();
    let mut claims: HashMap<Ipv4Addr, Claim> = HashMap::new();
    let mut sent_at: Option<tokio::time::Instant> = None;
    let mut sending = true;
    let mut listen_until = hard_deadline;
    loop {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            Some(heard) = heard_rx.recv() => {
                let Some((mut candidate, ask)) = admit(heard, plan, &ports, &mut claims) else { continue };
                lock_set(&claimed).insert(candidate.address);
                if !ask {
                    settle(&mut candidate);
                    upsert(inner, scan_id, candidate);
                    continue;
                }
                let permit = Arc::clone(&semaphore);
                let inner = Arc::clone(inner);
                let scan_id = scan_id.to_owned();
                enrichers.spawn(async move {
                    let Ok(_permit) = permit.acquire().await else { return };
                    enrich(&mut candidate).await;
                    settle(&mut candidate);
                    upsert(&inner, &scan_id, candidate);
                });
            }
            changed = phase_rx.changed(), if sending => {
                let now = tokio::time::Instant::now();
                let phase = if changed.is_ok() { *phase_rx.borrow_and_update() } else { Phase::Done };
                match phase {
                    Phase::Sending => {}
                    Phase::Sent => sent_at = Some(now),
                    Phase::Done => {
                        sending = false;
                        listen_until = (sent_at.unwrap_or(now) + LISTEN_AFTER_LAST_PROBE)
                            .max(now)
                            .min(hard_deadline);
                    }
                }
            }
            _ = tokio::time::sleep_until(listen_until) => break,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
    sender.abort();
    listeners.abort_all();
    // Devices already answering finish their introduction, within the scan limit.
    while !cancelled.load(Ordering::SeqCst) {
        match tokio::time::timeout_at(
            hard_deadline + Duration::from_secs(10),
            enrichers.join_next(),
        )
        .await
        {
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    enrichers.abort_all();
    Ok(())
}

#[cfg(test)]
mod tests;
