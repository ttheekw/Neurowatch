//! Shared application state — cameras, supervisor lifecycle, config persistence.
//!
//! Each configured camera has an encoded **ingest** ([`IngestSupervisor`]):
//! its status and its recording, never decoding. A **desktop view**
//! ([`PipelineSupervisor`]) decodes a camera only while a desktop UI's
//! renewable lease ([`AppState::set_view_demand`]) shows it; a UI that
//! closes, hides the wall or stops renewing (crashes) releases it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nw_core::camera::{CameraConfig, CameraId};
use nw_core::control_plane::{CameraAssignment, Lease, CONTROL_PROTOCOL_VERSION};
use nw_core::traits::{EventBus, LogEventBus};
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::assignment::{AssignmentError, AssignmentRegistry, LocalAssignmentRegistry};
use crate::config::AppConfig;
use crate::pipeline::reachability::Reachability;
use crate::pipeline::{
    hwaccel::HwAccelKind,
    ingest::IngestSupervisor,
    supervisor::{PipelineSupervisor, StreamInfo},
};
use crate::playback::session::PlaybackSession;
use crate::recording::supervisor::RecordingSupervisor;
use crate::stats::StreamHealthRegistry;
use crate::taps::{FrameTap, KeyframeSink};

pub struct AppState {
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub hwaccel: HwAccelKind,
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// Recording state machine — tracks which cameras are actively recording.
    pub recording: RecordingSupervisor,
    recording_intervals: crate::recording::event_retention::EligibilityClock,
    /// Active playback sessions keyed by session_id.
    pub playback_sessions: HashMap<String, PlaybackSession>,
    event_bus: Arc<dyn EventBus>,
    assignment_registry: Arc<dyn AssignmentRegistry>,
    assignment_generations: HashMap<CameraId, u64>,
    stop_txs: HashMap<CameraId, watch::Sender<bool>>,
    /// Supervisor threads, so shutdown can wait (bounded) for recordings to finalize.
    supervisor_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Receiving end of each supervisor's state watch channel.
    /// `watch::Receiver::borrow()` is non-blocking — safe to call while holding the RwLock.
    supervisor_state_rx: HashMap<CameraId, watch::Receiver<String>>,
    /// Receiving end of each supervisor stream-info (codec, resolution) channel.
    supervisor_stream_info_rx: HashMap<CameraId, watch::Receiver<StreamInfo>>,
    /// Live-view stream per camera: a selector for `CameraConfig::stream_url`.
    /// Absent means the sub stream, which keeps multi-camera walls light.
    live_stream: HashMap<CameraId, String>,
    /// Encoded ingest per camera: status and recording.
    ingest: HashMap<CameraId, IngestHandle>,
    /// Cameras whose desktop view (decoder) is running.
    views: HashSet<CameraId>,
    /// Desktop UIs' view leases: which cameras each shows, until when.
    view_demand: HashMap<String, (HashSet<CameraId>, Instant)>,
    /// Which camera each desktop UI listens to live, until when (its lease).
    audio_demand: HashMap<String, (CameraId, Instant)>,
    /// The camera played live on this computer's audio output, if any.
    live_audio: Option<(CameraId, watch::Sender<bool>)>,
    /// Cameras asked to record whose source has no compressed video to keep.
    recording_unsupported: HashSet<CameraId>,
    /// Recording locations' readiness (`crate::storage_guard`).
    pub storage: Arc<crate::storage_guard::StorageGuard>,
    /// Cameras asked to record whose location is missing, with why.
    storage_blocked: HashMap<CameraId, String>,
    /// The archive tier's mover (`recording::archive`).
    pub archive: Arc<crate::recording::archive::Archive>,
    /// Registry of per-camera renderer/decoder health counters, shared with
    /// active supervisors and read by the stats aggregator.
    pub stream_health: Arc<StreamHealthRegistry>,
    /// Shared camera connections, one per stream (`crate::streams`).
    pub streams: Arc<crate::streams::StreamHubs>,
    /// No desktop UI: camera pipelines skip the SHM ring and convert frames
    /// only when a [`FrameTap`] wants them.
    headless: bool,
    /// Optional consumer of decoded frames (see [`crate::taps`]).
    frame_tap: Option<Arc<dyn FrameTap>>,
    /// Optional consumer of each recording ingest's keyframes.
    keyframe_sink: Option<Arc<dyn KeyframeSink>>,
    /// Per camera, shared by its ingest and desktop view (and kept across
    /// their restarts): whoever gets data first ends the other's backoff.
    reachability: HashMap<CameraId, Arc<Reachability>>,
    /// Clip export jobs, shared by every client (desktop IPC, web portal).
    pub exports: Arc<crate::export::ExportService>,
    /// Motion, camera analytics, health and bookmarks ([`crate::events`]).
    pub events: Arc<crate::events::EventHub>,
    /// The cameras' own events over ONVIF ([`crate::onvif_events`]).
    pub onvif_events: Arc<crate::onvif_events::OnvifEvents>,
    /// Opt-in analysis decoders, independent of desktop views and recording.
    pub ptz: Arc<crate::ptz::PtzService>,
    /// Alert notifications (`crate::alerts`).
    pub alerts: Arc<crate::alerts::AlertService>,
    /// Signed-in local accounts (`crate::accounts`).
    pub sessions: Arc<crate::accounts::Sessions>,
    /// Two-way audio: who talks through which camera (`crate::talk`).
    pub talk: Arc<crate::talk::TalkService>,
    pub builtin_motion: Arc<crate::motion::runtime::BuiltinMotion>,
    pub snapshots: Arc<crate::snapshot::SnapshotService>,
}

impl AppState {
    pub fn new(config: AppConfig, config_path: PathBuf, hwaccel: HwAccelKind) -> Self {
        // This crate's tests never touch the user's real recording index.
        #[cfg(test)]
        let db = crate::db::connection::open_in_memory();
        #[cfg(not(test))]
        let db = crate::db::connection::open_db();
        Self::with_database(
            config,
            config_path,
            hwaccel,
            db.expect("Failed to open SQLite database"),
        )
    }

    /// Like [`Self::new`], with the given recording index (tests of other
    /// crates pass [`crate::db::connection::open_in_memory`]).
    pub fn with_database(
        config: AppConfig,
        config_path: PathBuf,
        hwaccel: HwAccelKind,
        db: rusqlite::Connection,
    ) -> Self {
        let db = Arc::new(Mutex::new(db));
        let events = Arc::new(crate::events::EventHub::new(Arc::clone(&db)));
        let streams = Arc::new(crate::streams::StreamHubs::default());
        let snapshots = Arc::new(crate::snapshot::SnapshotService::new(Arc::clone(&streams)));
        Self {
            config,
            config_path,
            hwaccel,
            db,
            recording: RecordingSupervisor::new(),
            recording_intervals: crate::recording::event_retention::EligibilityClock::default(),
            playback_sessions: HashMap::new(),
            event_bus: Arc::new(LogEventBus),
            assignment_registry: Arc::new(LocalAssignmentRegistry::new()),
            assignment_generations: HashMap::new(),
            stop_txs: HashMap::new(),
            supervisor_tasks: Vec::new(),
            supervisor_state_rx: HashMap::new(),
            supervisor_stream_info_rx: HashMap::new(),
            live_stream: HashMap::new(),
            ingest: HashMap::new(),
            views: HashSet::new(),
            view_demand: HashMap::new(),
            audio_demand: HashMap::new(),
            live_audio: None,
            recording_unsupported: HashSet::new(),
            storage: Arc::default(),
            storage_blocked: HashMap::new(),
            archive: Arc::default(),
            stream_health: Arc::new(StreamHealthRegistry::new()),
            streams,
            headless: false,
            frame_tap: None,
            keyframe_sink: None,
            reachability: HashMap::new(),
            exports: Arc::new(crate::export::ExportService::new()),
            events,
            onvif_events: Arc::new(crate::onvif_events::OnvifEvents::new()),
            ptz: Arc::new(crate::ptz::PtzService::default()),
            alerts: Arc::new(crate::alerts::AlertService::default()),
            sessions: Arc::new(crate::accounts::Sessions::default()),
            talk: Arc::new(crate::talk::TalkService::default()),
            builtin_motion: Arc::new(crate::motion::runtime::BuiltinMotion::default()),
            snapshots,
        }
    }

    /// Install a consumer of decoded frames, before the cameras start.
    pub fn set_frame_tap(&mut self, tap: Arc<dyn FrameTap>) {
        self.frame_tap = Some(tap);
    }

    /// Install a consumer of recorded keyframes, before the cameras start.
    pub fn set_keyframe_sink(&mut self, sink: Arc<dyn KeyframeSink>) {
        self.keyframe_sink = Some(sink);
    }

    pub fn set_headless(&mut self, headless: bool) {
        self.headless = headless;
    }

    /// Started without a desktop UI (`NW_HEADLESS=1`).
    pub fn headless(&self) -> bool {
        self.headless
    }

