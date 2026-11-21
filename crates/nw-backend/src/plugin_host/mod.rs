//! Analytics plugins: daemons this node connects to over
//! `nw-analytics-protocol` (design: `docs/plans/2026-10-27-analytics-plugins.md`).
//!
//! The node does no analysis for a plugin: each binding (plugin × camera)
//! subscribes to the camera's shared connection and forwards its compressed
//! video unchanged; the plugin decodes if it needs to and sends events back,
//! which go into the [`EventHub`] like any other. Bindings come from config
//! `plugins`, reconciled every second as motion and ONVIF events are. One
//! session (thread) per plugin; a slow or failing plugin loses its own units
//! and never holds up a camera connection, recording or the IPC.
mod config;
pub(crate) mod credentials;
mod decoded;
mod diagnostics;
pub(crate) mod discovery;
pub mod history;
pub mod lag;
pub(crate) mod legacy_motion;
pub mod overlays;
mod pump;
mod session;
#[cfg(all(test, unix))]
mod tests;
pub(crate) mod web;

pub use config::{Managed, PluginBinding, PluginServer, PluginsConfig};
pub use diagnostics::{PluginPictures, PluginStreamConsumer};
pub use overlays::OverlayStore;

use crate::app_state::AppState;
use crate::events::EventHub;
use nw_analytics_protocol::{address::Address, messages};
use nw_core::camera::CameraConfig;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};

/// What the node shows about one plugin.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServerStatus {
    pub id: String,
    pub address: String,
    /// `connecting`, `connected`, `error`, `disabled`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<messages::Manifest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<messages::Refusal>,
    pub bindings: BTreeMap<String, BindingView>,
    /// Unreachable without a break since then (retries included); cleared
    /// once the plugin answers. See [`PluginHost::offline`].
    #[serde(skip)]
    pub unreachable_since: Option<Instant>,
}

/// A plugin unreachable this long is offline: the tray, the status bar and
/// the `plugin_offline` event say so. Shorter, it may still be starting.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(10);

impl ServerStatus {
    /// Keep [`Self::unreachable_since`] in step with `state`.
    fn track_reachability(&mut self) {
        match self.state {
            "error" => {
                self.unreachable_since.get_or_insert_with(Instant::now);
            }
            "authorizing" | "pending_approval" | "connected" | "disabled" => {
                self.unreachable_since = None;
            }
            _ => {} // connecting: a retry keeps the time it became unreachable
        }
    }
}

/// An enabled plugin that stopped answering.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OfflinePlugin {
    pub id: String,
    pub name: String,
    #[serde(skip)]
    pub detail: Option<String>,
}

/// One binding, as the node sees it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BindingView {
    pub binding: u32,
    /// `attaching`, `active`, `refused`, `stream_unavailable`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the plugin last reported for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<messages::BindingStatus>,
    /// Units sent, and units lost because the plugin fell behind.
    pub sent: u64,
    pub dropped: u64,
    /// The camera stream it gets, and that stream's format (`H.264 640×360`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<messages::StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The video wall's decoded pictures are shared with the plugin now.
    pub shared_pictures: bool,
    /// How many of them a second (measured over the last two seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_fps: Option<f32>,
}

/// A plugin analysing a camera, for whoever shows the camera
/// (`plugins.cameras`: the tile's marker and its drawing layers).
#[derive(Debug, Clone, Serialize)]
pub struct CameraAnalytics {
    pub plugin: String,
    pub name: String,
    /// The plugin's manifest id, the topic of its events (unknown until it
    /// first connected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The plugin's own state for this camera (`running`, `waiting`, …), or
    /// `offline` while the plugin is not connected.
    pub state: String,
    pub layers: Vec<messages::OverlayLayer>,
    /// The layers drawn on the live wall.
    pub wall_layers: Vec<String>,
    /// The plugin's figures for this camera, shown beside its picture
    /// (`messages::BindingStatus::figures`); none while it is offline.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub figures: Vec<messages::Figure>,
}

