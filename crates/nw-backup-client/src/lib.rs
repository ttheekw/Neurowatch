//! The recorder's side of NeuroWatch Cloud Backup.
//!
//! The backend registers a [`Track`] per backed-up camera track and pushes
//! its compressed units as they arrive; this crate numbers them, keeps them
//! until the ingest confirms them ([`outbox`]), streams them over QUIC (or
//! TLS over TCP where UDP is blocked), reconnects with live units first and
//! the backlog after, renews the portal's ticket and reports [`Status`].
//! Design: `docs/plans/2026-11-07-cloud-backup.md`.
pub mod outbox;
mod session;
pub mod transport;

use ed25519_dalek::SigningKey;
use nw_backup_protocol::{
    frame::{MediaKind, MediaUnit},
    messages::{TrackFormat, TrackKind, TrackRefusal},
};
use outbox::{Budget, Outbox};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
};

pub use transport::tls_config;

/// Where to back up, from the portal: a ticket and the ingest it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub ticket: String,
    /// The ingest's TLS name (its certificate's).
    pub server_name: String,
    /// `host:port` for QUIC, and for TLS over TCP.
    pub quic: Option<String>,
    pub tcp: Option<String>,
}

pub type AssignFuture<'a> = Pin<Box<dyn Future<Output = Result<Assignment, String>> + Send + 'a>>;

/// Asks the portal for an [`Assignment`] (a fresh ticket each time).
pub trait Assigner: Send + Sync + 'static {
    fn assign(&self) -> AssignFuture<'_>;
}

pub struct Config {
    /// The recorder's cloud identity key (tickets name its public half).
    pub node_key: SigningKey,
    /// The recorder's version, for the ingest's logs.
    pub recorder: String,
    pub tls: Arc<rustls::ClientConfig>,
    /// The outage folder; `None` keeps unconfirmed units in memory only.
    pub outage_dir: Option<PathBuf>,
    pub memory_bytes_per_track: usize,
    pub disk_bytes_per_track: u64,
}

/// How the backup is doing, for the app and the portal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// "connecting", "connected" or "retrying".
    pub state: &'static str,
    pub ingest: Option<String>,
    /// "quic" or "tcp".
    pub transport: Option<&'static str>,
    /// Why it is not connected, in plain words.
    pub problem: Option<String>,
    pub tracks: Vec<TrackStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackStatus {
    pub camera_id: String,
    pub kind: TrackKind,
    /// Why the ingest does not take this track (over the plan, no plan…).
    pub refused: Option<String>,
    pub unconfirmed_units: usize,
    pub memory_bytes: usize,
    pub disk_bytes: u64,
    /// How far the confirmed footage is behind now (0 when nothing waits).
    pub behind_ms: i64,
}

pub(crate) struct TrackState {
    pub number: u32,
    pub camera_id: String,
    pub kind: TrackKind,
    pub format: Mutex<TrackFormat>,
    pub outbox: Mutex<Outbox>,
    /// Bumped on every push: wakes the senders.
    pub pushed: watch::Sender<u64>,
    /// Why the ingest does not take this track (no code: no plan here).
    pub refused: Mutex<Option<(Option<TrackRefusal>, String)>>,
    /// Left in the outage folder by a previous run: sent, then forgotten.
    pub recovered: bool,
}

/// A track's description in the outage folder (`<epoch>.track.json`).
#[derive(Serialize, serde::Deserialize)]
struct Description {
    camera_id: String,
    kind: TrackKind,
    format: TrackFormat,
}

pub(crate) struct Shared {
    pub config: Config,
    pub tracks: Mutex<BTreeMap<u32, Arc<TrackState>>>,
    pub tracks_changed: Notify,
    pub status: Mutex<Status>,
    pub stopping: watch::Sender<bool>,
    next_track: AtomicU32,
}

pub struct Backup {
    shared: Arc<Shared>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Backup {
    /// Start backing up (inside a Tokio runtime). Tracks are added with
    /// [`Backup::track`].
    pub fn start(config: Config, assigner: Arc<dyn Assigner>) -> Self {
        let (stopping, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            config,
            tracks: Mutex::new(BTreeMap::new()),
            tracks_changed: Notify::new(),
            status: Mutex::new(Status {
                state: "connecting",
                ingest: None,
                transport: None,
                problem: None,
                tracks: Vec::new(),
            }),
            stopping,
            next_track: AtomicU32::new(1),
        });
        recover(&shared);
        let task = tokio::spawn(session::run(shared.clone(), assigner));
        Self {
            shared,
            task: Mutex::new(Some(task)),
        }
    }