    /// Start (or restart) a camera: its ingest, and its desktop view if a
    /// desktop UI shows it. Used when a camera is added or its config changed.
    pub fn spawn_supervisor(&mut self, cam: &CameraConfig) {
        self.restart_ingest(cam);
        if self.desired_views().contains(&cam.id) {
            self.spawn_view(cam);
        }
    }

    /// (Re)start the camera's encoded ingest, picking up recording changes.
    /// Desktop views and browser live view are not touched.
    pub fn restart_ingest(&mut self, cam: &CameraConfig) {
        self.recording_intervals.observe(&cam.id, "", false, 0);
        if let Some(old) = self.ingest.remove(&cam.id) {
            let _ = old.stop_tx.send(true);
        }
        if let Some(sink) = &self.keyframe_sink {
            sink.forget(&cam.id);
        }
        self.snapshots.forget(&cam.id);
        self.storage_blocked.remove(&cam.id);
        // Global setting: automatically enable local recording for every camera.
        // Manual recording sessions take precedence and are preserved.
        if self.config.recording.local_all_cameras && !self.recording.wants_recording(&cam.id) {
            let storage_dir = self.global_recording_output_dir(cam);
            let root = self.global_storage_root();
            match self.prepare_recording_dir(&root, &storage_dir) {
                Ok(()) => self
                    .recording
                    .start_global_all_local(cam.id.clone(), storage_dir),
                Err(reason) => {
                    tracing::warn!(cam_id = %cam.id, reason = %reason, "Camera streams without recording: its recording location is unavailable");
                    self.storage_blocked.insert(cam.id.clone(), reason);
                }
            }
        }
        self.reconcile_configured_local_recording(cam);
        if crate::recording::policy::current_mode(cam) == nw_core::camera::RecordingMode::Off {
            self.recording.pause_automatic(&cam.id);
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let (state_tx, state_rx) = watch::channel("connecting".to_owned());
        let (stream_info_tx, info_rx) = watch::channel(StreamInfo::default());
        let mut ingest = IngestSupervisor::new(
            cam.clone(),
            Arc::clone(&self.event_bus),
            stop_rx,
            state_tx,
            stream_info_tx,
        )
        .with_health_counters(self.stream_health.counters_for(&cam.id))
        .with_stream_hubs(Arc::clone(&self.streams));
        ingest = ingest.with_reachability(self.reachability_of(&cam.id));
        ingest = ingest.with_keyframes(Arc::new(crate::snapshot::KeyframeFanout {
            local: Arc::clone(&self.snapshots),
            cloud: self.keyframe_sink.clone(),
        }));
        self.recording_unsupported.remove(&cam.id);
        // Last check before the ingest is given a folder (manual recordings
        // included): the location's cached readiness, kept current by
        // `run_storage_checks`.
        let recording_path = self
            .recording
            .recording_storage_path(&cam.id)
            .cloned()
            .filter(|path| {
                let root = self.storage_root_of(cam, path);
                match self
                    .storage
                    .status(&root)
                    .and_then(|status| status.reason().map(str::to_owned))
                {
                    Some(reason) => {
                        self.storage_blocked.insert(cam.id.clone(), reason);
                        false
                    }
                    None => true,
                }
            });
        if let Some(storage_path) = recording_path {
            if crate::pipeline::builder::recording_supported(&cam.url) {
                ingest = ingest.with_recording(
                    storage_path.to_string_lossy().to_string(),
                    self.config.recording.segment_minutes,
                );
            } else {
                tracing::warn!(cam_id = %cam.id, "Recording unavailable: this source has no compressed video to keep");
                self.recording_unsupported.insert(cam.id.clone());
            }
        }
        self.supervisor_tasks.retain(|task| !task.is_finished());
        self.supervisor_tasks
            .push(tokio::task::spawn_blocking(move || ingest.run()));
        self.ingest.insert(
            cam.id.clone(),
            IngestHandle {
                stop_tx,
                state_rx,
                info_rx,
            },
        );
    }

    /// Start the camera's desktop view: decode it for the desktop wall.
    fn spawn_view(&mut self, cam: &CameraConfig) {
        if crate::pipeline::hwaccel::live_view_block_reason().is_some() {
            return;
        }
        if self.stop_txs.contains_key(&cam.id) {
            self.stop_view(&cam.id);
        }

        let assignment = self.local_assignment(cam);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (state_tx, state_rx) = watch::channel("connecting".to_owned());
        let (stream_info_tx, stream_info_rx) = watch::channel(StreamInfo::default());
        let startup_stop_rx = stop_rx.clone();
        let startup_state_tx = state_tx.clone();
        let effective_cam = self.effective_camera_config(cam);
        let health_counters = self.stream_health.counters_for(&cam.id);
        let mut supervisor = PipelineSupervisor::new(
            effective_cam,
            self.hwaccel,
            Arc::clone(&self.event_bus),
            stop_rx,
            state_tx,
            stream_info_tx,
        )
        .with_assignment(assignment.clone(), Arc::clone(&self.assignment_registry))
        .with_health_counters(Arc::clone(&health_counters))
        .with_headless(self.headless)
        .with_stream_hubs(Arc::clone(&self.streams));
        if let Some(tap) = self.frame_tap.clone() {
            supervisor = supervisor.with_frame_tap(tap);
        }
        supervisor = supervisor.with_reachability(self.reachability_of(&cam.id));

        // Installing a replacement can wait for the old pipeline's verified
        // teardown. Keep that wait off the Tokio worker and outside AppState's
        // write lock so status/diagnostics and other cameras remain responsive.
        let registry = Arc::clone(&self.assignment_registry);
        self.supervisor_tasks.retain(|task| !task.is_finished());
        let task = tokio::task::spawn_blocking(move || {
            match install_local_assignment(&*registry, &assignment, &startup_stop_rx) {
                Ok(true) => supervisor.run(),
                Ok(false) => {
                    let _ = startup_state_tx.send("offline".to_owned());
                }
                Err(error) => {
                    health_counters.set_state("teardown_failed");
                    let _ = startup_state_tx.send("teardown_failed".to_owned());
                    tracing::error!(cam_id = %assignment.camera_id, %error, "Supervisor spawn skipped; assignment fence was not available");
                }
            }
        });
        self.supervisor_tasks.push(task);
        self.stop_txs.insert(cam.id.clone(), stop_tx);
        self.supervisor_state_rx.insert(cam.id.clone(), state_rx);
        self.supervisor_stream_info_rx
            .insert(cam.id.clone(), stream_info_rx);
        self.views.insert(cam.id.clone());
        tracing::info!(cam_id = %cam.id, name = %cam.name, "Desktop view started");
    }

    /// Send stop signal to a camera's desktop view (non-blocking).
    ///
    /// Channel handles are intentionally retained after the stop signal is sent.
    /// They are replaced when a new view is spawned, or cleared on full
    /// shutdown via `stop_all`. Retaining them lets the assignment registry fence
    /// remain observable and prevents a replacement from racing with an
    /// unquiesced teardown.
    fn stop_view(&mut self, cam_id: &CameraId) {
        self.views.remove(cam_id);
        if let Some(tx) = self.stop_txs.get(cam_id) {
            let _ = tx.send(true);
            tracing::info!(cam_id = %cam_id, "Desktop view stop requested");
        }
    }

    /// Stop everything for a camera: its desktop view and its ingest.
    /// The camera's shared "it answered" signal (see [`Reachability`]).
    /// Subscribe to a camera's stream through its shared connection (opened
    /// if none is open; see [`crate::streams`]). `stream` is `main`, `sub`
    /// (the main stream when no sub stream is configured, sharing its
    /// connection) or a listed stream id. Dropping the subscription
    /// unsubscribes.
    pub fn subscribe_stream(
        &self,
        camera_id: &str,
        stream: &str,
        policy: crate::streams::SubscriberPolicy,
    ) -> Result<crate::streams::Subscription, String> {
        let camera = self
            .config
            .cameras
            .iter()
            .find(|camera| camera.id == camera_id)
            .ok_or_else(|| "Camera not found".to_owned())?;
        let url = camera
            .stream_url(stream)
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| format!("camera has no stream '{stream}'"))?;
        let mut source = camera.clone();
        source.url = url;
        self.streams
            .subscribe(&source, policy, self.reachability.get(camera_id).cloned())
    }

    fn reachability_of(&mut self, cam_id: &CameraId) -> Arc<Reachability> {
        Arc::clone(self.reachability.entry(cam_id.clone()).or_default())
    }

    pub fn stop_supervisor(&mut self, cam_id: &CameraId) {
        self.ptz.forget(cam_id, Arc::clone(&self.events));
        self.stop_view(cam_id);
        if let Some(ingest) = self.ingest.remove(cam_id) {
            let _ = ingest.stop_tx.send(true);
        }
        if let Some(sink) = &self.keyframe_sink {
            sink.forget(cam_id);
        }
        self.snapshots.forget(cam_id);
        self.recording_unsupported.remove(cam_id);
        self.reachability.remove(cam_id);
    }