/// What a session should be doing: set by [`PluginHost::run`], read by the
/// session's thread.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Desired {
    pub bindings: Vec<DesiredBinding>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DesiredBinding {
    pub camera: CameraConfig,
    pub config: Value,
    /// The camera stream chosen for the plugin (`None`: the plugin's choice).
    pub stream: Option<messages::StreamChoice>,
    /// The camera is being moved (PTZ), so activity is paused.
    pub moving: bool,
    /// Never offer the plugin the recorder's decoded pictures.
    pub own_decoder: bool,
}

struct Running {
    server: PluginServer,
    desired: Arc<Mutex<Desired>>,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub struct PluginHost {
    status: Arc<Mutex<HashMap<String, ServerStatus>>>,
    /// The plugins' latest drawings (`plugins.overlays`).
    pub overlays: Arc<OverlayStore>,
    queries: Arc<Mutex<HashMap<String, std::sync::mpsc::SyncSender<Query>>>>,
    /// The portal's latest ticket for each hosted plugin server.
    tickets: Arc<Mutex<HashMap<String, String>>>,
    /// Bindings to plugins over the network losing video (`lagging`).
    lag: Arc<Mutex<lag::LagWatch>>,
}

pub(super) struct Query {
    method: String,
    params: Value,
    reply: tokio::sync::oneshot::Sender<Result<Value, String>>,
}

impl PluginHost {
    /// Hosted services' tickets, from the portal's latest answer.
    pub fn set_tickets(&self, tickets: HashMap<String, String>) {
        if let Ok(mut held) = self.tickets.lock() {
            *held = tickets;
        }
    }

    /// Only authenticated, approved sessions receive page requests.
    pub async fn query(&self, server: &str, method: &str, params: Value) -> Result<Value, String> {
        if !["web.request", "web.read"].contains(&method) {
            return Err("Unsupported plugin request".into());
        }
        let available = self.status.lock().is_ok_and(|status| {
            status.get(server).is_some_and(|status| {
                status.state == "connected"
                    && status.manifest.as_ref().is_some_and(|manifest| {
                        manifest.web_ui.is_some()
                            && manifest
                                .permissions
                                .iter()
                                .any(|permission| permission == "web_ui")
                    })
            })
        });
        if !available {
            return Err("The plugin's web interface is unavailable".into());
        }
        let sender = self
            .queries
            .lock()
            .ok()
            .and_then(|queries| queries.get(server).cloned())
            .ok_or("Plugin disconnected")?;
        let (reply, result) = tokio::sync::oneshot::channel();
        sender
            .try_send(Query {
                method: method.into(),
                params,
                reply,
            })
            .map_err(|_| "The plugin's page request queue is full")?;
        tokio::time::timeout(Duration::from_secs(10), result)
            .await
            .map_err(|_| "The plugin's page request timed out")?
            .map_err(|_| "The plugin disconnected")?
    }
    pub fn with_database(db: Arc<Mutex<rusqlite::Connection>>) -> Self {
        match history::History::new(db) {
            Ok(history) => Self {
                overlays: Arc::new(OverlayStore::with_history(Arc::new(history))),
                ..Self::default()
            },
            Err(error) => {
                tracing::error!(%error, "Analytics drawing history writer could not start");
                Self::default()
            }
        }
    }

    /// Enabled plugins unreachable for at least `after` ([`OFFLINE_AFTER`]),
    /// in config order, named as they introduce themselves.
    pub fn offline(&self, order: &[PluginServer], after: Duration) -> Vec<OfflinePlugin> {
        let Ok(status) = self.status.lock() else {
            return vec![];
        };
        order
            .iter()
            .filter(|server| server.enabled)
            .filter_map(|server| {
                let status = status.get(&server.id)?;
                let since = status.unreachable_since?;
                (since.elapsed() >= after).then(|| OfflinePlugin {
                    id: server.id.clone(),
                    name: server.shown_name(
                        status
                            .manifest
                            .as_ref()
                            .map(|manifest| manifest.name.as_str()),
                    ),
                    detail: status.detail.clone(),
                })
            })
            .collect()
    }

