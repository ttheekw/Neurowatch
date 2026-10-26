//! System-health events: a camera offline, a recording that failed, a
//! recording drive running out of space or missing, the backend starting.
//!
//! The tray used to be the only place that noticed these, and only while a
//! desktop session ran it; as events they reach the event list, alerts and
//! headless nodes too. [`HealthTracker`] turns periodic snapshots into event
//! starts and ends (pure, unit-tested); [`run`] feeds it from the backend.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nw_core::camera::CameraId;
use nw_core::event::{EventKind, EventSource};
use tokio::sync::{broadcast, RwLock};

use super::{EventHub, NewEvent};
use crate::app_state::{expand_user_path, AppState};

const POLL: Duration = Duration::from_secs(5);
/// Open events are marked as still going on this often.
const TOUCH_EVERY: u32 = 12;
/// A recording must stay failed this many polls before it is an event:
/// a recording that restarts at once is not worth a notification.
const FAILED_POLLS: u32 = 2;
/// Below this much free space (or above [`DISK_FULL_PERCENT`]) recording
/// is about to fail.
const DISK_LOW_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DISK_FULL_PERCENT: u64 = 98;
/// …and it is fine again above these (no flapping around the line).
const DISK_OK_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const DISK_OK_PERCENT: u64 = 97;

/// One poll: each camera's ingest state and recording status, and each
/// recording drive's `(total, available)` bytes.
#[derive(Debug, Clone, Default)]
pub struct HealthSnapshot {
    pub cameras: Vec<CameraHealth>,
    pub drives: Vec<(PathBuf, Option<(u64, u64)>)>,
    /// Recording locations in use that are missing, with why
    /// (`crate::storage_guard`).
    pub missing_storage: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone)]