    /// Stop all running supervisors and playback sessions.
    pub fn stop_all(&mut self) {
        self.ptz.shutdown(Arc::clone(&self.events));
        for (cam_id, tx) in self.stop_txs.drain() {
            let _ = tx.send(true);
            tracing::info!(cam_id = %cam_id, "Desktop view stop requested (shutdown)");
        }
        for (cam_id, ingest) in self.ingest.drain() {
            let _ = ingest.stop_tx.send(true);
            tracing::info!(cam_id = %cam_id, "Ingest stop requested (shutdown)");
        }
        self.views.clear();
        self.view_demand.clear();
        self.audio_demand.clear();
        if let Some((cam_id, stop)) = self.live_audio.take() {
            let _ = stop.send(true);
            tracing::info!(cam_id = %cam_id, "Live audio stop requested (shutdown)");
        }
        // A clip cut short by shutdown would be broken: stop and remove it.
        self.exports.cancel_all();
        self.supervisor_state_rx.clear();
        self.supervisor_stream_info_rx.clear();
        self.live_stream.clear();
        // Stop any open playback sessions.
        for (session_id, session) in self.playback_sessions.drain() {
            session.stop();
            tracing::info!(session_id = %session_id, "Playback session stopped (shutdown)");
        }
    }

    /// Supervisor threads that may still be running, for shutdown to wait on.
    pub fn take_supervisor_tasks(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(&mut self.supervisor_tasks)
    }

    // -- desktop view leases ---------------------------------------------------------

    /// A desktop UI shows exactly `cameras` for `ttl` from now. It renews
    /// this while it shows them; letting it lapse releases them.
    pub fn set_view_demand(&mut self, client_id: &str, cameras: HashSet<CameraId>, ttl: Duration) {
        self.view_demand
            .insert(client_id.to_owned(), (cameras, Instant::now() + ttl));
        self.reconcile_views();
    }

    /// A desktop UI listens to `camera` live (or to nothing), for the same
    /// `ttl` as its view lease: a UI that closes or crashes goes silent.
    pub fn set_listen(&mut self, client_id: &str, camera: Option<CameraId>, ttl: Duration) {
        match camera {
            Some(camera) => {
                self.audio_demand
                    .insert(client_id.to_owned(), (camera, Instant::now() + ttl));
            }
            None => {
                self.audio_demand.remove(client_id);
            }
        }
        self.reconcile_audio();
    }

    /// The camera played live right now, if any.
    pub fn listening(&self) -> Option<CameraId> {
        self.live_audio.as_ref().map(|(cam_id, _)| cam_id.clone())
    }

    /// A desktop UI shows no cameras any more.
    pub fn release_view_demand(&mut self, client_id: &str) {
        let listened = self.audio_demand.remove(client_id).is_some();
        if self.view_demand.remove(client_id).is_some() {
            self.reconcile_views();
        } else if listened {
            self.reconcile_audio();
        }
    }

    /// Drop leases not renewed by `now`; returns the clients that lapsed.
    pub fn expire_view_demand(&mut self, now: Instant) -> Vec<String> {
        let lapsed: Vec<String> = self
            .view_demand
            .iter()
            .filter(|(_, (_, until))| *until <= now)
            .map(|(client, _)| client.clone())
            .collect();
        let silent: Vec<String> = self
            .audio_demand
            .iter()
            .filter(|(_, (_, until))| *until <= now)
            .map(|(client, _)| client.clone())
            .collect();
        for client in &silent {
            self.audio_demand.remove(client);
        }
        if !lapsed.is_empty() {
            for client in &lapsed {
                self.view_demand.remove(client);
                self.audio_demand.remove(client);
            }
            tracing::info!(clients = lapsed.len(), "Desktop view leases lapsed");
            self.reconcile_views();
        } else if !silent.is_empty() {
            self.reconcile_audio();
        }
        lapsed
    }

    fn desired_views(&self) -> HashSet<CameraId> {
        let now = Instant::now();
        self.view_demand
            .values()
            .filter(|(_, until)| *until > now)
            .flat_map(|(cameras, _)| cameras.iter().cloned())
            .collect()
    }

    /// Run a desktop view for exactly the cameras some UI shows.
    pub fn reconcile_views(&mut self) {
        let desired = self.desired_views();
        let cameras = self.config.cameras.clone();
        for cam in &cameras {
            let running = self.views.contains(&cam.id);
            if desired.contains(&cam.id) && !running {
                self.spawn_view(cam);
            } else if !desired.contains(&cam.id) && running {
                self.stop_view(&cam.id);
            }
        }
        self.reconcile_audio();
    }

    /// The camera to play live: the most recently renewed listener's, if it
    /// is still configured. One computer has one audio output.
    fn desired_audio(&self) -> Option<CameraConfig> {
        if self.headless {
            return None;
        }
        let now = Instant::now();
        let (camera, _) = self
            .audio_demand
            .values()
            .filter(|(_, until)| *until > now)
            .max_by_key(|(_, until)| *until)?;
        self.config
            .cameras
            .iter()
            .find(|c| &c.id == camera)
            .cloned()
    }