    /// Bindings to plugins over the network that cannot keep up
    /// ([`lag::LagWatch`]); call at a steady pace (the health loop).
    pub fn lagging(&self, order: &[PluginServer]) -> Vec<lag::LaggingBinding> {
        let (Ok(status), Ok(mut lag)) = (self.status.lock(), self.lag.lock()) else {
            return vec![];
        };
        lag.lagging(order, &status, Instant::now())
    }

    /// Every configured plugin's status, in config order.
    pub fn status(&self, order: &[PluginServer]) -> Vec<ServerStatus> {
        let Ok(status) = self.status.lock() else {
            return vec![];
        };
        order
            .iter()
            .map(|server| {
                status.get(&server.id).cloned().unwrap_or(ServerStatus {
                    id: server.id.clone(),
                    address: server.address.clone(),
                    state: if server.enabled {
                        "connecting"
                    } else {
                        "disabled"
                    },
                    ..Default::default()
                })
            })
            .collect()
    }

    /// A connected plugin is analysing `camera_id` (it reported `running`):
    /// event-based recording may count on its events.
    pub fn detecting(&self, camera_id: &str) -> bool {
        self.status.lock().is_ok_and(|status| {
            status.values().any(|server| {
                server.state == "connected"
                    && server.bindings.get(camera_id).is_some_and(|binding| {
                        binding
                            .plugin
                            .as_ref()
                            .is_some_and(|plugin| plugin.state == messages::BindingState::Running)
                    })
            })
        })
    }