pub struct CameraHealth {
    pub id: CameraId,
    pub name: String,
    /// Ingest state (`camera.status_all`).
    pub state: String,
    /// `recording.status_all`: `recording`, `starting`, `failed`, `idle`.
    pub recording: Option<String>,
    /// Its recording location is missing (that event explains its failure).
    pub storage_missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Offline(CameraId),
    RecordingFailed(CameraId),
    Disk(PathBuf),
    Storage(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub enum HealthChange {
    Start(NewEventSpec),
    End(i64),
}

/// What to start; the tracker learns its id through [`HealthTracker::started`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewEventSpec {
    key: Key,
    pub camera_id: Option<CameraId>,
    pub kind: EventKind,
    pub detail: Option<String>,
}

#[derive(Default)]
pub struct HealthTracker {
    open: HashMap<Key, i64>,
    failing: HashMap<CameraId, u32>,
}

impl HealthTracker {
    pub fn observe(&mut self, snapshot: &HealthSnapshot) -> Vec<HealthChange> {
        let mut wanted: HashMap<Key, NewEventSpec> = HashMap::new();
        let mut known: HashSet<Key> = HashSet::new();
        for camera in &snapshot.cameras {
            let offline_key = Key::Offline(camera.id.clone());
            let failed_key = Key::RecordingFailed(camera.id.clone());
            known.insert(offline_key.clone());
            known.insert(failed_key.clone());
            // A camera refusing its password is as unreachable as an offline one.
            let offline = matches!(
                camera.state.as_str(),
                "offline" | crate::pipeline::supervisor::UNAUTHORIZED
            );
            let online = matches!(camera.state.as_str(), "streaming" | "recording_ready");
            if offline || (self.open.contains_key(&offline_key) && !online) {
                wanted.insert(
                    offline_key.clone(),
                    spec(
                        offline_key,
                        Some(&camera.id),
                        EventKind::CameraOffline,
                        None,
                    ),
                );
            }
            let failed = camera.recording.as_deref() == Some("failed");
            let polls = if failed {
                let polls = self.failing.entry(camera.id.clone()).or_default();
                *polls = polls.saturating_add(1);
                *polls
            } else {
                self.failing.remove(&camera.id);
                0
            };
            // An offline camera, or a missing recording location, explains
            // its failed recording: one event.
            if !offline && !camera.storage_missing && polls >= FAILED_POLLS {
                wanted.insert(
                    failed_key.clone(),
                    spec(
                        failed_key,
                        Some(&camera.id),
                        EventKind::RecordingFailed,
                        None,
                    ),
                );
            }
        }
        self.failing
            .retain(|id, _| snapshot.cameras.iter().any(|c| &c.id == id));
        for (root, space) in &snapshot.drives {
            let key = Key::Disk(root.clone());
            known.insert(key.clone());
            let Some((total, available)) = *space else {
                continue;
            };
            let used_percent = total.saturating_sub(available).saturating_mul(100) / total.max(1);
            let low = available < DISK_LOW_BYTES || used_percent >= DISK_FULL_PERCENT;
            let recovered = available > DISK_OK_BYTES && used_percent < DISK_OK_PERCENT;
            if low || (self.open.contains_key(&key) && !recovered) {
                let detail = format!("{} free on {}", human_bytes(available), root.display());
                wanted.insert(
                    key.clone(),
                    spec(key, None, EventKind::DiskLow, Some(detail)),
                );
            }
        }
        for (root, reason) in &snapshot.missing_storage {
            let key = Key::Storage(root.clone());
            known.insert(key.clone());
            wanted.insert(
                key.clone(),
                spec(key, None, EventKind::StorageMissing, Some(reason.clone())),
            );
        }
        // A location that is back (or no longer used) ends its event.
        for key in self.open.keys() {
            if matches!(key, Key::Storage(_)) {
                known.insert(key.clone());
            }
        }
        let mut changes: Vec<HealthChange> = Vec::new();
        let ended: Vec<Key> = self
            .open
            .keys()
            .filter(|key| !wanted.contains_key(*key) || !known.contains(*key))
            .cloned()
            .collect();
        for key in ended {
            if let Some(id) = self.open.remove(&key) {
                changes.push(HealthChange::End(id));
            }
        }
        let mut starts: Vec<NewEventSpec> = wanted
            .into_iter()
            .filter(|(key, _)| !self.open.contains_key(key))
            .map(|(_, spec)| spec)
            .collect();
        starts.sort_by(|a, b| format!("{:?}", a.key).cmp(&format!("{:?}", b.key)));
        changes.extend(starts.into_iter().map(HealthChange::Start));
        changes
    }

    /// The hub numbered an event this tracker asked to start.
    pub fn started(&mut self, spec: &NewEventSpec, id: i64) {
        self.open.insert(spec.key.clone(), id);
    }
}

fn spec(
    key: Key,
    camera: Option<&CameraId>,
    kind: EventKind,
    detail: Option<String>,
) -> NewEventSpec {
    NewEventSpec {
        key,
        camera_id: camera.cloned(),
        kind,
        detail,
    }
}

fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    format!("{:.1} GB", bytes as f64 / GIB)
}