    /// Play exactly the camera some desktop UI listens to.
    pub fn reconcile_audio(&mut self) {
        let desired = self.desired_audio();
        let running = self.live_audio.as_ref().map(|(cam_id, _)| cam_id.clone());
        if desired.as_ref().map(|c| &c.id) == running.as_ref() {
            return;
        }
        if let Some((cam_id, stop)) = self.live_audio.take() {
            let _ = stop.send(true);
            tracing::info!(cam_id = %cam_id, "Live audio stop requested");
        }
        if let Some(camera) = desired {
            let (stop_tx, stop_rx) = watch::channel(false);
            let cam_id = camera.id.clone();
            let listener = crate::pipeline::audio::LiveAudio::new(camera, stop_rx)
                .with_stream_hubs(Arc::clone(&self.streams));
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::task::spawn_blocking(move || listener.run());
            } else {
                std::thread::spawn(move || listener.run());
            }
            self.live_audio = Some((cam_id, stop_tx));
        }
    }

    /// Cameras being decoded for a desktop view right now.
    pub fn active_views(&self) -> usize {
        self.views.len()
    }

    /// Which cameras a desktop view is decoding right now.
    pub fn viewed_cameras(&self) -> HashSet<CameraId> {
        self.views.clone()
    }

    /// Whether `cam_id` was asked to record but its source cannot be recorded.
    pub fn recording_unsupported(&self, cam_id: &str) -> bool {
        self.recording_unsupported.contains(cam_id)
    }

    /// Each camera's status, from its ingest. `watch::Receiver::borrow()` is
    /// non-blocking — no risk of holding the RwLock across an await point.
    pub fn supervisor_states(&self) -> HashMap<CameraId, String> {
        self.ingest
            .iter()
            .map(|(id, ingest)| (id.clone(), ingest.state_rx.borrow().clone()))
            .collect()
    }

    /// Best-effort detected codec per camera.
    pub fn supervisor_codecs(&self) -> HashMap<CameraId, String> {
        self.supervisor_stream_infos()
            .into_iter()
            .map(|(id, info)| (id, info.codec))
            .collect()
    }

    /// The camera's audio track as its ingest described it, if it sends one.
    pub fn camera_audio(&self, camera_id: &str) -> Option<String> {
        self.ingest.get(camera_id)?.info_rx.borrow().audio.clone()
    }

    /// Codec and size per camera: the desktop view's input while one runs
    /// (it may show another stream), else what the ingest saw. The audio
    /// track is always the ingest's (it describes the camera).
    pub fn supervisor_stream_infos(&self) -> HashMap<CameraId, StreamInfo> {
        self.ingest
            .iter()
            .map(|(id, ingest)| {
                let described = ingest.info_rx.borrow().clone();
                let view = self
                    .views
                    .contains(id)
                    .then(|| self.supervisor_stream_info_rx.get(id))
                    .flatten()
                    .map(|rx| rx.borrow().clone())
                    .filter(|info| info.codec != StreamInfo::default().codec);
                let mut info = view.unwrap_or_else(|| described.clone());
                info.audio = described.audio;
                (id.clone(), info)
            })
            .collect()
    }

    /// Atomically save config to disk.
    pub fn save_config(&self) -> anyhow::Result<()> {
        match self.config.save(&self.config_path) {
            Ok(()) => {
                tracing::debug!(path = %self.config_path.display(), "Config saved to disk");
                Ok(())
            }
            Err(e) => {
                tracing::error!(path = %self.config_path.display(), error = %e, "Failed to save config");
                Err(e)
            }
        }
    }

    /// Switch a camera's live view to another stream. `selector` is
    /// `"main"`, `"sub"` or a stream id; recording keeps the main stream.
    /// Restarts the camera's supervisor only when its live URL changes.
    pub fn set_live_stream(&mut self, cam_id: &str, selector: &str) -> anyhow::Result<()> {
        let cam = self
            .config
            .cameras
            .iter()
            .find(|c| c.id == cam_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Camera not found"))?;
        let Some(url) = cam.stream_url(selector) else {
            anyhow::bail!("Unknown stream");
        };

        // Unchanged URL (e.g. "sub" on a camera without a sub stream):
        // restarting would only interrupt the view.
        let unchanged = self.effective_camera_config(&cam).url == url;
        self.live_stream.insert(cam.id.clone(), selector.to_owned());
        if unchanged {
            return Ok(());
        }
        tracing::info!(cam_id = %cam_id, stream = selector, "Live stream changed");
        // Only the desktop view decodes this stream; recording and browser
        // live view keep running.
        if self.views.contains(&cam.id) {
            self.spawn_view(&cam);
        }
        Ok(())
    }

    pub fn clear_live_stream(&mut self, cam_id: &str) {
        self.live_stream.remove(cam_id);
    }

    /// Merge top-level config keys (site_name, wall, ui, debug, recording)
    /// from a JSON value. Does NOT touch cameras. Saves to disk after updating.
    pub fn update_config_settings(&mut self, value: &serde_json::Value) -> anyhow::Result<()> {
        // Checked before anything changes: a bad layout leaves all settings as they were.
        let desktop = value
            .get("desktop")
            .filter(|v| v.is_object())
            .map(crate::config::DesktopConfig::from_value)
            .transpose()?;
        let events = value
            .get("events")
            .filter(|v| v.is_object())
            .map(|events| crate::config::EventsConfig::changed(&self.config.events, events))
            .transpose()?;
        let archive = value
            .get("recording")
            .and_then(|rec| rec.get("archive"))
            .filter(|v| v.is_object())
            .map(|archive| {
                crate::config::ArchiveConfig::changed(&self.config.recording.archive, archive)
            })
            .transpose()?;
        if let Some(archive) = archive.as_ref().filter(|a| a.enabled) {
            let mut roots = crate::recording::archive::recording_roots(self);
            if let Some(path) = value["recording"]["storage_path"].as_str() {
                roots.push(expand_user_path(path));
            }
            if let Some(reason) =
                crate::recording::archive::overlap(&expand_user_path(&archive.path), &roots)
            {
                anyhow::bail!("{reason}");
            }
        }
        let local_all_before = self.config.recording.local_all_cameras;
        let storage_before = self.config.recording.storage_path.clone();
        if let Some(s) = value["site_name"].as_str() {
            self.config.site_name = s.to_owned();
        }
        if let Some(wall) = value.get("wall").filter(|v| v.is_object()) {
            if let Some(v) = wall["rows"].as_u64() {
                self.config.wall.rows = v as u32;
            }
            if let Some(v) = wall["cols"].as_u64() {
                self.config.wall.cols = v as u32;
            }
            if let Some(v) = wall["auto_fit"].as_bool() {
                self.config.wall.auto_fit = v;
            }
            if let Some(v) = wall["page_interval_sec"].as_u64() {
                self.config.wall.page_interval_sec = v as u32;
            }
        }
        if let Some(ui) = value.get("ui").filter(|v| v.is_object()) {
            if let Some(s) = ui["theme"].as_str() {
                self.config.ui.theme = s.to_owned();
            }
            if let Some(v) = ui["fullscreen"].as_bool() {
                self.config.ui.fullscreen = v;
            }
            if let Some(v) = ui["fullscreen_status"].as_bool() {
                self.config.ui.fullscreen_status = v;
            }
            if let Some(v) = ui["fullscreen_status_opacity"].as_u64() {
                self.config.ui.fullscreen_status_opacity = v.min(100) as u8;
            }
            if let Some(fps) = ui.get("preview_fps").filter(|v| v.is_object()) {
                self.config.ui.preview_fps = self.config.ui.preview_fps.updated(fps);
            }
        }
        if let Some(desktop) = desktop {
            self.config.desktop = desktop;
        }
        if let Some(dbg) = value.get("debug").filter(|v| v.is_object()) {
            if let Some(s) = dbg["gst_log"].as_str() {
                self.config.debug.gst_log = s.to_owned();
            }
        }
        if let Some(rec) = value.get("recording").filter(|v| v.is_object()) {
            if let Some(s) = rec["storage_path"].as_str() {
                self.config.recording.storage_path = s.to_owned();
            }
            if let Some(v) = rec["segment_minutes"].as_u64() {
                self.config.recording.segment_minutes = v as u32;
            }
            if let Some(v) = rec["pre_buffer_sec"].as_u64() {
                self.config.recording.pre_buffer_sec = v as u32;
            }
            if let Some(v) = rec["local_all_cameras"].as_bool() {
                self.config.recording.local_all_cameras = v;
            }
            if let Some(v) = rec["disk_threshold_percent"]
                .as_u64()
                .filter(|v| *v <= u64::from(crate::config::MAX_DISK_THRESHOLD_PERCENT))
            {
                self.config.recording.disk_threshold_percent = v as u8;
            }
        }

        if let Some(archive) = archive {
            if archive != self.config.recording.archive {
                self.config.recording.archive = archive;
                self.archive.reconfigured();
            }
        }
        if let Some(events) = events {
            // Retention applies on the next cleanup pass; a new push port
            // when the camera-event listener next starts (recorder restart).
            self.config.events = events;
        }

        let local_all_after = self.config.recording.local_all_cameras;
        let cams = self.config.cameras.clone();
        let mut restart: HashSet<CameraId> = HashSet::new();
        if storage_before != self.config.recording.storage_path {
            restart.extend(self.move_recordings(&expand_user_path(&storage_before)));
        }
        if local_all_before != local_all_after {
            if !local_all_after {
                for cam in &cams {
                    self.recording.stop_global_all_local(&cam.id);
                }
            }
            restart.extend(cams.iter().map(|cam| cam.id.clone()));
        }
        for cam in cams.iter().filter(|cam| restart.contains(&cam.id)) {
            self.restart_ingest(cam);
        }

        self.save_config()
    }

    /// The recording location moved away from `old_base`: recordings that
    /// were written there continue in the new location. Cameras with their
    /// own folder keep it. Returns the cameras whose recording moved; their
    /// ingest must restart to write the new folder.
    fn move_recordings(&mut self, old_base: &Path) -> Vec<CameraId> {
        let mut moved = Vec::new();
        for cam in self.config.cameras.clone() {
            let in_old_base = self
                .recording
                .recording_storage_path(&cam.id)
                .is_some_and(|path| path.starts_with(old_base));
            if cam.recording.local.storage_path.is_some() || !in_old_base {
                continue;
            }
            let to = self.global_recording_output_dir(&cam);
            let root = self.global_storage_root();
            if let Err(reason) = self.prepare_recording_dir(&root, &to) {
                tracing::warn!(
                    cam_id = %cam.id,
                    reason = %reason,
                    "Cannot record in the new location; recording stays in the old location"
                );
                continue;
            }
            if self.recording.relocate(&cam.id, old_base, to) {
                moved.push(cam.id.clone());
            }
        }
        moved
    }

    pub fn local_recording_output_dir(&self, cam: &CameraConfig) -> PathBuf {
        let raw_path = cam
            .recording
            .local
            .storage_path
            .clone()
            .unwrap_or_else(|| self.config.recording.storage_path.clone());
        let base = expand_user_path(&raw_path);
        let date_stamp = current_local_date_stamp();
        base.join(&cam.id).join(date_stamp)
    }

    /// The global recording location.
    pub fn global_storage_root(&self) -> PathBuf {
        expand_user_path(&self.config.recording.storage_path)
    }

    /// Where `cam` records on its own (`recording.local`, manual Start): its
    /// own folder when it has one, else the global location.
    pub fn camera_storage_root(&self, cam: &CameraConfig) -> PathBuf {
        cam.recording
            .local
            .storage_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map_or_else(|| self.global_storage_root(), expand_user_path)
    }

    /// Where `cam` records automatically: its own location when it records
    /// by itself, else the global one ("record all cameras").
    fn automatic_storage_root(&self, cam: &CameraConfig) -> PathBuf {
        if cam.recording.local.enabled {
            self.camera_storage_root(cam)
        } else {
            self.global_storage_root()
        }
    }

    /// The recording location a recording folder of `cam` lies in.
    fn storage_root_of(&self, cam: &CameraConfig, path: &Path) -> PathBuf {
        let own = self.camera_storage_root(cam);
        if path.starts_with(&own) {
            own
        } else {
            self.global_storage_root()
        }
    }

    /// Why `cam` is not recording although asked to: its recording location
    /// is missing.
    pub fn storage_blocked(&self, cam_id: &str) -> Option<&str> {
        self.storage_blocked.get(cam_id).map(String::as_str)
    }

    /// Whether `root` can be recorded to now. Its first use gives it its
    /// marker and remembers the id (`recording.storage_ids`). A location
    /// already known to be missing is not checked again here: the periodic
    /// check (`run_storage_checks`) notices when it is back.
    pub fn ensure_storage(&mut self, root: &Path) -> Result<(), String> {
        use crate::storage_guard::{bounded, Readiness};
        if let Some(Readiness::Missing(reason)) = self.storage.status(root) {
            return Err(reason);
        }
        let key = root.to_string_lossy().into_owned();
        let expected = self.config.recording.storage_ids.get(&key).cloned();
        let result = bounded(root, expected.as_deref(), &self.storage);
        if let (Ok(id), None) = (&result, &expected) {
            self.config.recording.storage_ids.insert(key, id.clone());
            if let Err(error) = self.save_config() {
                tracing::warn!(storage = %root.display(), %error, "Cannot save the recording location's id");
            }
            tracing::info!(storage = %root.display(), "Recording location marked");
        }
        let readiness = match &result {
            Ok(_) => Readiness::Ready,
            Err(reason) => Readiness::Missing(reason.clone()),
        };
        if self.storage.set(root, readiness) {
            if let Err(reason) = &result {
                tracing::warn!(storage = %root.display(), reason = %reason, "Recording location missing; nothing is recorded there");
            }
        }
        result.map(|_| ())
    }

    /// Create a recording folder `dir` under the location `root`, only while
    /// `root` is really there (never on an empty mount point).
    pub fn prepare_recording_dir(&mut self, root: &Path, dir: &Path) -> Result<(), String> {
        self.ensure_storage(root)?;
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))
    }

    pub fn global_recording_output_dir(&self, cam: &CameraConfig) -> PathBuf {
        let base = expand_user_path(&self.config.recording.storage_path);
        let date_stamp = current_local_date_stamp();
        base.join(&cam.id).join(date_stamp)
    }

    /// Reevaluate automatic schedules without restarting the encoded pipeline
    /// for Continuous↔Events transitions. Explicit Start/Stop always wins.
    pub fn reconcile_recording_policies(&mut self) {
        let now = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        self.recording_intervals
            .retain_cameras(&self.config.cameras);
        for cam in self.config.cameras.clone() {
            let mode = crate::recording::policy::current_mode(&cam);
            if self.recording.has_manual_override(&cam.id) {
                self.recording_intervals.observe(&cam.id, "", false, now);
                continue;
            }
            let enabled = cam.recording.local.enabled || self.config.recording.local_all_cameras;
            // A missing location (cached; no file-system access here) is not
            // recorded to: no restart loop while it stays missing.
            let wanted = enabled
                && mode != nw_core::camera::RecordingMode::Off
                && self.storage.is_ready(&self.automatic_storage_root(&cam));
            if wanted != self.recording.wants_recording(&cam.id) {
                if !wanted {
                    self.recording.pause_automatic(&cam.id);
                }
                self.restart_ingest(&cam);
            }
            let source = cam.motion.effective_source();
            let healthy = (source.uses_builtin()
                && self
                    .builtin_motion
                    .status(&cam.id)
                    .is_some_and(|status| status.state == "running"))
                || (source.uses_onvif()
                    && self.onvif_events.status(&cam.id).is_some_and(|status| {
                        status.state == crate::onvif_events::SubscriptionState::Subscribed
                    }));
            let fingerprint = serde_json::to_string(&(
                &cam.recording,
                &cam.motion,
                &cam.url,
                &cam.sub_url,
                self.events.activity_generation(&cam.id),
            ))
            .unwrap_or_default();
            self.recording_intervals.observe(
                &cam.id,
                &fingerprint,
                wanted
                    && !self.events.activity_paused(&cam.id)
                    && healthy
                    && self.recording_pipeline_ready(&cam.id)
                    && mode == nw_core::camera::RecordingMode::Events,
                now,
            );
        }
    }

    pub fn event_retention_since(&self, camera_id: &str) -> Option<i64> {
        let camera = self
            .config
            .cameras
            .iter()
            .find(|camera| camera.id == camera_id)?;
        if self.recording.has_manual_override(&camera.id)
            || !self.recording.wants_recording(&camera.id)
            || !self.recording_pipeline_ready(camera_id)
            || crate::recording::policy::current_mode(camera)
                != nw_core::camera::RecordingMode::Events
        {
            return None;
        }
        let source = camera.motion.effective_source();
        let healthy = (source.uses_builtin()
            && self
                .builtin_motion
                .status(camera_id)
                .is_some_and(|status| status.state == "running"))
            || (source.uses_onvif()
                && self.onvif_events.status(camera_id).is_some_and(|status| {
                    status.state == crate::onvif_events::SubscriptionState::Subscribed
                }));
        if !healthy {
            return None;
        }
        if self.events.activity_paused(camera_id) {
            return None;
        }
        self.recording_intervals.since(camera_id)
    }

    fn recording_pipeline_ready(&self, camera_id: &str) -> bool {
        self.ingest
            .get(camera_id)
            .is_some_and(|ingest| ingest.state_rx.borrow().as_str() == "recording_ready")
    }

    fn reconcile_configured_local_recording(&mut self, cam: &CameraConfig) {
        if !cam.recording.local.enabled {
            self.recording.stop_configured_local(&cam.id);
            if self.config.recording.local_all_cameras && !self.recording.wants_recording(&cam.id) {
                let storage_dir = self.global_recording_output_dir(cam);
                let root = self.global_storage_root();
                match self.prepare_recording_dir(&root, &storage_dir) {
                    Ok(()) => self
                        .recording
                        .start_global_all_local(cam.id.clone(), storage_dir),
                    Err(reason) => {
                        tracing::warn!(cam_id = %cam.id, reason = %reason, "Camera streams without recording: its recording location is unavailable");
                        self.storage_blocked.insert(cam.id.clone(), reason);
                    }
                }
            }
            return;
        }

        let storage_dir = self.local_recording_output_dir(cam);
        let root = self.camera_storage_root(cam);
        match self.prepare_recording_dir(&root, &storage_dir) {
            Ok(()) => self
                .recording
                .start_configured_local(cam.id.clone(), storage_dir),
            Err(reason) => {
                tracing::warn!(cam_id = %cam.id, reason = %reason, "Camera streams without recording: its recording location is unavailable");
                // An automatic recording already running there stops too
                // (a manual one is kept, and resumes when the location is back).
                self.recording.pause_automatic(&cam.id);
                self.storage_blocked.insert(cam.id.clone(), reason);
            }
        }
    }

    fn effective_camera_config(&self, cam: &CameraConfig) -> CameraConfig {
        // Default to the sub stream to keep multi-camera walls responsive; the
        // UI picks main (or any listed stream) via camera.set_stream.
        let selector = self
            .live_stream
            .get(&cam.id)
            .map(String::as_str)
            .unwrap_or(nw_core::camera::SUB_STREAM);
        let mut clone = cam.clone();
        if let Some(url) = cam.stream_url(selector) {
            clone.url = url;
        }
        clone
    }

    fn local_assignment(&mut self, cam: &CameraConfig) -> CameraAssignment {
        let generation = self
            .assignment_generations
            .entry(cam.id.clone())
            .or_insert(0);
        *generation += 1;
        let issued_at = OffsetDateTime::now_utc();
        // Standalone cameras have no control-plane renewer. Give their local,
        // still-fenced assignment a long compatibility lease so a healthy
        // legacy deployment is not stopped after the former 24-hour timeout.
        // Distributed assignments continue to use their supplied lease exactly.
        const LOCAL_STANDALONE_LEASE_DAYS: i64 = 365 * 100;
        let expires_at = issued_at + time::Duration::days(LOCAL_STANDALONE_LEASE_DAYS);
        CameraAssignment {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            assignment_id: format!("local-{}-{generation}", cam.id),
            camera_id: cam.id.clone(),
            node_id: "local-standalone".to_owned(),
            lease: Lease {
                token: format!("local-{}-{generation}", cam.id),
                issued_at_utc: issued_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .expect("UTC timestamp formats"),
                expires_at_utc: expires_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .expect("UTC timestamp formats"),
                duration_seconds: (LOCAL_STANDALONE_LEASE_DAYS as u64) * 24 * 60 * 60,
            },
            fencing_generation: *generation,
            assigned_at_utc: issued_at
                .format(&time::format_description::well_known::Rfc3339)
                .expect("UTC timestamp formats"),
        }
    }
}