    /// Per camera, the enabled plugins bound to it (in config order).
    pub fn cameras(&self, config: &PluginsConfig) -> BTreeMap<String, Vec<CameraAnalytics>> {
        let status = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        let mut cameras: BTreeMap<String, Vec<CameraAnalytics>> = BTreeMap::new();
        for binding in config.bindings.iter().filter(|b| b.enabled) {
            let Some(server) = config
                .servers
                .iter()
                .find(|s| s.id == binding.server && s.enabled)
            else {
                continue;
            };
            let server_status = status.get(&server.id);
            let manifest = server_status.and_then(|s| s.manifest.as_ref());
            let connected = server_status.is_some_and(|s| s.state == "connected");
            let plugin_view = server_status
                .and_then(|s| s.bindings.get(&binding.camera_id))
                .and_then(|view| view.plugin.as_ref());
            let reported = plugin_view.map(|plugin| plugin.state);
            let figures = match (connected, plugin_view) {
                (true, Some(plugin)) => plugin.figures.clone(),
                _ => vec![],
            };
            let state = match (connected, reported) {
                (false, _) => "offline".to_owned(),
                (true, Some(state)) => serde_json::to_value(state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                (true, None) => "starting".to_owned(),
            };
            let layers = manifest
                .map(|m| m.overlay_layers.clone())
                .unwrap_or_default();
            let wall_layers = binding.wall_layers.clone().unwrap_or_else(|| {
                layers
                    .iter()
                    .filter(|l| l.default_on)
                    .map(|l| l.id.clone())
                    .collect()
            });
            cameras
                .entry(binding.camera_id.clone())
                .or_default()
                .push(CameraAnalytics {
                    plugin: server.id.clone(),
                    topic: manifest.map(|m| m.id.clone()),
                    name: server.shown_name(manifest.map(|m| m.name.as_str())),
                    state,
                    layers,
                    wall_layers,
                    figures,
                });
        }
        cameras
    }

    pub async fn run(
        self: Arc<Self>,
        state: Arc<RwLock<AppState>>,
        events: Arc<EventHub>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let mut running: HashMap<String, Running> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! { _ = shutdown.recv() => break, _ = tick.tick() => {} }
            let (plugins, cameras, streams, node, config_path) = {
                let st = state.read().await;
                (
                    st.config.plugins.clone(),
                    st.config.cameras.clone(),
                    Arc::clone(&st.streams),
                    messages::NodeInfo {
                        name: st.config.site_name.clone(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    },
                    st.config_path.clone(),
                )
            };
            let wanted: HashMap<&str, &PluginServer> = plugins
                .servers
                .iter()
                .filter(|server| server.enabled)
                .map(|server| (server.id.as_str(), server))
                .collect();
            self.overlays.set_wall_layers(
                self.cameras(&plugins)
                    .into_iter()
                    .flat_map(|(camera, analytics)| {
                        analytics.into_iter().flat_map(move |plugin| {
                            let camera = camera.clone();
                            plugin
                                .wall_layers
                                .into_iter()
                                .map(move |layer| (camera.clone(), plugin.plugin.clone(), layer))
                        })
                    })
                    .collect(),
            );
            let retired: Vec<String> = running
                .iter()
                .filter(|(id, run)| {
                    wanted
                        .get(id.as_str())
                        .is_none_or(|server| *server != &run.server)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in retired {
                if let Some(run) = running.remove(&id) {
                    stop(run).await;
                }
                if let Ok(mut status) = self.status.lock() {
                    status.remove(&id);
                }
                self.overlays.clear(&id, None);
            }
            if let Ok(mut status) = self.status.lock() {
                status.retain(|id, _| wanted.contains_key(id.as_str()));
            }
            for (id, server) in wanted {
                let desired = Desired {
                    bindings: plugins
                        .bindings
                        .iter()
                        .filter(|binding| binding.enabled && binding.server == id)
                        .filter_map(|binding| {
                            let camera = cameras.iter().find(|c| c.id == binding.camera_id)?;
                            Some(DesiredBinding {
                                camera: camera.clone(),
                                config: binding.config.clone(),
                                stream: binding.stream,
                                moving: events.activity_paused(&camera.id),
                                own_decoder: binding.own_decoder,
                            })
                        })
                        .collect(),
                };
                if let Some(run) = running.get(id) {
                    if let Ok(mut current) = run.desired.lock() {
                        if *current != desired {
                            *current = desired;
                        }
                    }
                    continue;
                }
                let address = match Address::parse(&server.address) {
                    Ok(address) => address,
                    Err(error) => {
                        // `plugins.set` validates; a hand-edited file may not.
                        self.set_server(id, &server.address, "error", Some(error.to_string()));
                        continue;
                    }
                };
                // A hosted plugin's TLS address, for when QUIC can't get through.
                let fallback = server
                    .managed
                    .as_ref()
                    .and_then(|managed| managed.fallback.as_deref())
                    .and_then(|fallback| Address::parse(fallback).ok());
                let context = session::Context {
                    server_id: id.to_owned(),
                    address,
                    fallback,
                    node: node.clone(),
                    streams: Arc::clone(&streams),
                    events: Arc::clone(&events),
                    status: Arc::clone(&self.status),
                    overlays: Arc::clone(&self.overlays),
                    approval: server.approval.clone(),
                    managed: server.managed.is_some(),
                    tickets: Arc::clone(&self.tickets),
                    config_path: config_path.clone(),
                    queries: Arc::clone(&self.queries),
                };
                let desired = Arc::new(Mutex::new(desired));
                let stop = Arc::new(AtomicBool::new(false));
                let thread = {
                    let desired = Arc::clone(&desired);
                    let stop = Arc::clone(&stop);
                    std::thread::Builder::new()
                        .name(format!("plugin-{id}"))
                        .spawn(move || session::run(context, desired, stop))
                };
                match thread {
                    Ok(thread) => {
                        running.insert(
                            id.to_owned(),
                            Running {
                                server: server.clone(),
                                desired,
                                stop,
                                thread,
                            },
                        );
                    }
                    Err(error) => {
                        tracing::warn!(plugin = %id, %error, "Could not start a plugin session");
                    }
                }
            }
        }
        for (_, run) in running {
            stop(run).await;
        }
        if let Ok(mut status) = self.status.lock() {
            status.clear();
        }
    }

    fn set_server(&self, id: &str, address: &str, state: &'static str, detail: Option<String>) {
        if let Ok(mut status) = self.status.lock() {
            let entry = status.entry(id.to_owned()).or_default();
            entry.id = id.to_owned();
            entry.address = address.to_owned();
            entry.state = state;
            entry.detail = detail;
            entry.track_reachability();
        }
    }
}

async fn stop(run: Running) {
    run.stop.store(true, Ordering::Release);
    let _ = tokio::task::spawn_blocking(move || run.thread.join()).await;
}