async fn snapshot(state: &Arc<RwLock<AppState>>) -> HealthSnapshot {
    let (cameras, roots, missing_storage) = {
        let st = state.read().await;
        let states = st.supervisor_states();
        let recording = st.recording.status_all_with_pipeline_states(&states);
        let cameras = st
            .config
            .cameras
            .iter()
            .map(|cam| CameraHealth {
                id: cam.id.clone(),
                name: cam.name.clone(),
                state: states.get(&cam.id).cloned().unwrap_or_default(),
                recording: if st.storage_blocked(&cam.id).is_some() {
                    Some("failed".to_owned())
                } else {
                    recording.get(&cam.id).cloned()
                },
                storage_missing: st.storage_blocked(&cam.id).is_some(),
            })
            .collect::<Vec<_>>();
        // Drives something records to.
        let mut roots: Vec<PathBuf> = Vec::new();
        for cam in &st.config.cameras {
            if let Some(path) = st.recording.recording_storage_path(&cam.id) {
                let root = cam
                    .recording
                    .local
                    .storage_path
                    .as_deref()
                    .map(expand_user_path)
                    .filter(|root| path.starts_with(root))
                    .unwrap_or_else(|| expand_user_path(&st.config.recording.storage_path));
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        let in_use: Vec<PathBuf> = crate::storage_guard::locations(&st)
            .into_iter()
            .map(|(root, _)| root)
            .collect();
        let missing_storage: Vec<(PathBuf, String)> = st
            .storage
            .missing()
            .into_iter()
            .filter(|(root, _)| in_use.contains(root))
            .collect();
        roots.retain(|root| st.storage.is_ready(root));
        (cameras, roots, missing_storage)
    };
    let drives = tokio::task::spawn_blocking(move || {
        roots
            .into_iter()
            .map(|root| {
                let space = crate::storage_drives::space(&root.to_string_lossy());
                (root, space)
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    HealthSnapshot {
        cameras,
        drives,
        missing_storage,
    }
}

/// Report the backend's start, then watch cameras, recordings and drives
/// until shutdown.
pub async fn run(
    state: Arc<RwLock<AppState>>,
    hub: Arc<EventHub>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut detail = format!("NeuroWatch {}", env!("CARGO_PKG_VERSION"));
    if let Some((reason, _restarts)) = crate::supervise::previous_exit() {
        detail.push_str(&format!(
            "; started again after the backend stopped unexpectedly: {reason}"
        ));
    }
    hub.instant(
        NewEvent::new(None, EventKind::BackendStarted, EventSource::System).detail(detail),
        super::now_ms(),
    );
    let mut tracker = HealthTracker::default();
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Cameras need a moment to connect before "offline" means anything;
    // the ingest only reports offline after several failed attempts anyway.
    tick.tick().await;
    let mut polls: u32 = 0;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.recv() => return,
        }
        let snapshot = snapshot(&state).await;
        for change in tracker.observe(&snapshot) {
            match change {
                HealthChange::End(id) => {
                    hub.end(id);
                }
                HealthChange::Start(spec) => {
                    let mut event =
                        NewEvent::new(spec.camera_id.clone(), spec.kind, EventSource::System);
                    event.detail = spec.detail.clone().or_else(|| {
                        let id = spec.camera_id.as_ref()?;
                        let camera = snapshot.cameras.iter().find(|c| &c.id == id)?;
                        Some(camera.name.clone())
                    });
                    let record = hub.start(event);
                    tracker.started(&spec, record.id);
                }
            }
        }
        polls = polls.wrapping_add(1);
        if polls % TOUCH_EVERY == 0 {
            hub.touch_open();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(id: &str, state: &str, recording: Option<&str>) -> CameraHealth {
        CameraHealth {
            id: id.into(),
            name: format!("Camera {id}"),
            state: state.into(),
            recording: recording.map(str::to_owned),
            storage_missing: false,
        }
    }

    fn observe(tracker: &mut HealthTracker, snapshot: HealthSnapshot) -> Vec<(String, EventKind)> {
        let mut out = Vec::new();
        let mut next_id = 100 + tracker.open.len() as i64 * 10;
        for change in tracker.observe(&snapshot) {
            match change {
                HealthChange::Start(spec) => {
                    next_id += 1;
                    tracker.started(&spec, next_id);
                    out.push(("start".into(), spec.kind));
                }
                HealthChange::End(_) => out.push(("end".into(), EventKind::Motion)),
            }
        }
        out
    }

    fn cams(cameras: Vec<CameraHealth>) -> HealthSnapshot {
        HealthSnapshot {
            cameras,
            drives: vec![],
            missing_storage: vec![],
        }
    }

    #[test]
    fn offline_starts_once_and_ends_when_the_camera_streams_again() {
        let mut t = HealthTracker::default();
        assert!(observe(&mut t, cams(vec![camera("a", "connecting", None)])).is_empty());
        assert_eq!(
            observe(&mut t, cams(vec![camera("a", "offline", None)])),
            [("start".to_owned(), EventKind::CameraOffline)]
        );
        assert!(observe(&mut t, cams(vec![camera("a", "offline", None)])).is_empty());
        // Still not back while reconnecting.
        assert!(observe(&mut t, cams(vec![camera("a", "reconnecting", None)])).is_empty());
        assert_eq!(
            observe(&mut t, cams(vec![camera("a", "streaming", None)])).len(),
            1
        );
        assert!(t.open.is_empty());
    }

    #[test]
    fn a_failed_recording_counts_after_two_polls_unless_the_camera_is_offline() {
        let mut t = HealthTracker::default();
        let failing = || cams(vec![camera("a", "error", Some("failed"))]);
        assert!(observe(&mut t, failing()).is_empty());
        assert_eq!(
            observe(&mut t, failing()),
            [("start".to_owned(), EventKind::RecordingFailed)]
        );
        assert_eq!(
            observe(
                &mut t,
                cams(vec![camera("a", "recording_ready", Some("recording"))])
            )
            .len(),
            1
        );

        let mut t = HealthTracker::default();
        let offline = || cams(vec![camera("a", "offline", Some("failed"))]);
        observe(&mut t, offline());
        let kinds: Vec<_> = observe(&mut t, offline())
            .into_iter()
            .map(|c| c.1)
            .collect();
        assert!(!kinds.contains(&EventKind::RecordingFailed));
    }

    #[test]
    fn a_removed_camera_ends_its_events() {
        let mut t = HealthTracker::default();
        observe(&mut t, cams(vec![camera("a", "offline", None)]));
        assert_eq!(
            observe(&mut t, cams(vec![])),
            [("end".to_owned(), EventKind::Motion)]
        );
    }

    #[test]
    fn a_missing_location_is_one_event_that_explains_its_cameras_failures() {
        let missing = |cameras: Vec<CameraHealth>, gone: bool| HealthSnapshot {
            cameras,
            drives: vec![],
            missing_storage: if gone {
                vec![(
                    PathBuf::from("/mnt/nas/rec"),
                    "/mnt/nas is not mounted".into(),
                )]
            } else {
                vec![]
            },
        };
        let blocked = || {
            let mut cam = camera("a", "streaming", Some("failed"));
            cam.storage_missing = true;
            cam
        };
        let mut t = HealthTracker::default();
        assert_eq!(
            observe(&mut t, missing(vec![blocked()], true)),
            [("start".to_owned(), EventKind::StorageMissing)]
        );
        assert!(
            observe(&mut t, missing(vec![blocked()], true)).is_empty(),
            "no recording_failed for a camera whose location is missing"
        );
        let back = observe(
            &mut t,
            missing(
                vec![camera("a", "recording_ready", Some("recording"))],
                false,
            ),
        );
        assert_eq!(
            back.len(),
            1,
            "the event ends when the location is back: {back:?}"
        );
    }

    #[test]
    fn disk_low_has_hysteresis() {
        const GB: u64 = 1024 * 1024 * 1024;
        let drive = |available: u64| HealthSnapshot {
            cameras: vec![],
            drives: vec![(PathBuf::from("/rec"), Some((50 * GB, available)))],
            missing_storage: vec![],
        };
        let mut t = HealthTracker::default();
        assert!(observe(&mut t, drive(40 * GB)).is_empty());
        assert_eq!(
            observe(&mut t, drive(GB)),
            [("start".to_owned(), EventKind::DiskLow)]
        );
        // 2.5 GB free: no longer below the line, not yet clearly above it.
        assert!(observe(&mut t, drive(5 * GB / 2)).is_empty());
        assert_eq!(observe(&mut t, drive(40 * GB)).len(), 1);
    }
}