    /// Back up a camera's video or sound track (a new epoch: numbering
    /// starts again). Dropping the [`Track`] stops it.
    pub fn track(&self, camera_id: &str, kind: TrackKind, format: TrackFormat) -> Track {
        let number = self.shared.next_track.fetch_add(1, Ordering::Relaxed);
        let media = if kind == TrackKind::Audio {
            MediaKind::Audio
        } else {
            MediaKind::Video
        };
        let budget = Budget {
            memory_bytes: self.shared.config.memory_bytes_per_track,
            disk_bytes: self.shared.config.disk_bytes_per_track,
            dir: self.shared.config.outage_dir.clone(),
        };
        let state = Arc::new(TrackState {
            number,
            camera_id: camera_id.to_owned(),
            kind,
            format: Mutex::new(format),
            outbox: Mutex::new(Outbox::new(media, random_epoch(), budget)),
            pushed: watch::channel(0).0,
            refused: Mutex::new(None),
            recovered: false,
        });
        if let Some(dir) = &self.shared.config.outage_dir {
            let epoch = lock(&state.outbox).epoch;
            let description = Description {
                camera_id: camera_id.to_owned(),
                kind,
                format: lock(&state.format).clone(),
            };
            let written = std::fs::create_dir_all(dir).and_then(|()| {
                std::fs::write(
                    dir.join(outbox::description_name(epoch)),
                    serde_json::to_vec(&description).unwrap_or_default(),
                )
            });
            if let Err(error) = written {
                tracing::warn!(%error, "cloud backup: the outage folder cannot be written");
            }
        }
        lock(&self.shared.tracks).insert(number, state.clone());
        self.shared.tracks_changed.notify_one();
        Track {
            state,
            shared: self.shared.clone(),
        }
    }

    pub fn status(&self) -> Status {
        let mut status = lock(&self.shared.status).clone();
        let now = now_us();
        status.tracks = lock(&self.shared.tracks)
            .values()
            .map(|track| {
                let counts = lock(&track.outbox).counts();
                TrackStatus {
                    camera_id: track.camera_id.clone(),
                    kind: track.kind,
                    refused: lock(&track.refused).as_ref().map(|(_, why)| why.clone()),
                    unconfirmed_units: counts.units,
                    memory_bytes: counts.memory_bytes,
                    disk_bytes: counts.disk_bytes,
                    behind_ms: counts
                        .oldest_us
                        .map_or(0, |oldest| ((now - oldest) / 1000).max(0)),
                }
            })
            .collect();
        status
    }

    /// Say goodbye to the ingest and stop. What is unconfirmed stays in the
    /// outage folder; the next start sends it.
    pub async fn stop(&self) {
        let _ = self.shared.stopping.send(true);
        let task = lock(&self.task).take();
        if let Some(task) = task {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        for track in lock(&self.shared.tracks).values() {
            lock(&track.outbox).persist();
        }
    }
}

/// One backed-up track. Push its units as they arrive.
pub struct Track {
    state: Arc<TrackState>,
    shared: Arc<Shared>,
}

impl Track {
    /// One unit as the camera sent it (video: Annex-B with parameter sets
    /// before keyframes). Its number and track are set here. Never blocks on
    /// the network: call it from the thread that receives the camera's units.
    pub fn push(&self, mut unit: MediaUnit) {
        unit.track = self.state.number;
        let seq = lock(&self.state.outbox).push(unit);
        self.state.pushed.send_replace(seq);
    }

    /// The track's format changed (e.g. a new camera resolution).
    pub fn set_format(&self, format: TrackFormat) {
        *lock(&self.state.format) = format;
    }

    pub fn camera_id(&self) -> &str {
        &self.state.camera_id
    }

    /// Why the ingest does not take this track, if it does not.
    pub fn refusal(&self) -> Option<String> {
        lock(&self.state.refused)
            .as_ref()
            .map(|(_, why)| why.clone())
    }

    /// Refused for being over its plan's limits (a smaller stream may fit).
    pub fn over_plan(&self) -> bool {
        matches!(
            lock(&self.state.refused).as_ref(),
            Some((
                Some(
                    TrackRefusal::OverResolution
                        | TrackRefusal::OverFrameRate
                        | TrackRefusal::OverBitrate
                ),
                _
            ))
        )
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        lock(&self.shared.tracks).remove(&self.state.number);
        self.shared.tracks_changed.notify_one();
    }
}

/// Tracks a previous run left in the outage folder, as backlog.
fn recover(shared: &Shared) {
    let Some(dir) = shared.config.outage_dir.clone() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(epoch) = name
            .strip_suffix(".track.json")
            .and_then(|e| u64::from_str_radix(e, 16).ok())
        else {
            continue;
        };
        let description: Option<Description> = std::fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let Some(description) = description else {
            continue;
        };
        let media = if description.kind == TrackKind::Audio {
            MediaKind::Audio
        } else {
            MediaKind::Video
        };
        let budget = Budget {
            memory_bytes: shared.config.memory_bytes_per_track,
            disk_bytes: shared.config.disk_bytes_per_track,
            dir: Some(dir.clone()),
        };
        let mut outbox = match Outbox::recover(media, epoch, budget) {
            Ok(outbox) => outbox,
            Err(error) => {
                tracing::warn!(%error, "cloud backup: an outage file could not be read");
                continue;
            }
        };
        if outbox.counts().units == 0 {
            outbox.discard();
            continue;
        }
        tracing::info!(camera = %description.camera_id, units = outbox.counts().units, "cloud backup: sending what the last run left");
        let number = shared.next_track.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(TrackState {
            number,
            camera_id: description.camera_id,
            kind: description.kind,
            format: Mutex::new(description.format),
            outbox: Mutex::new(outbox),
            pushed: watch::channel(0).0,
            refused: Mutex::new(None),
            recovered: true,
        });
        lock(&shared.tracks).insert(number, state);
    }
}

fn random_epoch() -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        return now_us() as u64;
    }
    u64::from_le_bytes(bytes)
}

pub(crate) fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