struct IngestHandle {
    stop_tx: watch::Sender<bool>,
    state_rx: watch::Receiver<String>,
    info_rx: watch::Receiver<StreamInfo>,
}

/// Returns false when a queued replacement was stopped or superseded.
/// Must run on a blocking thread: registry installation waits for teardown.
fn install_local_assignment(
    registry: &dyn AssignmentRegistry,
    assignment: &CameraAssignment,
    stop_rx: &watch::Receiver<bool>,
) -> Result<bool, AssignmentError> {
    const INSTALL_ATTEMPTS: usize = 3;
    for attempt in 1..=INSTALL_ATTEMPTS {
        if *stop_rx.borrow() || stop_rx.has_changed().is_err() {
            return Ok(false);
        }
        let result = registry.install(assignment.clone());
        // A newer quality change or shutdown may arrive while install waits.
        if *stop_rx.borrow() || stop_rx.has_changed().is_err() {
            return Ok(false);
        }
        match result {
            Ok(()) => return Ok(true),
            Err(AssignmentError::TeardownTimeout) if attempt < INSTALL_ATTEMPTS => {
                tracing::warn!(
                    cam_id = %assignment.camera_id,
                    attempt,
                    "Waiting for prior supervisor teardown acknowledgement before replacement"
                );
            }
            Err(error) => return Err(error),
        }
    }
    Err(AssignmentError::TeardownTimeout)
}

