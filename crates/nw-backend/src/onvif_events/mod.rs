//! The camera's own events over ONVIF: motion, intrusion, line crossing,
//! tamper, alarm inputs. They cost no decoding: one pull-point subscription
//! per camera (`CreatePullPointSubscription`, then `PullMessages` in a loop,
//! `Renew` before it expires).
//!
//! [`OnvifEvents::run`] keeps one subscriber per camera whose motion source
//! includes the camera (`motion.source` `onvif` or `both`), reconciling with
//! the configuration every few seconds or at once after `motion.configure`.
//! A subscriber that finds the camera's ONVIF service stores it on the
//! camera ([`OnvifEndpoint`]), so the search runs once.

pub mod parse;
pub mod push;
mod subscriber;
pub mod tracker;

use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use nw_core::camera::{CameraConfig, CameraId, OnvifEndpoint};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, watch, Notify, RwLock};

use crate::app_state::AppState;
use crate::discovery::onvif::{self, Credentials, OnvifError, SoapClient};
use crate::events::EventHub;

const RECONCILE_EVERY: Duration = Duration::from_secs(5);

/// A subscriber's state, for `motion.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionState {
    Connecting,
    Subscribed,
    /// The camera has no ONVIF event service (or no pull points).
    Unsupported,
    Unauthorized,
    /// ONVIF is turned off on the camera.
    Disabled,
    Unreachable,
    Error,
}

/// How a subscribed camera's events reach NeuroWatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The camera sends each event here as it happens.
    Push,
    /// NeuroWatch asks the camera for its events (a held-open request).
    Pull,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OnvifStatus {
    pub state: SubscriptionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// Why events are pulled rather than pushed, when push was tried.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Topics the camera has sent (classified as events).
    pub topics: BTreeSet<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_ms: Option<i64>,
}

impl Default for OnvifStatus {
    fn default() -> Self {
        Self {
            state: SubscriptionState::Connecting,
            detail: None,
            delivery: None,
            note: None,
            topics: BTreeSet::new(),
            last_event_ms: None,
        }
    }
}

/// What makes a subscriber restart: its camera's address, credentials and
/// motion source. The stored endpoint is left out, or storing it would
/// restart the subscriber that found it.
fn fingerprint(cam: &CameraConfig) -> String {
    format!(
        "{}\n{:?}\n{:?}\n{:?}",
        cam.url,
        cam.username,
        cam.password_b64,
        cam.motion.effective_source()
    )
}

struct Running {
    fingerprint: String,
    stop: watch::Sender<bool>,
}

/// Subscribers and their status.
#[derive(Default)]
pub struct OnvifEvents {
    status: Mutex<HashMap<CameraId, OnvifStatus>>,
    kick: Notify,
}

impl OnvifEvents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile now (a camera's motion settings changed).
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub fn status(&self, camera_id: &str) -> Option<OnvifStatus> {
        self.status.lock().ok()?.get(camera_id).cloned()
    }

    fn update(&self, camera_id: &str, change: impl FnOnce(&mut OnvifStatus)) {
        if let Ok(mut status) = self.status.lock() {
            change(status.entry(camera_id.to_owned()).or_default());
        }
    }

    fn forget(&self, camera_id: &str) {
        if let Ok(mut status) = self.status.lock() {
            status.remove(camera_id);
        }
    }

    /// Keep subscribers in line with the configuration until shutdown.
    pub async fn run(
        self: Arc<Self>,
        state: Arc<RwLock<AppState>>,
        hub: Arc<EventHub>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let (found_tx, mut found_rx) = mpsc::unbounded_channel::<(CameraId, OnvifEndpoint)>();
        let mut running: HashMap<CameraId, Running> = HashMap::new();
        // One listener for every camera's pushed events (0: pull only).
        let push_port = state.read().await.config.events.push_port;
        let push = if push_port == 0 {
            None
        } else {
            push::PushListener::start(push_port).await
        };
        loop {
            let wanted: Vec<CameraConfig> = {
                let st = state.read().await;
                st.config
                    .cameras
                    .iter()
                    .filter(|cam| cam.motion.effective_source().uses_onvif())
                    .cloned()
                    .collect()
            };
            running.retain(|id, run| {
                let keep = wanted
                    .iter()
                    .any(|cam| &cam.id == id && fingerprint(cam) == run.fingerprint);
                if !keep {
                    let _ = run.stop.send(true);
                    self.forget(id);
                    tracing::info!(cam_id = %id, "Camera events: subscriber stopped");
                }
                keep
            });
            for cam in wanted {
                if running.contains_key(&cam.id) {
                    continue;
                }
                let (stop, stopped) = watch::channel(false);
                running.insert(
                    cam.id.clone(),
                    Running {
                        fingerprint: fingerprint(&cam),
                        stop,
                    },
                );
                self.update(&cam.id, |status| *status = OnvifStatus::default());
                tracing::info!(cam_id = %cam.id, "Camera events: subscriber started");
                tokio::spawn(subscriber::run(
                    cam,
                    Arc::clone(&hub),
                    Arc::clone(&self),
                    push.clone(),
                    stopped,
                    found_tx.clone(),
                ));
            }
            tokio::select! {
                _ = tokio::time::sleep(RECONCILE_EVERY) => {}
                _ = self.kick.notified() => {}
                Some((id, endpoint)) = found_rx.recv() => store_endpoint(&state, &id, endpoint).await,
                _ = shutdown.recv() => break,
            }
        }
        for (_, run) in running {
            let _ = run.stop.send(true);
        }
    }
}

