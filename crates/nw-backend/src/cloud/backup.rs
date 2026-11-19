//! Cloud Backup on the recorder: the cameras the portal says have a plan are
//! copied in real time to a backup ingest (`docs/plans/2026-11-07-cloud-backup.md`).
//!
//! Every [`CHECK_INTERVAL`] the portal is asked which cameras have a plan
//! (`link::backup`). For each, a capture thread subscribes to the camera's
//! shared connection ([`SubscriberPolicy::cloud_backup`]: no new camera
//! connection, nothing decoded) and pushes its video, Annex-B, into a
//! `nw_backup_client` track; the client sends it, keeps what is unconfirmed
//! (the outage folder), reconnects and renews. Main is used when it fits the
//! plan, else the sub stream. With sound in the plan, the camera's sound
//! goes too: AAC as it is, other sound converted to AAC (`backup_sound`).
//!
//! A ticket names how each stream was when its plan was priced
//! (`CameraGrant::priced`); a stream that sends clearly more for
//! [`CHANGED_AFTER`] (bitrate, frame rate, size, H.265 → H.264) is still
//! backed up, and is reported (`CameraReport::stream_changed`, the
//! `backup_stream_changed` event) until it is back or priced again.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nw_backup_client::{AssignFuture, Assigner, Assignment, Backup, Config as ClientConfig, Track};
use nw_backup_protocol::{
    frame::{flags, MediaKind, MediaUnit},
    limits::Limits,
    messages::{Codec, TrackFormat, TrackKind},
    ticket::{self, PricedStream, SeenStream},
};
use nw_core::encoded::VideoCodec;
use serde::Serialize;
use tokio::sync::{watch, RwLock};

use super::backup_sound::SoundPath;
use super::link::backup::{parse_backup_reply, BackupAssignment};
use super::link::services::BACKUP_SERVICE;
use super::link::{CloudError, CloudNode, LinkPhase};
use crate::app_state::AppState;
use crate::streams::{annexb::AnnexB, Delivery, Media, Phase, SubscriberPolicy, Unit};

/// How often the portal is asked which cameras have a plan.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// After the portal says it offers no backup (an older portal).
const UNSUPPORTED_PAUSE: Duration = Duration::from_secs(300);
/// Status is refreshed this often.
const STATUS_INTERVAL: Duration = Duration::from_secs(2);
/// An assignment fetched for the plan check is handed to the client if it
/// asks within this long, instead of asking the portal again.
const FRESH: Duration = Duration::from_secs(15);
/// Per backed-up camera: units held in memory, then in the outage folder.
const MEMORY_PER_TRACK: usize = 32 * 1024 * 1024;
const DISK_PER_TRACK: u64 = 2 * 1024 * 1024 * 1024;

/// `<data_local_dir>/neurowatch/backup-outage` (a temporary folder in tests).
pub fn default_outage_dir() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join(format!("nw-backup-outage-{}", std::process::id()));
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("neurowatch")
        .join("backup-outage")
}

/// What the backup loop needs from the backend.
#[derive(Clone)]
pub struct BackupSource {
    pub state: Arc<RwLock<AppState>>,
    /// Unconfirmed units during an outage.
    pub outage_dir: PathBuf,
    pub status: Arc<BackupStatus>,
}

/// A camera not backed up for this long is failing (an event, a red symbol);
/// shorter troubles (a reconnect, a brief backlog) are not.
pub const FAIL_AFTER: Duration = Duration::from_secs(90);
/// Footage waiting to be confirmed longer than this is a backup not keeping up.
const BEHIND_FAILS_MS: i64 = 120_000;
/// A stream sending more than its plan was priced for this long is changed
/// (the bitrate is already a minute's average; a brief burst is not).
pub const CHANGED_AFTER: Duration = Duration::from_secs(300);

/// The latest status, for `cloud.backup_status`, `system.status` and the
/// recorder's health events.
#[derive(Default)]
pub struct BackupStatus {
    report: Mutex<StatusReport>,
    /// When each camera's current trouble began.
    troubled: Mutex<HashMap<String, std::time::Instant>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StatusReport {
    /// "off", "connecting", "connected" or "retrying".
    pub state: String,
    /// Why it is off or not connected, in plain words.
    pub problem: Option<String>,
    pub ingest: Option<String>,
    pub transport: Option<String>,
    pub cameras: Vec<CameraReport>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CameraReport {
    pub camera_id: String,
    /// "main" or "sub".
    pub stream: String,
    /// Why it is not being backed up, if it is not.
    pub problem: Option<String>,
    pub unconfirmed_units: usize,
    pub disk_bytes: u64,
    /// How far the confirmed footage is behind now.
    pub behind_ms: i64,
    /// Not backed up for [`FAIL_AFTER`] or longer, and why.
    pub failing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trouble: Option<String>,
    /// The stream sends more than its plan was priced for (still backed up), and how.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_changed: Option<String>,
}

/// Which backed-up streams have sent more than they were priced for, and
/// since when: a change counts once it lasted [`CHANGED_AFTER`].
#[derive(Default)]
struct ChangeWatch {
    since: HashMap<String, Instant>,
    shown: HashMap<String, String>,
}

impl ChangeWatch {
    /// `seen`: each backed-up camera's changes now (empty: none).
    fn observe(&mut self, seen: Vec<(String, Vec<String>)>, now: Instant) {
        let mut since = HashMap::new();
        let mut shown = HashMap::new();
        for (camera, changes) in seen {
            if changes.is_empty() {
                continue;
            }
            let began = self.since.get(&camera).copied().unwrap_or(now);
            since.insert(camera.clone(), began);
            if now.saturating_duration_since(began) >= CHANGED_AFTER {
                shown.insert(camera, changed_text(&changes));
            }
        }
        self.since = since;
        self.shown = shown;
    }

    fn changed(&self, camera: &str) -> Option<String> {
        self.shown.get(camera).cloned()
    }
}

fn changed_text(changes: &[String]) -> String {
    format!(
        "The stream now sends more than its backup was priced for: {}. It is still backed up; \
         set the camera back, or start its backup again in NeuroWatch Cloud to price it as it is.",
        changes.join(", ")
    )
}

/// What is wrong with a camera's backup now, if anything.
fn trouble(report: &StatusReport, camera: &CameraReport) -> Option<String> {
    if let Some(problem) = &camera.problem {
        return Some(problem.clone());
    }
    if report.state != "connected" {
        return Some(match &report.problem {
            Some(problem) => format!("Not connected to the backup server: {problem}"),
            None => "Not connected to the backup server.".to_owned(),
        });
    }
    (camera.behind_ms > BEHIND_FAILS_MS).then(|| {
        format!(
            "Footage is {} min behind: the internet upload is too slow for this camera's stream.",
            camera.behind_ms / 60_000
        )
    })
}

impl BackupStatus {
    pub fn report(&self) -> StatusReport {
        let report = self.report.lock().map(|r| r.clone()).unwrap_or_default();
        if report.state.is_empty() {
            return StatusReport {
                state: "off".into(),
                problem: Some("Cloud Backup has not started.".into()),
                ..StatusReport::default()
            };
        }
        report
    }

    fn set(&self, report: StatusReport) {
        self.set_at(report, std::time::Instant::now());
    }

    /// Record `report`, marking each camera troubled since [`FAIL_AFTER`]
    /// or longer as failing.
    fn set_at(&self, mut report: StatusReport, now: std::time::Instant) {
        if let Ok(mut troubled) = self.troubled.lock() {
            let mut seen = HashMap::new();
            for index in 0..report.cameras.len() {
                let why = trouble(&report, &report.cameras[index]);
                let camera = &mut report.cameras[index];
                if let Some(why) = why {
                    let since = troubled.get(&camera.camera_id).copied().unwrap_or(now);
                    seen.insert(camera.camera_id.clone(), since);
                    camera.failing = now.saturating_duration_since(since) >= FAIL_AFTER;
                    camera.trouble = Some(why);
                }
            }
            *troubled = seen;
        }
        if let Ok(mut current) = self.report.lock() {
            *current = report;
        }
    }
}

/// The portal, for the client: assignments, the plan check's if recent.
struct PortalAssigner {
    node: CloudNode,
    cached: Mutex<Option<(Instant, BackupAssignment)>>,
}

impl PortalAssigner {
    fn remember(&self, assignment: BackupAssignment) {
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Some((Instant::now(), assignment));
        }
    }
}

impl Assigner for PortalAssigner {
    fn assign(&self) -> AssignFuture<'_> {
        Box::pin(async move {
            let recent = self
                .cached
                .lock()
                .ok()
                .and_then(|mut cached| cached.take())
                .filter(|(at, _)| at.elapsed() < FRESH)
                .map(|(_, assignment)| assignment);
            let assignment = match recent {
                Some(assignment) => assignment,
                None => fetch(&self.node)
                    .await?
                    .ok_or_else(|| "No camera has a backup plan.".to_owned())?,
            };
            Ok(Assignment {
                ticket: assignment.ticket,
                server_name: assignment.ingest.name,
                quic: assignment.ingest.quic,
                tcp: assignment.ingest.tcp,
                instance_key: assignment.ingest.public_key,
            })
        })
    }
}