pub fn expand_user_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    }
}

pub(crate) fn current_local_date_stamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

#[cfg(test)]
mod tests {
    use super::AppState;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;
    use nw_core::camera::CameraConfig;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::watch;

    fn test_config_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("neurowatch-{name}-{nanos}.json"))
    }

    fn app_state(config: AppConfig) -> AppState {
        AppState::new(config, test_config_path("app-state"), HwAccelKind::Software)
    }

    fn init_gstreamer() {
        let _ = gstreamer::init();
    }

    /// Mount protection: a location recorded to before, whose marker is
    /// gone (an unmounted share leaves an empty mount point), gets no new
    /// folders and no recording; the camera reports `failed`; recording
    /// resumes when the marker is back.
    #[tokio::test]
    async fn a_missing_recording_location_is_never_recorded_to_and_resumes_when_back() {
        use crate::storage_guard::{Readiness, MARKER_FILE};
        init_gstreamer();
        let root = test_config_path("mount").with_extension("");
        let mut cam = camera("on-the-nas");
        cam.recording.local.enabled = true;
        let mut cfg = AppConfig::default();
        cfg.recording.storage_path = root.to_string_lossy().into_owned();
        cfg.cameras.push(cam.clone());
        let mut st = app_state(cfg);

        st.restart_ingest(&cam);
        assert!(
            st.recording.wants_recording(&cam.id),
            "first use claims the location"
        );
        assert!(root.join(MARKER_FILE).is_file());
        let id = st.config.recording.storage_ids[&root.to_string_lossy().into_owned()].clone();
        let saved = crate::config::AppConfig::load_or_default(&st.config_path).unwrap();
        assert_eq!(
            saved.recording.storage_ids.values().next(),
            Some(&id),
            "the id is saved"
        );

        // The share is unmounted: an empty folder, no marker.
        st.stop_supervisor(&cam.id);
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        st.storage.set(&root, Readiness::Missing("gone".into()));
        st.reconcile_recording_policies();
        st.restart_ingest(&cam);
        assert!(!st.recording.wants_recording(&cam.id));
        assert!(st.storage_blocked(&cam.id).is_some());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "nothing was created on the empty mount point"
        );
        // Asking again re-checks nothing (the periodic check does) and still refuses.
        assert!(st.ensure_storage(&root).is_err());

        // Mounted again: the marker is there; the next check sees it.
        std::fs::write(root.join(MARKER_FILE), format!("x\nid={id}\n")).unwrap();
        st.storage.set(&root, Readiness::Ready);
        st.reconcile_recording_policies();
        assert!(st.recording.wants_recording(&cam.id), "recording resumes");
        assert!(st.storage_blocked(&cam.id).is_none());
        st.stop_supervisor(&cam.id);
        std::fs::remove_dir_all(root).unwrap();
        let _ = std::fs::remove_file(&st.config_path);
    }

    #[tokio::test]
    async fn off_policy_pauses_automatic_ingest_recording_but_not_manual_start() {
        init_gstreamer();
        let root = test_config_path("off-policy").with_extension("");
        let mut cam = camera("policy-off");
        cam.recording.local.enabled = true;
        cam.recording.mode = nw_core::camera::RecordingMode::Off;
        let mut cfg = AppConfig::default();
        cfg.recording.storage_path = root.to_string_lossy().into_owned();
        let mut st = app_state(cfg);
        st.restart_ingest(&cam);
        assert!(!st.recording.wants_recording(&cam.id));
        st.recording.start(cam.id.clone(), root.clone());
        st.restart_ingest(&cam);
        assert!(st.recording.wants_recording(&cam.id));
        st.stop_supervisor(&cam.id);
        if root.exists() {
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    fn camera(id: &str) -> CameraConfig {
        let mut cam = CameraConfig::new("Camera", "rtsp://example.invalid/stream");
        cam.id = id.to_owned();
        cam
    }

    #[tokio::test]
    async fn spawn_supervisor_honors_persisted_local_recording_enabled() {
        init_gstreamer();
        let mut cam = camera("cam-local-enabled");
        cam.recording.local.enabled = true;

        let mut st = app_state(AppConfig::default());
        st.spawn_supervisor(&cam);

        assert!(st.recording.wants_recording(&cam.id));
        assert!(st.recording.recording_storage_path(&cam.id).is_some());

        st.stop_supervisor(&cam.id);
    }

    #[tokio::test]
    async fn recordings_follow_a_new_recording_location() {
        init_gstreamer();
        let old_base = test_config_path("old-location").with_extension("");
        let new_base = test_config_path("new-location").with_extension("");
        let own = test_config_path("own-folder").with_extension("");
        let mut follows = camera("cam-follows");
        follows.recording.local.enabled = true;
        let mut keeps = camera("cam-keeps");
        keeps.recording.local.enabled = true;
        keeps.recording.local.storage_path = Some(own.display().to_string());
        let mut cfg = AppConfig::default();
        cfg.recording.storage_path = old_base.display().to_string();
        cfg.cameras = vec![follows.clone(), keeps.clone()];
        let mut st = app_state(cfg);
        st.spawn_supervisor(&follows);
        st.spawn_supervisor(&keeps);

        st.update_config_settings(
            &serde_json::json!({"recording": {"storage_path": new_base.display().to_string()}}),
        )
        .expect("settings should save");

        let moved = st.recording.recording_storage_path(&follows.id).cloned();
        let kept = st.recording.recording_storage_path(&keeps.id).cloned();
        st.stop_all();
        let _ = std::fs::remove_file(&st.config_path);
        for dir in [&old_base, &new_base, &own] {
            let _ = std::fs::remove_dir_all(dir);
        }
        assert!(moved.is_some_and(|path| path.starts_with(&new_base)));
        assert!(kept.is_some_and(|path| path.starts_with(&own)));
    }

    #[tokio::test]
    async fn preview_frame_rates_default_to_the_old_limits_and_are_clamped() {
        use crate::config::PreviewFpsConfig;
        let older: crate::config::UiConfig =
            serde_json::from_value(serde_json::json!({"theme": "dark", "fullscreen": true}))
                .expect("a config without the setting still loads");
        assert_eq!(older.preview_fps, PreviewFpsConfig::default());
        let partial: PreviewFpsConfig = serde_json::from_value(serde_json::json!({"single": 30}))
            .expect("missing fields take their defaults");
        assert_eq!((partial.single, partial.wall), (30, 15));

        let mut st = app_state(AppConfig::default());
        let defaults = st.config.ui.preview_fps;
        assert_eq!(
            (
                defaults.single,
                defaults.wall,
                defaults.dense,
                defaults.hidden
            ),
            (25, 15, 4, 2)
        );
        st.update_config_settings(&serde_json::json!({"ui": {"preview_fps": {
            "single": 30, "wall": 30, "dense": 0, "hidden": 500,
        }}}))
        .expect("settings should save");
        let _ = std::fs::remove_file(&st.config_path);
        let fps = st.config.ui.preview_fps;
        assert_eq!((fps.single, fps.wall), (30, 30));
        assert_eq!(fps.dense, 1, "at least 1 fps");
        assert_eq!(fps.hidden, 30, "hidden pages at most 30 fps");
    }

    #[tokio::test]
    async fn the_fullscreen_status_overlay_is_on_by_default_and_can_be_turned_off() {
        let older: crate::config::UiConfig =
            serde_json::from_value(serde_json::json!({"theme": "dark", "fullscreen": true}))
                .expect("a config without the setting still loads");
        assert!(older.fullscreen_status, "older configs keep the overlay on");
        assert_eq!(older.fullscreen_status_opacity, 50);

        let mut st = app_state(AppConfig::default());
        assert!(st.config.ui.fullscreen_status);
        st.update_config_settings(&serde_json::json!({"ui": {"fullscreen_status": false}}))
            .expect("settings should save");
        assert!(!st.config.ui.fullscreen_status);
        st.update_config_settings(&serde_json::json!({"ui": {"fullscreen_status_opacity": 30}}))
            .expect("settings should save");
        assert_eq!(st.config.ui.fullscreen_status_opacity, 30);
        st.update_config_settings(&serde_json::json!({"ui": {"fullscreen_status_opacity": 400}}))
            .expect("settings should save");
        let _ = std::fs::remove_file(&st.config_path);
        assert_eq!(
            st.config.ui.fullscreen_status_opacity, 100,
            "clamped to 100%"
        );
        assert!(st.config.ui.fullscreen, "other ui settings are unchanged");
    }

    #[tokio::test]
    async fn spawn_supervisor_does_not_record_when_local_recording_is_disabled() {
        init_gstreamer();
        let cam = camera("cam-local-disabled");

        let mut st = app_state(AppConfig::default());
        st.spawn_supervisor(&cam);

        assert!(!st.recording.wants_recording(&cam.id));
        assert!(st.recording.recording_storage_path(&cam.id).is_none());

        st.stop_supervisor(&cam.id);
    }

    #[tokio::test]
    async fn spawn_supervisor_preserves_existing_manual_recording_state() {
        init_gstreamer();
        let mut cam = camera("cam-manual");
        cam.recording.local.enabled = true;
        let manual_path = std::env::temp_dir().join("neurowatch-manual-recording");

        let mut st = app_state(AppConfig::default());
        st.recording.start(cam.id.clone(), manual_path.clone());
        st.spawn_supervisor(&cam);

        assert_eq!(
            st.recording.recording_storage_path(&cam.id),
            Some(&manual_path)
        );

        st.stop_supervisor(&cam.id);
    }

    #[tokio::test]
    async fn spawn_supervisor_preserves_manual_stop_over_persisted_local_recording() {
        init_gstreamer();
        let mut cam = camera("cam-manual-stop");
        cam.recording.local.enabled = true;
        let manual_path = std::env::temp_dir().join("neurowatch-manual-stop-recording");

        let mut st = app_state(AppConfig::default());
        st.recording.start(cam.id.clone(), manual_path);
        st.recording.stop(&cam.id);
        st.spawn_supervisor(&cam);

        assert!(!st.recording.wants_recording(&cam.id));
        assert!(st.recording.recording_storage_path(&cam.id).is_none());

        st.stop_supervisor(&cam.id);
    }

    #[tokio::test]
    async fn spawn_supervisor_reports_failed_teardown_without_blocking_control_plane() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        init_gstreamer();
        let cam = camera("cam-slow-teardown");
        let mut st = app_state(AppConfig::default());
        let assignment = st.local_assignment(&cam);
        st.assignment_registry.install(assignment.clone()).unwrap();
        let unreleased = st
            .assignment_registry
            .acquire(&assignment, time::OffsetDateTime::now_utc())
            .expect("initial assignment should acquire");
        let registry = Arc::clone(&st.assignment_registry);
        let state = Arc::new(tokio::sync::RwLock::new(st));

        let start = Instant::now();
        let mut state_rx = {
            let mut st = state.write().await;
            st.spawn_view(&cam);
            st.supervisor_state_rx[&cam.id].clone()
        };
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "spawn must queue the replacement without waiting for prior teardown"
        );

        // This default current-thread Tokio runtime also proves the timer and
        // state readers can run while the blocking registry wait is in flight.
        tokio::time::timeout(Duration::from_millis(500), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let st = state.read().await;
            assert_eq!(
                st.supervisor_state_rx[&cam.id].borrow().as_str(),
                "connecting"
            );
        })
        .await
        .expect("control-plane reads must stay responsive");

        tokio::time::timeout(Duration::from_secs(5), async {
            while state_rx.borrow().as_str() != "teardown_failed" {
                state_rx.changed().await.unwrap();
            }
        })
        .await
        .expect("an unreleased prior owner must fail closed after bounded retries");

        {
            use crate::stats::CameraHealthSource;
            let st = state.read().await;
            assert_eq!(st.stream_health.snapshot()[0].state, "teardown_failed");
        }
        let replacement = state.write().await.local_assignment(&cam);
        assert!(matches!(
            registry.acquire(&replacement, time::OffsetDateTime::now_utc()),
            Err(crate::assignment::AssignmentError::NotCurrent)
        ));
        unreleased.release();
        state.write().await.stop_all();
    }

    #[tokio::test]
    async fn background_replacement_installs_only_after_prior_owner_releases() {
        use std::sync::Arc;
        use std::time::Duration;

        let cam = camera("cam-delayed-replacement");
        let mut st = app_state(AppConfig::default());
        let initial = st.local_assignment(&cam);
        st.assignment_registry.install(initial.clone()).unwrap();
        let owner = st
            .assignment_registry
            .acquire(&initial, time::OffsetDateTime::now_utc())
            .unwrap();
        let replacement = st.local_assignment(&cam);
        let registry = Arc::clone(&st.assignment_registry);
        let (stop_tx, stop_rx) = watch::channel(false);
        let task_registry = Arc::clone(&registry);
        let task_assignment = replacement.clone();
        let task = tokio::task::spawn_blocking(move || {
            super::install_local_assignment(&*task_registry, &task_assignment, &stop_rx)
        });

        // Wait for install to revoke the prior owner, without releasing its
        // active fence. There must still be no replacement owner at this point.
        tokio::time::timeout(Duration::from_secs(1), async {
            while owner.permits(time::OffsetDateTime::now_utc()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background install must request prior teardown");
        assert!(!task.is_finished());
        assert!(matches!(
            registry.acquire(&replacement, time::OffsetDateTime::now_utc()),
            Err(crate::assignment::AssignmentError::NotCurrent)
        ));

        owner.release();
        assert_eq!(task.await.unwrap(), Ok(true));
        let replacement_owner = registry
            .acquire(&replacement, time::OffsetDateTime::now_utc())
            .expect("replacement can acquire only after verified teardown");
        replacement_owner.release();
        drop(stop_tx);
    }

    #[test]
    fn stopped_replacement_does_not_install_an_assignment() {
        use crate::assignment::LocalAssignmentRegistry;
        use time::OffsetDateTime;

        let cam = camera("cam-cancelled-startup");
        let mut st = app_state(AppConfig::default());
        let assignment = st.local_assignment(&cam);
        let registry = LocalAssignmentRegistry::new();
        let (stop_tx, stop_rx) = watch::channel(false);
        stop_tx.send(true).unwrap();

        assert_eq!(
            super::install_local_assignment(&registry, &assignment, &stop_rx),
            Ok(false)
        );
        assert!(registry
            .permits(&assignment, OffsetDateTime::now_utc())
            .is_err());
    }

    #[test]
    fn replacement_stopped_during_install_does_not_retry_or_start() {
        use crate::assignment::{ActiveAssignmentLease, AssignmentError, AssignmentRegistry};
        use nw_core::control_plane::CameraAssignment;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use time::OffsetDateTime;

        struct StoppedDuringInstall {
            stop_tx: watch::Sender<bool>,
            attempts: AtomicUsize,
        }
        impl AssignmentRegistry for StoppedDuringInstall {
            fn install(&self, _: CameraAssignment) -> Result<(), AssignmentError> {
                self.attempts.fetch_add(1, Ordering::Relaxed);
                self.stop_tx.send(true).unwrap();
                Err(AssignmentError::TeardownTimeout)
            }

            fn acquire(
                &self,
                _: &CameraAssignment,
                _: OffsetDateTime,
            ) -> Result<Box<dyn ActiveAssignmentLease>, AssignmentError> {
                panic!("a stopped replacement must never acquire ownership")
            }
        }

        let cam = camera("cam-superseded-startup");
        let mut st = app_state(AppConfig::default());
        let assignment = st.local_assignment(&cam);
        let (stop_tx, stop_rx) = watch::channel(false);
        let registry = StoppedDuringInstall {
            stop_tx,
            attempts: AtomicUsize::new(0),
        };

        assert_eq!(
            super::install_local_assignment(&registry, &assignment, &stop_rx),
            Ok(false)
        );
        assert_eq!(registry.attempts.load(Ordering::Relaxed), 1);
    }

    /// Deterministic recording lifecycle proof using an injectable state harness.
    ///
    /// The `watch::Sender<String>` acts as a stand-in for the real GStreamer
    /// pipeline state (the "recording branch event harness"). Injecting state
    /// transitions this way proves the AppState/RecordingSupervisor boundary
    /// without sleeping, using real network cameras, or relying on splitmuxsink
    /// timing. Each step asserts the exact expected status before advancing.
    ///
    /// Steps exercised:
    ///   1. `streaming`  — pipeline up, recording branch not yet open → `starting`
    ///   2. `recording_ready` — first segment opened (sink-open event) → `recording`
    ///   3. `error`      — pipeline failure → `failed`
    ///   4. disable + stop → `idle`; stop handle retained (fence in place)
    ///   5. repath: new storage path accepted while old fence prevents overlap
    #[test]
    fn recording_lifecycle_demonstrates_ready_failure_and_repath() {
        let cam_id = "cam-lifecycle".to_owned();
        let (stop_tx, _stop_rx) = watch::channel(false);
        let (state_tx, state_rx) = watch::channel("connecting".to_owned());
        let (_stream_info_tx, stream_info_rx) =
            watch::channel(crate::pipeline::supervisor::StreamInfo::default());

        let mut st = app_state(AppConfig::default());
        st.ingest.insert(
            cam_id.clone(),
            super::IngestHandle {
                stop_tx,
                state_rx,
                info_rx: stream_info_rx,
            },
        );
        st.recording
            .start(cam_id.clone(), std::path::PathBuf::from("/tmp/rec"));

        // 1. Pipeline reached streaming but recording branch not yet confirmed open.
        state_tx.send("streaming".to_owned()).unwrap();
        let statuses = st
            .recording
            .status_all_with_pipeline_states(&st.supervisor_states());
        assert_eq!(
            statuses.get(&cam_id).map(String::as_str),
            Some("starting"),
            "recording must report 'starting' until first segment is opened"
        );

        // 2. splitmuxsink-fragment-opened element message received by supervisor →
        //    supervisor publishes `recording_ready` (the definitive sink-open event).
        state_tx.send("recording_ready".to_owned()).unwrap();
        assert!(st.recording_pipeline_ready(&cam_id));
        let statuses = st
            .recording
            .status_all_with_pipeline_states(&st.supervisor_states());
        assert_eq!(
            statuses.get(&cam_id).map(String::as_str),
            Some("recording"),
            "recording must report 'recording' only after the sink-open event"
        );

        // 3. Pipeline failure maps to failed status.
        state_tx.send("error".to_owned()).unwrap();
        assert!(!st.recording_pipeline_ready(&cam_id));
        let statuses = st
            .recording
            .status_all_with_pipeline_states(&st.supervisor_states());
        assert_eq!(
            statuses.get(&cam_id).map(String::as_str),
            Some("failed"),
            "recording must report 'failed' on pipeline error"
        );

        // 4. Disable recording and stop the camera.
        st.recording.stop(&cam_id);
        st.stop_supervisor(&cam_id);
        let statuses = st
            .recording
            .status_all_with_pipeline_states(&st.supervisor_states());
        assert_eq!(
            statuses.get(&cam_id).map(String::as_str),
            Some("idle"),
            "recording must report 'idle' after recording is stopped"
        );

        // 5. Repath: new storage path accepted while the old fence is still in
        //    place (the background startup waits until the assignment registry clears).
        let new_path = std::path::PathBuf::from("/tmp/rec-new");
        st.recording.start(cam_id.clone(), new_path.clone());
        assert_eq!(
            st.recording.recording_storage_path(&cam_id),
            Some(&new_path),
            "repath must update the storage path"
        );
    }

    #[test]
    fn stop_supervisor_retains_handles_until_terminal_shutdown_state() {
        let cam_id = "cam-retain-stop-handles".to_owned();
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let (_state_tx, state_rx) = watch::channel("streaming".to_owned());
        let (_stream_info_tx, stream_info_rx) =
            watch::channel(crate::pipeline::supervisor::StreamInfo::default());
        let mut st = app_state(AppConfig::default());
        st.stop_txs.insert(cam_id.clone(), stop_tx);
        st.supervisor_state_rx.insert(cam_id.clone(), state_rx);
        st.supervisor_stream_info_rx
            .insert(cam_id.clone(), stream_info_rx);

        st.stop_supervisor(&cam_id);

        assert!(*stop_rx.borrow_and_update());
        assert!(st.stop_txs.contains_key(&cam_id));
        assert!(st.supervisor_state_rx.contains_key(&cam_id));
        assert!(st.supervisor_stream_info_rx.contains_key(&cam_id));
    }

    fn configured(cams: &[CameraConfig]) -> AppState {
        app_state(AppConfig {
            cameras: cams.to_vec(),
            ..AppConfig::default()
        })
    }

    #[tokio::test]
    async fn live_audio_follows_the_latest_listener_and_its_lease() {
        // File cameras: the listener thread ends at once (live audio needs RTSP).
        let mut a = CameraConfig::new("A", "/nonexistent/a.mkv");
        a.id = "cam-a".into();
        let mut b = CameraConfig::new("B", "/nonexistent/b.mkv");
        b.id = "cam-b".into();
        let mut st = configured(&[a, b]);
        let ttl = std::time::Duration::from_secs(10);
        assert_eq!(st.listening(), None);

        st.set_listen("ui-1", Some("cam-a".into()), ttl);
        assert_eq!(st.listening().as_deref(), Some("cam-a"));
        std::thread::sleep(std::time::Duration::from_millis(5));
        st.set_listen("ui-2", Some("cam-b".into()), ttl);
        assert_eq!(
            st.listening().as_deref(),
            Some("cam-b"),
            "one output: the latest"
        );

        st.release_view_demand("ui-2");
        assert_eq!(st.listening().as_deref(), Some("cam-a"));
        st.set_listen("ui-1", None, ttl);
        assert_eq!(st.listening(), None, "stop listening");

        st.set_listen(
            "ui-1",
            Some("cam-a".into()),
            std::time::Duration::from_millis(1),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        st.expire_view_demand(std::time::Instant::now());
        assert_eq!(st.listening(), None, "a UI that stops renewing goes silent");

        st.set_listen("ui-1", Some("cam-gone".into()), ttl);
        assert_eq!(st.listening(), None, "only configured cameras");

        st.set_headless(true);
        st.set_listen("ui-1", Some("cam-a".into()), ttl);
        assert_eq!(st.listening(), None, "headless has no audio output");
        st.stop_all();
    }

    #[tokio::test]
    async fn a_camera_is_decoded_only_while_a_desktop_view_shows_it() {
        init_gstreamer();
        let (a, b) = (camera("cam-view-a"), camera("cam-view-b"));
        let mut st = configured(&[a.clone(), b.clone()]);
        st.spawn_supervisor(&a);
        st.spawn_supervisor(&b);
        assert_eq!(st.active_views(), 0, "no desktop view: no decoder");
        assert_eq!(
            st.supervisor_states().len(),
            2,
            "every camera still has a status"
        );

        let shown = std::collections::HashSet::from([a.id.clone()]);
        st.set_view_demand("ui-1", shown.clone(), std::time::Duration::from_secs(10));
        assert_eq!(st.active_views(), 1);
        // A second UI showing the same camera shares its decoder.
        st.set_view_demand("ui-2", shown, std::time::Duration::from_secs(10));
        assert_eq!(st.active_views(), 1);
        st.release_view_demand("ui-1");
        assert_eq!(st.active_views(), 1, "still shown by ui-2");
        st.release_view_demand("ui-2");
        assert_eq!(st.active_views(), 0);
        st.stop_all();
    }

    #[tokio::test]
    async fn a_ui_that_stops_renewing_its_lease_releases_its_decoders() {
        init_gstreamer();
        let cam = camera("cam-view-lapse");
        let mut st = configured(std::slice::from_ref(&cam));
        st.spawn_supervisor(&cam);
        st.set_view_demand(
            "crashed-ui",
            std::collections::HashSet::from([cam.id.clone()]),
            std::time::Duration::from_millis(50),
        );
        assert_eq!(st.active_views(), 1);
        assert!(st.expire_view_demand(std::time::Instant::now()).is_empty());
        let lapsed =
            st.expire_view_demand(std::time::Instant::now() + std::time::Duration::from_secs(1));
        assert_eq!(lapsed, vec!["crashed-ui".to_owned()]);
        assert_eq!(st.active_views(), 0);
        st.stop_all();
    }

    #[tokio::test]
    async fn a_stream_switch_restarts_only_the_desktop_view() {
        init_gstreamer();
        let mut cam = camera("cam-view-switch");
        cam.sub_url = Some("rtsp://example.invalid/sub".into());
        let mut st = configured(std::slice::from_ref(&cam));
        st.spawn_supervisor(&cam);
        let ingest_before = st.ingest[&cam.id].stop_tx.clone();
        st.set_view_demand(
            "ui",
            std::collections::HashSet::from([cam.id.clone()]),
            std::time::Duration::from_secs(10),
        );
        st.set_live_stream(&cam.id, "main").unwrap();
        assert!(
            !*ingest_before.borrow(),
            "recording and status keep running"
        );
        assert!(ingest_before.same_channel(&st.ingest[&cam.id].stop_tx));
        assert_eq!(st.active_views(), 1);
        st.stop_all();
    }

    #[tokio::test]
    async fn changing_recording_restarts_the_ingest_but_not_the_desktop_view() {
        init_gstreamer();
        let cam = camera("cam-view-record");
        let mut st = configured(std::slice::from_ref(&cam));
        st.spawn_supervisor(&cam);
        st.set_view_demand(
            "ui",
            std::collections::HashSet::from([cam.id.clone()]),
            std::time::Duration::from_secs(10),
        );
        let view_before = st.stop_txs[&cam.id].clone();
        st.recording
            .start(cam.id.clone(), std::env::temp_dir().join("nw-view-record"));
        st.restart_ingest(&cam);
        assert!(!*view_before.borrow(), "the desktop view was not restarted");
        assert!(view_before.same_channel(&st.stop_txs[&cam.id]));
        st.stop_all();
    }

    #[tokio::test]
    async fn a_source_without_compressed_video_reports_recording_unsupported() {
        init_gstreamer();
        let mut cam = CameraConfig::new("USB", "/dev/video-neurowatch-test");
        cam.id = "cam-usb".into();
        cam.recording.local.enabled = true;
        let mut st = configured(std::slice::from_ref(&cam));
        st.spawn_supervisor(&cam);
        assert!(st.recording_unsupported(&cam.id));
        st.stop_all();
    }
}

#[cfg(test)]
#[path = "recording/qualification.rs"]
mod recording_qualification;