/// Remember where a camera answers ONVIF.
async fn store_endpoint(state: &Arc<RwLock<AppState>>, camera_id: &str, endpoint: OnvifEndpoint) {
    let mut st = state.write().await;
    let Some(cam) = st.config.cameras.iter_mut().find(|c| c.id == camera_id) else {
        return;
    };
    if cam.onvif.as_ref() == Some(&endpoint) {
        return;
    }
    cam.onvif = Some(endpoint);
    tracing::info!(cam_id = %camera_id, "Camera's ONVIF service stored");
    if let Err(error) = st.save_config() {
        tracing::warn!(%error, "Could not save the camera's ONVIF service");
    }
}

/// The camera's credentials: its username and password, else any in its URL.
pub fn credentials(cam: &CameraConfig) -> Option<Credentials> {
    if let Some(username) = cam.username.as_deref().filter(|u| !u.is_empty()) {
        let password = cam
            .password_b64
            .as_deref()
            .and_then(|p| STANDARD.decode(p).ok())
            .and_then(|p| String::from_utf8(p).ok())
            .unwrap_or_default();
        return Some(Credentials {
            username: username.to_owned(),
            password,
        });
    }
    let (_, rest) = cam.url.split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    let (userinfo, _) = authority.rsplit_once('@')?;
    let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    Some(Credentials {
        username: percent_decode(username),
        password: percent_decode(password),
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The host of a URL, without credentials or port.
pub fn url_host(url: &str) -> Option<String> {
    let (_, rest) = url.trim().split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        host_port.rsplit_once(':').map_or(host_port, |(h, _)| h)
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// The port a URL names, if any.
pub fn url_port(url: &str) -> Option<u16> {
    let (_, rest) = url.trim().split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let port = match host_port.strip_prefix('[') {
        Some(v6) => v6.split_once("]:")?.1,
        None => host_port.rsplit_once(':')?.1,
    };
    port.parse().ok()
}

/// The camera's IPv4 address, from its stream URL (its ONVIF service URL is
/// pinned to the same address).
async fn camera_address(cam: &CameraConfig) -> Result<Ipv4Addr, OnvifError> {
    let host = url_host(&cam.url)
        .or_else(|| {
            cam.onvif
                .as_ref()
                .and_then(|endpoint| url_host(&endpoint.device_url))
        })
        .ok_or_else(|| OnvifError::Invalid("the camera's address could not be read".into()))?;
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    let addresses = tokio::net::lookup_host((host.as_str(), 80))
        .await
        .map_err(|e| OnvifError::Unreachable(e.to_string()))?;
    let found = addresses.into_iter().find_map(|addr| match addr.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    });
    found.ok_or_else(|| OnvifError::Unreachable("the camera has no IPv4 address".into()))
}

const GET_CAPABILITIES: &str =
    "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>";

/// A camera's ONVIF services, found from its stored endpoint or address.
pub struct Detected {
    pub endpoint: OnvifEndpoint,
    pub services: parse::Services,
    pub address: Ipv4Addr,
    pub client: SoapClient,
}

/// Find where the camera answers ONVIF and what it offers.
pub async fn detect(cam: &CameraConfig) -> Result<Detected, OnvifError> {
    let address = camera_address(cam).await?;
    let creds = credentials(cam);
    let mut urls: Vec<String> = cam
        .onvif
        .as_ref()
        .and_then(|endpoint| onvif::pin_http_url(&endpoint.device_url, address))
        .into_iter()
        .collect();
    // Standard paths on the stored service's port first (a port typed when
    // the camera was added), then the usual ports.
    let stored_port = cam
        .onvif
        .as_ref()
        .and_then(|endpoint| url_port(&endpoint.device_url));
    for url in onvif::device_url_candidates(address, stored_port) {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    let mut client = SoapClient::new()?;
    let mut last = OnvifError::Unreachable("no ONVIF service answered".into());
    for url in urls {
        match client.call(&url, "<tds:GetSystemDateAndTime/>", None).await {
            Ok(_) | Err(OnvifError::Unauthorized) => {}
            Err(OnvifError::Disabled) => return Err(OnvifError::Disabled),
            Err(error) => {
                last = error;
                continue;
            }
        }
        client.sync_clock(&url).await;
        let reply = client.call(&url, GET_CAPABILITIES, creds.as_ref()).await?;
        let services = parse::services(&reply, address).unwrap_or_default();
        return Ok(Detected {
            endpoint: OnvifEndpoint {
                device_url: url,
                events_url: services.events_url.clone(),
                ptz_url: services.ptz_url.clone(),
            },
            services,
            address,
            client,
        });
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_come_without_credentials_or_port() {
        assert_eq!(
            url_host("rtsp://admin:p@ss@192.168.1.64:554/Streaming/101").as_deref(),
            Some("192.168.1.64")
        );
        assert_eq!(
            url_host("http://cam.local/onvif").as_deref(),
            Some("cam.local")
        );
        assert_eq!(
            url_host("rtsp://[fe80::1]:554/").as_deref(),
            Some("fe80::1")
        );
        assert_eq!(url_host("nonsense"), None);
        assert_eq!(
            url_port("http://10.0.0.2:8899/onvif/device_service"),
            Some(8899)
        );
        assert_eq!(url_port("http://10.0.0.2/onvif"), None);
        assert_eq!(url_port("rtsp://u:p@[fe80::1]:554/"), Some(554));
    }

    #[test]
    fn credentials_prefer_the_fields_then_the_url() {
        let mut cam = CameraConfig::new("Cam", "rtsp://url%40user:p%3Ass@10.0.0.2/live");
        let from_url = credentials(&cam).expect("from url");
        assert_eq!(
            (from_url.username.as_str(), from_url.password.as_str()),
            ("url@user", "p:ss")
        );
        cam.username = Some("admin".into());
        cam.password_b64 = Some(STANDARD.encode("secret"));
        let from_fields = credentials(&cam).expect("from fields");
        assert_eq!(
            (from_fields.username.as_str(), from_fields.password.as_str()),
            ("admin", "secret")
        );
        assert!(credentials(&CameraConfig::new("Cam", "rtsp://10.0.0.2/live")).is_none());
    }

    #[test]
    fn storing_the_endpoint_does_not_restart_the_subscriber() {
        let mut cam = CameraConfig::new("Cam", "rtsp://10.0.0.2/live");
        cam.motion.source = nw_core::camera::MotionSource::Onvif;
        let before = fingerprint(&cam);
        cam.onvif = Some(OnvifEndpoint {
            device_url: "http://10.0.0.2/onvif/device_service".into(),
            events_url: None,
            ptz_url: None,
        });
        assert_eq!(fingerprint(&cam), before);
        cam.motion.source = nw_core::camera::MotionSource::Off;
        assert_ne!(fingerprint(&cam), before);
    }
}