/// The assignment: from the portal, or (debug builds) from the file
/// `NW_BACKUP_DEV_ASSIGNMENT` names, in the portal's reply format.
async fn fetch(node: &CloudNode) -> Result<Option<BackupAssignment>, String> {
    if let Some(path) = dev_assignment_file() {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let reply: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        return parse_backup_reply(&reply).map_err(|e| e.to_string());
    }
    // An older portal has no `service-tickets`: ask the way it understands.
    if let Ok(entries) = node.hosted_services().await {
        return Ok(entries
            .into_iter()
            .find(|entry| entry.service == BACKUP_SERVICE)
            .map(BackupAssignment::from_service));
    }
    node.backup_assignment().await.map_err(|error| match error {
        CloudError::Network(message) if message == "HTTP 404 Not Found" => UNSUPPORTED.to_owned(),
        other => other.to_string(),
    })
}

const UNSUPPORTED: &str = "This cloud does not offer backup yet.";

fn dev_assignment_file() -> Option<PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var_os("NW_BACKUP_DEV_ASSIGNMENT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

struct Running {
    backup: Arc<Backup>,
    assigner: Arc<PortalAssigner>,
    captures: HashMap<String, Capture>,
    /// Each camera's stream as its plan priced it (tickets that say).
    priced: HashMap<String, PricedStream>,
    changes: ChangeWatch,
}

pub(crate) async fn run_loop(
    node: CloudNode,
    source: BackupSource,
    mut stop: watch::Receiver<bool>,
) {
    let mut running: Option<Running> = None;
    let mut next_check = tokio::time::Instant::now();
    let mut off: Option<String> = None;
    loop {
        if tokio::time::Instant::now() >= next_check {
            next_check = tokio::time::Instant::now() + CHECK_INTERVAL;
            let linked =
                node.status().phase == LinkPhase::Linked || dev_assignment_file().is_some();
            let checked = if linked {
                fetch(&node).await
            } else {
                Err("Not linked to NeuroWatch Cloud.".into())
            };
            match checked {
                Ok(Some(assignment)) => {
                    off = None;
                    if let Err(problem) = apply(&node, &source, &mut running, assignment) {
                        off = Some(problem);
                    }
                }
                Ok(None) => {
                    off = Some("No camera has a backup plan.".into());
                    stop_running(&mut running).await;
                }
                Err(problem) => {
                    if problem == UNSUPPORTED {
                        next_check = tokio::time::Instant::now() + UNSUPPORTED_PAUSE;
                    }
                    if !linked || problem == UNSUPPORTED {
                        stop_running(&mut running).await;
                        off = Some(problem);
                    } else {
                        // A failed check keeps what runs: the client
                        // reconnects and renews on its own.
                        tracing::warn!(%problem, "Cloud Backup: the plan check failed");
                    }
                }
            }
        }
        if let Some(running) = running.as_mut() {
            let seen = seen_changes(&source.state, running).await;
            running.changes.observe(seen, Instant::now());
        }
        source.status.set(report(&running, off.as_deref()));
        tokio::select! {
            _ = tokio::time::sleep(STATUS_INTERVAL) => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
        }
    }
    stop_running(&mut running).await;
}

/// Start or update backing up for an assignment's cameras.
fn apply(
    node: &CloudNode,
    source: &BackupSource,
    running: &mut Option<Running>,
    assignment: BackupAssignment,
) -> Result<(), String> {
    let claims =
        ticket::read_claims(&assignment.ticket).ok_or("The backup ticket could not be read.")?;
    if running.is_none() {
        let key = node
            .identity_key()
            .ok_or("This recorder has no cloud identity yet.")?;
        let ca = match assignment.ingest.ca_pem.as_deref() {
            Some(pem) => {
                Some(pem_der(pem).ok_or("The backup server's development CA is not PEM.")?)
            }
            None => None,
        };
        let tls = nw_backup_client::tls_config(ca.as_deref())?;
        let assigner = Arc::new(PortalAssigner {
            node: node.shared(),
            cached: Mutex::new(None),
        });
        let backup = Arc::new(Backup::start(
            ClientConfig {
                node_key: key,
                recorder: env!("CARGO_PKG_VERSION").into(),
                tls,
                outage_dir: Some(source.outage_dir.clone()),
                memory_bytes_per_track: MEMORY_PER_TRACK,
                disk_bytes_per_track: DISK_PER_TRACK,
            },
            assigner.clone() as Arc<dyn Assigner>,
        ));
        tracing::info!(cameras = claims.cameras.len(), ingest = %assignment.ingest.name, "Cloud Backup: starting");
        *running = Some(Running {
            backup,
            assigner,
            captures: HashMap::new(),
            priced: HashMap::new(),
            changes: ChangeWatch::default(),
        });
    }
    let Some(running) = running.as_mut() else {
        return Ok(());
    };
    running.assigner.remember(assignment);
    running.priced = claims
        .cameras
        .iter()
        .filter_map(|grant| Some((grant.camera_id.clone(), grant.priced.clone()?)))
        .collect();
    // Cameras with a plan get a capture (limits follow plan changes);
    // cameras that lost theirs stop.
    for grant in &claims.cameras {
        match running.captures.get(&grant.camera_id) {
            Some(capture) if capture.sound == grant.sound && capture.named == named(grant) => {
                capture.set_limits(grant.limits)
            }
            existing => {
                // New, sound switched on or off, or another stream: (re)start its capture.
                if existing.is_some() {
                    if let Some(old) = running.captures.remove(&grant.camera_id) {
                        tokio::task::spawn_blocking(move || drop(old));
                    }
                }
                let capture = Capture::start(
                    source.state.clone(),
                    running.backup.clone(),
                    grant.camera_id.clone(),
                    grant.limits,
                    grant.sound,
                    named(grant),
                );
                running.captures.insert(grant.camera_id.clone(), capture);
            }
        }
    }
    let gone: Vec<String> = running
        .captures
        .keys()
        .filter(|id| claims.camera(id).is_none())
        .cloned()
        .collect();
    for id in gone {
        if let Some(capture) = running.captures.remove(&id) {
            tokio::task::spawn_blocking(move || drop(capture));
        }
    }
    Ok(())
}

/// What each priced camera's stream sends more of than its plan was priced
/// for: its size and codec from the capture, its rates as the recorder
/// measures them (`StreamHubs::video_formats`: a minute's average bitrate).
async fn seen_changes(state: &RwLock<AppState>, running: &Running) -> Vec<(String, Vec<String>)> {
    let state = state.read().await;
    running
        .priced
        .iter()
        .filter_map(|(camera_id, priced)| {
            let capture = running.captures.get(camera_id)?.info();
            let camera = state.config.cameras.iter().find(|c| &c.id == camera_id)?;
            let format = state
                .streams
                .video_formats(camera)
                .get(capture.stream)
                .copied();
            let seen = SeenStream {
                width: format.and_then(|f| f.width),
                height: format.and_then(|f| f.height),
                fps: format.and_then(|f| f.fps),
                bitrate_bps: format.and_then(|f| f.video_bps),
                codec: capture.codec,
            };
            Some((camera_id.clone(), priced.changes(&seen)))
        })
        .collect()
}

async fn stop_running(running: &mut Option<Running>) {
    let Some(stopped) = running.take() else {
        return;
    };
    // The client first: it keeps what is unconfirmed in the outage folder
    // for the next start, before the captures' tracks go.
    stopped.backup.stop().await;
    let captures: Vec<Capture> = stopped.captures.into_values().collect();
    let _ = tokio::task::spawn_blocking(move || drop(captures)).await;
    tracing::info!("Cloud Backup: stopped");
}

fn report(running: &Option<Running>, off: Option<&str>) -> StatusReport {
    let Some(running) = running else {
        return StatusReport {
            state: "off".into(),
            problem: off.map(str::to_owned),
            ..StatusReport::default()
        };
    };
    let status = running.backup.status();
    let mut cameras: Vec<CameraReport> = running
        .captures
        .iter()
        .map(|(id, capture)| {
            let info = capture.info();
            let track = status
                .tracks
                .iter()
                .find(|t| &t.camera_id == id && t.kind == TrackKind::Video);
            CameraReport {
                camera_id: id.clone(),
                stream: info.stream.into(),
                problem: info
                    .problem
                    .or_else(|| track.and_then(|t| t.refused.clone())),
                unconfirmed_units: track.map_or(0, |t| t.unconfirmed_units),
                disk_bytes: track.map_or(0, |t| t.disk_bytes),
                behind_ms: track.map_or(0, |t| t.behind_ms),
                failing: false,
                trouble: None,
                stream_changed: running.changes.changed(id),
            }
        })
        .collect();
    cameras.sort_by(|a, b| a.camera_id.cmp(&b.camera_id));
    StatusReport {
        state: status.state.into(),
        problem: off.map(str::to_owned).or(status.problem),
        ingest: status.ingest,
        transport: status.transport.map(str::to_owned),
        cameras,
    }
}

/// The DER of the first certificate in `pem`.
fn pem_der(pem: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN CERTIFICATE-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .ok()
        .filter(|d| !d.is_empty())
}

#[derive(Debug, Clone, Default)]
struct CaptureInfo {
    stream: &'static str,
    problem: Option<String>,
    /// The codec being sent (`h264`, `h265`), once known.
    codec: Option<&'static str>,
}

/// The stream a plan is for, as the capture names it (`None`: an older
/// ticket; the main stream while it fits the limits, else the sub).
fn named(grant: &nw_backup_protocol::ticket::CameraGrant) -> Option<&'static str> {
    match grant.named_stream() {
        Some("main") => Some("main"),
        Some("sub") => Some("sub"),
        _ => None,
    }
}

