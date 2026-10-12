//! Camera discovery: search the network for ONVIF cameras and add one.
//!
//! The node that owns the cameras does all of it; the desktop and the web
//! portal only ask. A search sends WS-Discovery probes (multicast on a local
//! interface, and one by one to each address of a typed range), asks every
//! device that answers who it is, and keeps the results for a few minutes
//! under opaque ids. Credentials are asked for one chosen device at a time
//! and kept only until it is added or the cache expires. Nothing here
//! persists a camera: `camera.discovery.add` builds the configuration and the
//! RPC handler adds it through the same validation as `camera.add`.

pub mod matchers;
pub mod net;
pub mod onvif;

use std::collections::HashMap;
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
use net::Ipv4Net;
use onvif::{Credentials, DeviceInfo, MediaProfile, OnvifError, ProbeMatch, SoapClient};

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
    /// A typed IPv4 range.
    Cidr(String),
}

/// A planned search: where to multicast, which range to probe one by one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    label: String,
    multicast_from: Option<Ipv4Addr>,
    accept: Ipv4Net,
    unicast: Option<Ipv4Net>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Ready,
    CredentialsNeeded,
    Unavailable,
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
    /// A typed port that means a Hikvision camera with ONVIF off may answer.
    hikvision_hint: bool,
    streams: Vec<CachedStream>,
    credentials: Option<Credentials>,
    expires: Instant,
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
    ws_discovery_port: u16,
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
            ws_discovery_port: onvif::WS_DISCOVERY_PORT,
            allow_loopback: false,
        }
    }

    #[cfg(test)]
    fn for_test(ws_discovery_port: u16) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            ws_discovery_port,
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
            let net = Ipv4Net::new(address, iface.prefix);
            Ok(Plan {
                label: net.to_string(),
                multicast_from: Some(address),
                accept: net,
                unicast: None,
            })
        };
        match scope {
            Scope::Current => match interfaces.iter().find(|i| i.primary) {
                Some(iface) => on_interface(iface),
                None => err("No local network was found to search (no active private IPv4 network). Enter a range such as 192.168.1.0/24 instead."),
            },
            Scope::Interface(id) => match interfaces.iter().find(|i| &i.id == id) {
                Some(iface) => on_interface(iface),
                None => err("That network interface is not available for searching."),
            },
            Scope::Cidr(text) => {
                let net = Ipv4Net::parse(text).map_err(DiscoveryError)?;
                if !(self.allow_loopback && net.network.is_loopback()) {
                    net::check_scope(&net, from_portal).map_err(DiscoveryError)?;
                }
                // A range on a local interface is also searched by multicast,
                // which finds cameras whose unicast probe is filtered.
                let local = interfaces.iter().find_map(|iface| {
                    let address: Ipv4Addr = iface.address.parse().ok()?;
                    Ipv4Net::new(address, iface.prefix)
                        .contains(net.network)
                        .then_some(address)
                });
                Ok(Plan {
                    label: net.to_string(),
                    multicast_from: local,
                    accept: net,
                    unicast: Some(net),
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
                    total: plan.unicast.map_or(0, |n| n.host_count() as u32),
                    candidates: Vec::new(),
                    error: None,
                    cancelled: Arc::clone(&cancelled),
                    started: Instant::now(),
                },
            );
        }
        tracing::info!(scan_id = %scan_id, scope = %plan.label, "Camera search started");
        let inner = Arc::clone(&self.inner);
        let port = self.ws_discovery_port;
        let id = scan_id.clone();
        tokio::spawn(async move {
            let outcome = run_scan(&inner, &id, &plan, port, &cancelled).await;
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
        let mut found = probe_one(ip, self.ws_discovery_port).await;
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
        let mut candidate = new_candidate(ip, probe, device_urls);
        candidate.hikvision_hint = port == Some(onvif::HIKVISION_SDK_PORT);
        enrich(&mut candidate).await;
        if candidate.reach == Reach::Unavailable && candidate.hikvision_hint {
            // ONVIF may be off; the Hikvision stream paths are offered instead.
            candidate.reach = Reach::CredentialsNeeded;
        }
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

/// Main: the largest picture. Grid: the smallest, if it is another stream.
fn suggested_streams(streams: &[CachedStream]) -> (Option<String>, Option<String>) {
    let area = |s: &&CachedStream| {
        u64::from(s.profile.width.unwrap_or(0)) * u64::from(s.profile.height.unwrap_or(0))
    };
    // Ties keep the device's order: its first profile is its main stream.
    let main = streams.iter().rev().max_by_key(area);
    let sub = streams
        .iter()
        .min_by_key(area)
        .filter(|s| Some(s.stream_id.as_str()) != main.map(|m| m.stream_id.as_str()));
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
    DiscoveryCandidate {
        candidate_id: candidate.id.clone(),
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

fn new_candidate(address: Ipv4Addr, probe: ProbeMatch, device_urls: Vec<String>) -> Candidate {
    Candidate {
        id: opaque_id(),
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
    }
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
                        ..info
                    };
                }
                candidate.reach = Reach::Ready;
                promote(candidate, &url);
                client.sync_clock(&url).await;
                if let Ok((_, streams)) = fetch_streams(candidate, None).await {
                    candidate.streams = streams;
                }
                return;
            }
            Err(OnvifError::Unauthorized) => {
                candidate.reach = Reach::CredentialsNeeded;
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
            Err(OnvifError::Disabled) if candidate.hikvision_hint || is_hikvision(candidate) => {
                return Ok((candidate.info.clone(), hikvision_streams(candidate.address)));
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
    if candidate.hikvision_hint {
        return Ok((candidate.info.clone(), hikvision_streams(candidate.address)));
    }
    Err(last)
}

fn is_hikvision(candidate: &Candidate) -> bool {
    [&candidate.info.manufacturer, &candidate.name]
        .into_iter()
        .flatten()
        .any(|text| text.to_ascii_lowercase().contains("hikvision"))
}

fn hikvision_streams(address: Ipv4Addr) -> Vec<CachedStream> {
    onvif::hikvision_profiles(address)
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

async fn run_scan(
    inner: &Arc<Mutex<Inner>>,
    scan_id: &str,
    plan: &Plan,
    port: u16,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let socket = Arc::new(
        probe_socket(plan.multicast_from)
            .map_err(|e| DiscoveryError(format!("Could not open a search socket: {e}")))?,
    );
    let ids: Vec<String> = (0..2).map(|_| opaque_id()).collect();
    let started = tokio::time::Instant::now();
    let hard_deadline = started + MAX_SCAN_DURATION;
    let (sent_tx, mut sent_rx) = tokio::sync::watch::channel(false);

    // Send: multicast twice, then each address of the range, paced.
    let sender = {
        let socket = Arc::clone(&socket);
        let ids = ids.clone();
        let plan = plan.clone();
        let inner = Arc::clone(inner);
        let scan_id = scan_id.to_owned();
        let cancelled = Arc::clone(cancelled);
        tokio::spawn(async move {
            if plan.multicast_from.is_some() {
                for _ in 0..2 {
                    for (id, video) in ids.iter().zip([true, false]) {
                        let _ = socket
                            .send_to(
                                onvif::probe_message(id, video).as_bytes(),
                                (onvif::WS_DISCOVERY_GROUP, port),
                            )
                            .await;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            if let Some(net) = plan.unicast {
                let pause = Duration::from_secs(1) / PROBES_PER_SECOND;
                let probe = onvif::probe_message(&ids[0], true);
                for (index, host) in net.hosts().enumerate() {
                    if cancelled.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = socket.send_to(probe.as_bytes(), (host, port)).await;
                    if index % 16 == 15 {
                        if let Some(scan) = lock(&inner).scans.get_mut(&scan_id) {
                            scan.probed = index as u32 + 1;
                        }
                    }
                    tokio::time::sleep(pause).await;
                }
                if let Some(scan) = lock(&inner).scans.get_mut(&scan_id) {
                    scan.probed = scan.total;
                }
            }
            let _ = sent_tx.send(true);
        })
    };

    // Receive until a little after the last probe, asking each new device who it is.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(ENRICH_CONCURRENCY));
    let mut enrichers = tokio::task::JoinSet::new();
    let mut seen: Vec<String> = Vec::new();
    let mut buf = vec![0u8; 65_536];
    let mut listen_until = hard_deadline;
    loop {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let Ok((len, SocketAddr::V4(from))) = received else { continue };
                let ip = *from.ip();
                if !plan.accept.contains(ip) && !ip.is_loopback() {
                    continue;
                }
                let Some(found) = onvif::parse_probe_match(&buf[..len], ip, &ids) else { continue };
                let key = found.endpoint_reference.clone().unwrap_or_else(|| ip.to_string());
                if seen.contains(&key) || seen.len() >= MAX_CANDIDATES {
                    continue;
                }
                seen.push(key);
                let urls = found.device_urls.clone();
                let mut candidate = new_candidate(ip, found, urls);
                let permit = Arc::clone(&semaphore);
                let inner = Arc::clone(inner);
                let scan_id = scan_id.to_owned();
                enrichers.spawn(async move {
                    let Ok(_permit) = permit.acquire().await else { return };
                    enrich(&mut candidate).await;
                    let mut guard = lock(&inner);
                    let id = candidate.id.clone();
                    guard.candidates.insert(id.clone(), candidate);
                    if let Some(scan) = guard.scans.get_mut(&scan_id) {
                        scan.candidates.push(id);
                    }
                });
            }
            changed = sent_rx.changed(), if !*sent_rx.borrow() => {
                if changed.is_ok() {
                    listen_until = (tokio::time::Instant::now() + LISTEN_AFTER_LAST_PROBE).min(hard_deadline);
                }
            }
            _ = tokio::time::sleep_until(listen_until) => break,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
    sender.abort();
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