/// One camera's capture thread.
struct Capture {
    /// Whether this capture copies the camera's sound.
    sound: bool,
    /// The stream the plan is for, if it names one.
    named: Option<&'static str>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    info: Arc<Mutex<CaptureInfo>>,
    limits: Arc<Mutex<Limits>>,
}

impl Capture {
    fn start(
        state: Arc<RwLock<AppState>>,
        backup: Arc<Backup>,
        camera_id: String,
        limits: Limits,
        sound: bool,
        named: Option<&'static str>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let info = Arc::new(Mutex::new(CaptureInfo {
            stream: named.unwrap_or("main"),
            problem: None,
            codec: None,
        }));
        let limits = Arc::new(Mutex::new(limits));
        let thread = std::thread::Builder::new()
            .name(format!("backup-{camera_id}"))
            .spawn({
                let (stop, info, limits) = (stop.clone(), info.clone(), limits.clone());
                move || {
                    capture(
                        &state, &backup, &camera_id, &limits, sound, named, &info, &stop,
                    )
                }
            })
            .map_err(|error| tracing::error!(%error, "Cloud Backup: no capture thread"))
            .ok();
        Self {
            sound,
            named,
            stop,
            thread,
            info,
            limits,
        }
    }

    fn info(&self) -> CaptureInfo {
        self.info.lock().map(|i| i.clone()).unwrap_or_default()
    }

    fn set_limits(&self, limits: Limits) {
        if let Ok(mut current) = self.limits.lock() {
            *current = limits;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn set_info(info: &Mutex<CaptureInfo>, stream: &'static str, problem: Option<String>) {
    if let Ok(mut info) = info.lock() {
        info.stream = stream;
        info.problem = problem;
    }
}

/// Copy a camera's video into a backup track until stopped: the stream the
/// plan names, as the camera sends it (a larger one than bought is still
/// backed up; the portal warns); for older tickets, main while it fits the
/// plan, else sub.
#[allow(clippy::too_many_arguments)]
fn capture(
    state: &RwLock<AppState>,
    backup: &Backup,
    camera_id: &str,
    limits: &Mutex<Limits>,
    sound: bool,
    named: Option<&'static str>,
    info: &Mutex<CaptureInfo>,
    stop: &AtomicBool,
) {
    let mut stream: &'static str = named.unwrap_or("main");
    // A named stream is never swapped for the other one.
    let switchable = named.is_none();
    'connection: while !stop.load(Ordering::Acquire) {
        let subscribed = {
            let state = state.blocking_read();
            let has_sub = state
                .config
                .cameras
                .iter()
                .find(|c| c.id == camera_id)
                .map(|c| c.sub_url.as_deref().is_some_and(|u| !u.trim().is_empty()));
            match has_sub {
                None => Err("This camera is not on this recorder.".to_owned()),
                Some(false) if stream == "sub" => Err(
                    "The plan is for this camera's substream, and the camera has none on this recorder."
                        .to_owned(),
                ),
                Some(has_sub) => state
                    .subscribe_stream(camera_id, stream, SubscriberPolicy::cloud_backup(sound))
                    .map(|subscription| (has_sub, subscription)),
            }
        };
        let (has_sub, subscription) = match subscribed {
            Ok(subscribed) => subscribed,
            Err(problem) => {
                set_info(info, stream, Some(problem));
                if wait(stop, Duration::from_secs(10)) {
                    return;
                }
                continue;
            }
        };
        set_info(info, stream, None);
        let mut track: Option<Track> = None;
        let mut converter: Option<(gstreamer::Caps, Option<AnnexB>)> = None;
        let mut restart = true;
        let mut sound_state = SoundState::None;
        let mut sound_track: Option<(TrackFormat, Track)> = None;
        while !stop.load(Ordering::Acquire) {
            if switchable
                && stream == "main"
                && has_sub
                && track.as_ref().is_some_and(Track::over_plan)
            {
                tracing::info!(
                    camera = camera_id,
                    "Cloud Backup: main stream is over the plan; using the sub stream"
                );
                stream = "sub";
                continue 'connection;
            }
            let Some(delivery) = subscription.recv(Duration::from_millis(100)) else {
                if subscription.state().phase == Phase::Closed {
                    if wait(stop, Duration::from_secs(2)) {
                        return;
                    }
                    continue 'connection;
                }
                continue;
            };
            let (unit, after_gap) = match delivery {
                Delivery::Discontinuity { .. } => {
                    restart = true;
                    // A new connection's sound: a new converter.
                    sound_state = SoundState::None;
                    continue;
                }
                Delivery::Unit { unit, after_gap } => (unit, after_gap),
            };
            if unit.media == Media::Audio {
                if sound {
                    back_up_sound(&mut sound_state, &mut sound_track, backup, camera_id, &unit);
                }
                continue;
            }
            restart |= after_gap;
            let keyframe = unit.keyframe();
            if restart && !keyframe {
                continue;
            }
            // Caps that differ in nothing the units depend on (the GOP
            // cache's and the live units' can) change nothing: a
            // discontinuity would start a new file on the ingest.
            if let Some((caps, Some(old))) = converter.as_mut() {
                if *caps != unit.caps && AnnexB::from_caps(&unit.caps).as_ref() == Ok(&*old) {
                    *caps = unit.caps.clone();
                }
            }
            if converter
                .as_ref()
                .is_none_or(|(caps, _)| *caps != unit.caps)
            {
                let next = match AnnexB::from_caps(&unit.caps) {
                    Ok(next) => next,
                    Err(name) => {
                        set_info(
                            info,
                            stream,
                            Some(format!(
                                "The camera sends {name}; backup takes H.264 or H.265."
                            )),
                        );
                        converter = Some((unit.caps.clone(), None));
                        continue;
                    }
                };
                let plan = limits.lock().map(|l| *l).unwrap_or(Limits::P1080_10);
                if let Err(breach) = plan.check_size(next.width, next.height) {
                    if switchable && stream == "main" && has_sub {
                        stream = "sub";
                        continue 'connection;
                    }
                    let why = plan.explain(&breach);
                    set_info(
                        info,
                        stream,
                        Some(format!("{why} Give the camera a smaller stream.")),
                    );
                    track = None;
                    converter = Some((unit.caps.clone(), None));
                    continue;
                }
                set_info(info, stream, None);
                let format = TrackFormat {
                    codec: match next.codec {
                        VideoCodec::H264 => Codec::H264,
                        VideoCodec::H265 => Codec::H265,
                    },
                    width: Some(next.width),
                    height: Some(next.height),
                    sample_rate: None,
                    channels: None,
                    codec_data: None,
                };
                // A new codec is a new track; a new size is told to the one there.
                let same_codec = converter
                    .as_ref()
                    .and_then(|(_, old)| old.as_ref())
                    .is_some_and(|old| old.codec == next.codec);
                match &track {
                    Some(existing) if same_codec => existing.set_format(format),
                    _ => track = Some(backup.track(camera_id, TrackKind::Video, format)),
                }
                if let Ok(mut info) = info.lock() {
                    info.codec = Some(match next.codec {
                        VideoCodec::H264 => "h264",
                        VideoCodec::H265 => "h265",
                    });
                }
                converter = Some((unit.caps.clone(), Some(next)));
                restart = true;
            }
            let (Some(track), Some((_, Some(annexb)))) = (&track, &converter) else {
                continue;
            };
            let data = match unit.buffer.map_readable() {
                Ok(map) => annexb.convert(&map, keyframe),
                Err(_) => None,
            };
            let Some(data) = data else { continue };
            let mut unit_flags = 0;
            if keyframe {
                unit_flags |= flags::KEYFRAME;
            }
            if restart {
                unit_flags |= flags::DISCONTINUITY;
            }
            track.push(MediaUnit {
                media: MediaKind::Video,
                flags: unit_flags,
                track: 0,
                seq: 0,
                time_us: unit.arrived_us,
                pts_ns: unit.buffer.pts().map(gstreamer::ClockTime::nseconds),
                dts_ns: unit.buffer.dts().map(gstreamer::ClockTime::nseconds),
                data,
            });
            restart = false;
        }
    }
}

/// Where a capture's sound is.
enum SoundState {
    None,
    Ready(gstreamer::Caps, SoundPath),
    /// Sound with these caps cannot be backed up (said once).
    Failed(gstreamer::Caps),
}

/// One sound unit into the camera's sound track (made once its AAC format
/// is known; a new format is a new track).
fn back_up_sound(
    state: &mut SoundState,
    track: &mut Option<(TrackFormat, Track)>,
    backup: &Backup,
    camera_id: &str,
    unit: &crate::streams::SharedUnit,
) {
    let same = match &*state {
        SoundState::Ready(caps, _) | SoundState::Failed(caps) => *caps == unit.caps,
        SoundState::None => false,
    };
    if !same {
        *state = match SoundPath::for_caps(&unit.caps) {
            Ok(path) => SoundState::Ready(unit.caps.clone(), path),
            Err(error) => {
                tracing::warn!(camera = camera_id, %error, "Cloud Backup: this camera's sound cannot be backed up");
                SoundState::Failed(unit.caps.clone())
            }
        };
    }
    let SoundState::Ready(_, path) = state else {
        return;
    };
    let frames = path.push(unit);
    if frames.is_empty() {
        return;
    }
    let Some(format) = path.format() else { return };
    if track.as_ref().is_none_or(|(known, _)| *known != format) {
        *track = Some((
            format.clone(),
            backup.track(camera_id, TrackKind::Audio, format),
        ));
    }
    let Some((_, track)) = track.as_ref() else {
        return;
    };
    for frame in frames {
        track.push(MediaUnit {
            media: MediaKind::Audio,
            flags: flags::KEYFRAME,
            track: 0,
            seq: 0,
            time_us: frame.time_us,
            pts_ns: None,
            dts_ns: None,
            data: frame.data,
        });
    }
}

/// Sleep up to `time`; true when asked to stop meanwhile.
fn wait(stop: &AtomicBool, time: Duration) -> bool {
    let until = Instant::now() + time;
    while Instant::now() < until {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod health_tests {
    use super::*;
    use std::time::Instant;

    fn report(state: &str, camera: CameraReport) -> StatusReport {
        StatusReport {
            state: state.into(),
            cameras: vec![camera],
            ..StatusReport::default()
        }
    }
    fn camera(problem: Option<&str>, behind_ms: i64) -> CameraReport {
        CameraReport {
            camera_id: "cam-1".into(),
            stream: "main".into(),
            problem: problem.map(str::to_owned),
            behind_ms,
            ..CameraReport::default()
        }
    }

    #[test]
    fn a_changed_stream_counts_once_the_change_lasts() {
        let mut watch = ChangeWatch::default();
        let start = Instant::now();
        let more = || {
            vec![(
                "cam-1".to_owned(),
                vec!["4.0 Mbit/s (priced 1.0 Mbit/s)".to_owned()],
            )]
        };
        watch.observe(more(), start);
        assert_eq!(watch.changed("cam-1"), None, "not yet");
        watch.observe(more(), start + CHANGED_AFTER - Duration::from_secs(1));
        assert_eq!(watch.changed("cam-1"), None);
        watch.observe(more(), start + CHANGED_AFTER);
        let shown = watch.changed("cam-1").unwrap();
        assert!(
            shown.contains("4.0 Mbit/s (priced 1.0 Mbit/s)") && shown.contains("still backed up")
        );
        // Back within its price: no longer changed, and the clock starts over.
        watch.observe(
            vec![("cam-1".to_owned(), vec![])],
            start + CHANGED_AFTER * 2,
        );
        assert_eq!(watch.changed("cam-1"), None);
        watch.observe(more(), start + CHANGED_AFTER * 3);
        assert_eq!(watch.changed("cam-1"), None, "a new change waits again");
    }

    #[test]
    fn trouble_fails_only_once_it_lasts() {
        let status = BackupStatus::default();
        let start = Instant::now();
        status.set_at(report("connected", camera(None, 1000)), start);
        let ok = status.report();
        assert!(!ok.cameras[0].failing && ok.cameras[0].trouble.is_none());
        // Disconnected: troubled at once, failing only after FAIL_AFTER.
        status.set_at(report("retrying", camera(None, 1000)), start);
        assert!(!status.report().cameras[0].failing);
        assert!(status.report().cameras[0]
            .trouble
            .as_deref()
            .unwrap()
            .starts_with("Not connected"));
        status.set_at(report("retrying", camera(None, 1000)), start + FAIL_AFTER);
        assert!(status.report().cameras[0].failing);
        // Back: the trouble clock starts over.
        status.set_at(
            report("connected", camera(None, 1000)),
            start + FAIL_AFTER * 2,
        );
        assert!(!status.report().cameras[0].failing);
        status.set_at(report("connected", camera(Some("The plan is for this camera's substream, and the camera has none on this recorder."), 0)), start + FAIL_AFTER * 2);
        status.set_at(
            report("connected", camera(Some("x"), 0)),
            start + FAIL_AFTER * 3,
        );
        assert_eq!(status.report().cameras[0].trouble.as_deref(), Some("x"));
        assert!(
            status.report().cameras[0].failing,
            "a camera problem that lasts"
        );
        // Footage piling up unsent.
        let behind = BackupStatus::default();
        behind.set_at(
            report("connected", camera(None, BEHIND_FAILS_MS + 60_000)),
            start,
        );
        behind.set_at(
            report("connected", camera(None, BEHIND_FAILS_MS + 60_000)),
            start + FAIL_AFTER,
        );
        assert!(behind.report().cameras[0].failing);
        assert!(behind.report().cameras[0]
            .trouble
            .as_deref()
            .unwrap()
            .contains("3 min behind"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_development_ca_is_read_from_pem() {
        let pem = "junk\n-----BEGIN CERTIFICATE-----\nAAEC\nAwQ=\n-----END CERTIFICATE-----\n";
        assert_eq!(pem_der(pem), Some(vec![0, 1, 2, 3, 4]));
        assert_eq!(pem_der("nothing"), None);
    }

    #[test]
    fn status_before_starting_says_so() {
        let status = BackupStatus::default();
        assert_eq!(status.report().state, "off");
        let report = report(&None, Some("No camera has a backup plan."));
        assert_eq!(
            (report.state.as_str(), report.problem.as_deref()),
            ("off", Some("No camera has a backup plan."))
        );
    }
}

#[cfg(test)]
mod run_tests {
    /// `NW_BACKUP_DEV_ASSIGNMENT` is process-wide and `fetch` reads it: the
    /// test that sets it and every test that can reach `fetch` (directly, or
    /// through a running backup client asking `PortalAssigner`) take turns.
    static DEV_ASSIGNMENT_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    use super::*;
    use crate::streams::testing::CameraFile;
    use nw_backup_protocol::ticket::{CameraGrant, Ticket, TICKET_VERSION};
    use nw_core::camera::CameraConfig;

    /// An assigner with nothing to give: the client keeps what it is pushed.
    struct NoPlan;
    impl Assigner for NoPlan {
        fn assign(&self) -> AssignFuture<'_> {
            Box::pin(async { Err("No camera has a backup plan.".to_owned()) })
        }
    }

    fn client_config() -> ClientConfig {
        ClientConfig {
            node_key: ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]),
            recorder: "test".into(),
            tls: nw_backup_client::tls_config(None).unwrap(),
            outage_dir: None,
            memory_bytes_per_track: 1 << 20,
            disk_bytes_per_track: 0,
        }
    }

    fn state_with(cameras: Vec<CameraConfig>) -> Arc<RwLock<AppState>> {
        let config = crate::config::AppConfig {
            cameras,
            ..crate::config::AppConfig::default()
        };
        Arc::new(RwLock::new(AppState::with_database(
            config,
            std::env::temp_dir().join("nw-backup-run-config.json"),
            crate::pipeline::hwaccel::HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )))
    }

    fn grant(camera: &str, stream: Option<&str>, sound: bool, limits: Limits) -> CameraGrant {
        CameraGrant {
            camera_id: camera.into(),
            retention_s: 3600,
            sound,
            limits,
            hot_s: 0,
            move_every_s: 0,
            stream: stream.map(str::to_owned),
            seal_key: None,
            priced: Some(PricedStream {
                width: 160,
                height: 120,
                fps: 5.0,
                bitrate_bps: Some(1_000),
                codec: Some("h265".into()),
                tolerance_percent: 20,
            }),
        }
    }

    fn ticket_for(grants: Vec<CameraGrant>) -> String {
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        ticket::issue(
            &Ticket {
                v: TICKET_VERSION,
                id: "t-1".into(),
                org_id: "org".into(),
                node_id: "node".into(),
                node_key: ticket::hex(key.verifying_key().as_bytes()),
                audience: "ingest.example.com".into(),
                issued_at_ms: 0,
                expires_at_ms: i64::MAX / 2,
                cameras: grants,
            },
            &key,
        )
    }

    fn assignment(ticket: String, ca_pem: Option<&str>) -> BackupAssignment {
        BackupAssignment {
            ticket,
            ingest: super::super::link::backup::BackupIngest {
                name: "ingest.example.com".into(),
                quic: None,
                tcp: Some("127.0.0.1:1".into()),
                ca_pem: ca_pem.map(str::to_owned),
                public_key: None,
            },
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nw-backup-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn until(mut done: impl FnMut() -> bool) {
        for _ in 0..1500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the condition never held");
    }

    async fn drop_off_thread<T: Send + 'static>(value: T) {
        tokio::task::spawn_blocking(move || drop(value))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_capture_copies_a_cameras_video_and_reports_it() {
        let _env = DEV_ASSIGNMENT_ENV.lock().await;
        let fixture = CameraFile::new(250);
        let id = fixture.camera.id.clone();
        let state = state_with(vec![fixture.camera.clone()]);
        let backup = Arc::new(Backup::start(client_config(), Arc::new(NoPlan)));
        let capture = Capture::start(
            state.clone(),
            backup.clone(),
            id.clone(),
            Limits::P1080_10,
            false,
            Some("main"),
        );
        until(|| {
            backup
                .status()
                .tracks
                .iter()
                .any(|t| t.kind == TrackKind::Video && t.unconfirmed_units > 0)
        })
        .await;
        until(|| capture.info().codec.is_some()).await;
        assert_eq!(capture.info().codec, Some("h264"));
        assert_eq!(capture.info().problem, None);
        capture.set_limits(Limits::P1080_10);

        let node_dir = temp_dir("node");
        let node = CloudNode::open(node_dir.join("node")).unwrap();
        let mut captures = HashMap::new();
        captures.insert(id.clone(), capture);
        let mut priced = HashMap::new();
        priced.insert(
            id.clone(),
            grant(&id, None, false, Limits::P1080_10).priced.unwrap(),
        );
        let mut running = Some(Running {
            backup: backup.clone(),
            assigner: Arc::new(PortalAssigner {
                node: node.shared(),
                cached: Mutex::new(None),
            }),
            captures,
            priced,
            changes: ChangeWatch::default(),
        });

        // The stream is H.264, the plan was priced for H.265: a change.
        let seen = seen_changes(&state, running.as_ref().unwrap()).await;
        assert_eq!(seen.len(), 1);
        assert!(seen[0].1.iter().any(|c| c.contains("H.264")), "{seen:?}");
        let now = Instant::now();
        let later = now + CHANGED_AFTER;
        {
            let running = running.as_mut().unwrap();
            running.changes.observe(seen.clone(), now);
            running.changes.observe(seen, later);
        }
        let shown = report(&running, None);
        assert_eq!(shown.cameras.len(), 1);
        let camera = &shown.cameras[0];
        assert_eq!(
            (camera.camera_id.as_str(), camera.stream.as_str()),
            (id.as_str(), "main")
        );
        assert!(camera.unconfirmed_units > 0 && camera.problem.is_none());
        assert!(camera
            .stream_changed
            .as_deref()
            .unwrap()
            .contains("still backed up"));
        assert_ne!(shown.state, "off");
        let with_problem = report(&running, Some("paused"));
        assert_eq!(with_problem.problem.as_deref(), Some("paused"));

        stop_running(&mut running).await;
        assert!(running.is_none());
        assert_eq!(report(&running, Some("gone")).state, "off");
        // Stopping what is not running is nothing.
        stop_running(&mut running).await;
        let _ = std::fs::remove_dir_all(node_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_capture_says_why_a_camera_cannot_be_backed_up() {
        let _env = DEV_ASSIGNMENT_ENV.lock().await;
        let fixture = CameraFile::new(250);
        let id = fixture.camera.id.clone();
        let state = state_with(vec![fixture.camera.clone()]);
        let backup = Arc::new(Backup::start(client_config(), Arc::new(NoPlan)));
        let start = |camera: &str, limits: Limits, named: Option<&'static str>| {
            Capture::start(
                state.clone(),
                backup.clone(),
                camera.to_owned(),
                limits,
                false,
                named,
            )
        };
        let problem_of = |capture: &Capture| capture.info().problem;

        let missing = start("not-here", Limits::P1080_10, None);
        until(|| problem_of(&missing).is_some()).await;
        assert_eq!(
            problem_of(&missing).as_deref(),
            Some("This camera is not on this recorder.")
        );

        // The plan is for a substream the camera does not have.
        let no_sub = start(&id, Limits::P1080_10, Some("sub"));
        until(|| problem_of(&no_sub).is_some()).await;
        assert!(problem_of(&no_sub).unwrap().contains("substream"));
        assert_eq!(no_sub.info().stream, "sub");

        // The named main stream is larger than the plan: said, not swapped.
        let small = Limits {
            max_long_side: 200,
            max_short_side: 100,
            ..Limits::P1080_10
        };
        let too_big = start(&id, small, Some("main"));
        until(|| problem_of(&too_big).is_some()).await;
        let why = problem_of(&too_big).unwrap();
        assert!(
            why.contains("320×240") && why.ends_with("Give the camera a smaller stream."),
            "{why}"
        );
        assert_eq!(too_big.info().stream, "main");

        // An older ticket names no stream: main is too large, so the sub is tried.
        {
            let mut app = state.write().await;
            app.config.cameras[0].sub_url = Some(fixture.camera.url.clone());
        }
        let switching = start(&id, small, None);
        until(|| switching.info().stream == "sub" && problem_of(&switching).is_some()).await;
        assert!(problem_of(&switching).unwrap().contains("Give the camera"));

        for capture in [missing, no_sub, too_big, switching] {
            drop_off_thread(capture).await;
        }
        backup.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_plans_cameras_start_change_and_stop_with_each_ticket() {
        let _env = DEV_ASSIGNMENT_ENV.lock().await;
        let dir = temp_dir("apply");
        let node = CloudNode::open(dir.join("node")).unwrap();
        let source = BackupSource {
            state: state_with(vec![]),
            outage_dir: dir.join("outage"),
            status: Arc::new(BackupStatus::default()),
        };
        let mut running: Option<Running> = None;
        let main = Limits::P1080_10;

        // A ticket nobody can read, and a development CA that is not one.
        assert_eq!(
            apply(
                &node,
                &source,
                &mut running,
                assignment("junk".into(), None)
            )
            .unwrap_err(),
            "The backup ticket could not be read."
        );
        let one = ticket_for(vec![grant("cam-a", Some("main"), false, main)]);
        assert_eq!(
            apply(
                &node,
                &source,
                &mut running,
                assignment(one.clone(), Some("not pem"))
            )
            .unwrap_err(),
            "The backup server's development CA is not PEM."
        );
        let bad_ca = "-----BEGIN CERTIFICATE-----\nAAEC\n-----END CERTIFICATE-----\n";
        assert!(apply(
            &node,
            &source,
            &mut running,
            assignment(one.clone(), Some(bad_ca))
        )
        .is_err());
        assert!(running.is_none());

        // Camera A starts.
        apply(&node, &source, &mut running, assignment(one, None)).unwrap();
        {
            let running = running.as_ref().unwrap();
            assert_eq!(running.captures.len(), 1);
            assert!(running.priced.contains_key("cam-a"));
        }
        // A ticket just fetched goes to the client once, then the portal is asked.
        let assigner = PortalAssigner {
            node: node.shared(),
            cached: Mutex::new(None),
        };
        assigner.remember(assignment(ticket_for(vec![]), None));
        let given = assigner.assign().await.unwrap();
        assert_eq!(
            (given.server_name.as_str(), given.tcp.as_deref()),
            ("ingest.example.com", Some("127.0.0.1:1"))
        );
        assert!(
            assigner.assign().await.is_err(),
            "nothing cached, nothing linked"
        );

        // A new limit for A, B joins: A keeps its capture.
        let roomy = Limits {
            max_bitrate_bps: 9_000_000,
            ..main
        };
        let two = ticket_for(vec![
            grant("cam-a", Some("main"), false, roomy),
            grant("cam-b", None, false, main),
        ]);
        apply(&node, &source, &mut running, assignment(two, None)).unwrap();
        assert_eq!(running.as_ref().unwrap().captures.len(), 2);
        assert_eq!(
            *running.as_ref().unwrap().captures["cam-a"]
                .limits
                .lock()
                .unwrap(),
            roomy
        );

        // Sound switched on for A restarts it; B left the plan and stops.
        let three = ticket_for(vec![grant("cam-a", Some("main"), true, roomy)]);
        apply(&node, &source, &mut running, assignment(three, None)).unwrap();
        {
            let running = running.as_ref().unwrap();
            assert_eq!(running.captures.len(), 1);
            assert!(running.captures["cam-a"].sound);
        }
        // The capture's thread says why on its own time.
        until(|| report(&running, None).cameras[0].problem.is_some()).await;
        let shown = report(&running, None);
        assert_eq!(shown.cameras[0].camera_id, "cam-a");
        assert_eq!(
            shown.cameras[0].problem.as_deref(),
            Some("This camera is not on this recorder.")
        );
        stop_running(&mut running).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_plan_names_main_or_sub_or_neither() {
        let main = Limits::P1080_10;
        assert_eq!(named(&grant("c", Some("main"), false, main)), Some("main"));
        assert_eq!(named(&grant("c", Some("sub"), false, main)), Some("sub"));
        assert_eq!(named(&grant("c", Some("third"), false, main)), None);
        assert_eq!(named(&grant("c", None, false, main)), None);
    }

    #[test]
    fn waiting_ends_early_when_asked_to_stop() {
        let stop = AtomicBool::new(true);
        assert!(wait(&stop, Duration::from_secs(30)));
        let go = AtomicBool::new(false);
        assert!(!wait(&go, Duration::from_millis(1)));
    }

    #[test]
    fn the_outage_folder_is_private_to_the_test_process() {
        let dir = default_outage_dir();
        assert!(dir.starts_with(std::env::temp_dir()));
        assert!(dir
            .to_string_lossy()
            .contains(&std::process::id().to_string()));
    }

    fn sound_unit(caps: &gstreamer::Caps, i: u64) -> crate::streams::SharedUnit {
        let mut buffer = gstreamer::Buffer::from_mut_slice(vec![0x7fu8; 160]);
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gstreamer::ClockTime::from_mseconds(i * 20));
        crate::streams::SharedUnit {
            media: Media::Audio,
            buffer,
            caps: caps.clone(),
            arrived_us: 1_000_000_000 + i as i64 * 20_000,
            frame: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sound_is_converted_into_a_track_and_unusable_sound_is_said_once() {
        let _env = DEV_ASSIGNMENT_ENV.lock().await;
        gstreamer::init().unwrap();
        let backup = Backup::start(client_config(), Arc::new(NoPlan));
        let mut state = SoundState::None;
        let mut track = None;

        // Sound that cannot be backed up: failed once, nothing sent.
        let odd = gstreamer::Caps::builder("audio/x-unknown").build();
        back_up_sound(&mut state, &mut track, &backup, "cam", &sound_unit(&odd, 0));
        assert!(matches!(&state, SoundState::Failed(caps) if *caps == odd));
        back_up_sound(&mut state, &mut track, &backup, "cam", &sound_unit(&odd, 1));
        assert!(track.is_none());

        let encoder = ["voaacenc", "fdkaacenc", "avenc_aac", "faac"]
            .iter()
            .any(|name| gstreamer::ElementFactory::find(name).is_some());
        if !encoder || gstreamer::ElementFactory::find("alawdec").is_none() {
            eprintln!("no AAC encoder or G.711 decoder here: skipped");
            backup.stop().await;
            return;
        }
        let alaw = gstreamer::Caps::builder("audio/x-alaw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build();
        for i in 0..150 {
            back_up_sound(
                &mut state,
                &mut track,
                &backup,
                "cam",
                &sound_unit(&alaw, i),
            );
            if track.is_some() {
                break;
            }
        }
        assert!(matches!(&state, SoundState::Ready(caps, _) if *caps == alaw));
        // The encoder may need a moment before its first frame.
        for i in 150..400 {
            if track.is_some() {
                break;
            }
            back_up_sound(
                &mut state,
                &mut track,
                &backup,
                "cam",
                &sound_unit(&alaw, i),
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if track.is_some() {
            assert!(backup
                .status()
                .tracks
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.camera_id == "cam"));
        }
        drop(track);
        backup.stop().await;
    }

    /// `NW_BACKUP_DEV_ASSIGNMENT` is process-wide: everything that reads it
    /// (the dev file, the plan check loop) is in this one test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_plan_check_follows_the_development_assignment_file() {
        let _env = DEV_ASSIGNMENT_ENV.lock().await;
        let dir = temp_dir("loop");
        let file = dir.join("assignment.json");
        let node = CloudNode::open(dir.join("node")).unwrap();

        // Unlinked and without a file: asking the portal fails, plainly.
        assert!(fetch(&node).await.is_err());

        std::env::set_var("NW_BACKUP_DEV_ASSIGNMENT", &file);
        // Missing file, then a file that is not JSON, then no plan.
        let missing = fetch(&node).await.unwrap_err();
        assert!(missing.contains("assignment.json"), "{missing}");
        std::fs::write(&file, "not json").unwrap();
        assert!(fetch(&node).await.is_err());
        std::fs::write(&file, r#"{"backup": null}"#).unwrap();
        assert_eq!(fetch(&node).await, Ok(None));
        let plan = serde_json::json!({"backup": {
            "ticket": ticket_for(vec![grant("cam-x", Some("main"), false, Limits::P1080_10)]),
            "ingest": {"name": "ingest.example.com", "tcp": "127.0.0.1:1"}
        }});
        std::fs::write(&file, plan.to_string()).unwrap();
        assert!(fetch(&node).await.unwrap().is_some());

        let run = |file_text: Option<String>| {
            let dir = dir.clone();
            let node = node.shared();
            async move {
                if let Some(text) = file_text {
                    std::fs::write(dir.join("assignment.json"), text).unwrap();
                }
                let status = Arc::new(BackupStatus::default());
                let source = BackupSource {
                    state: state_with(vec![]),
                    outage_dir: dir.join("outage"),
                    status: status.clone(),
                };
                let (stop, stopped) = watch::channel(false);
                let task = tokio::spawn(run_loop(node, source, stopped));
                (status, stop, task)
            }
        };
        let finish = |stop: watch::Sender<bool>, task: tokio::task::JoinHandle<()>| async move {
            stop.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("the loop stops")
                .unwrap();
        };

        // No camera has a plan.
        let (status, stop, task) = run(Some(r#"{"backup": null}"#.into())).await;
        until(|| status.report().problem.as_deref() == Some("No camera has a backup plan.")).await;
        assert_eq!(status.report().state, "off");
        finish(stop, task).await;

        // A ticket that cannot be read.
        let junk = serde_json::json!({"backup": {
            "ticket": "junk", "ingest": {"name": "ingest.example.com", "tcp": "127.0.0.1:1"}
        }});
        let (status, stop, task) = run(Some(junk.to_string())).await;
        until(|| {
            status.report().problem.as_deref() == Some("The backup ticket could not be read.")
        })
        .await;
        finish(stop, task).await;

        // A plan for a camera this recorder lacks: running, with the camera's problem.
        let (status, stop, task) = run(Some(plan.to_string())).await;
        until(|| {
            let report = status.report();
            report.state != "off" && report.cameras.iter().any(|c| c.problem.is_some())
        })
        .await;
        let report = status.report();
        assert_eq!(report.cameras[0].camera_id, "cam-x");
        assert!(report.cameras[0].trouble.is_some());
        finish(stop, task).await;

        // A failed check keeps the loop alive without a report of its own.
        let (status, stop, task) = run(Some("not json".into())).await;
        until(|| {
            let report = status.report();
            report.state == "off" && report.problem.is_none()
        })
        .await;
        finish(stop, task).await;

        std::env::remove_var("NW_BACKUP_DEV_ASSIGNMENT");
        let _ = std::fs::remove_dir_all(dir);
    }
}
