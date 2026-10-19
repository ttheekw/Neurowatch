//! Events: motion, camera analytics, camera offline, recording failed, disk
//! low, bookmarks.
//!
//! Producers (the ONVIF event subscriber in [`crate::onvif_events`], the
//! [`health`] monitor, `events.bookmark`) report through the one
//! [`EventHub`] on [`crate::app_state::AppState`]. The hub numbers each
//! event, keeps the latest notices in memory for `events.wait` (the IPC has
//! no push stream), broadcasts them to in-process consumers and hands the
//! database writes to its own thread, so a producer never waits for SQLite.
//!
//! Every event ends: one still open when the backend stops is closed at its
//! last-seen time on the next start.

pub mod health;
pub mod store;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nw_core::camera::CameraId;
use nw_core::event::{EventKind, EventNotice, EventPhase, EventRecord, EventSource};
use tokio::sync::{broadcast, watch};

/// Notices kept for `events.wait`; a client further behind starts over.
const RECENT_NOTICES: usize = 1024;
/// Longest `events.wait` holds a request open.
pub const MAX_WAIT: Duration = Duration::from_secs(25);

/// Event pictures: `<local data dir>/neurowatch/events`.
pub fn events_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neurowatch")
        .join("events")
}

/// Remove event pictures (paths relative to [`events_dir`]); never a path
/// outside it.
pub async fn remove_thumbnails(thumbnails: Vec<String>) {
    if thumbnails.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        let root = events_dir();
        for relative in thumbnails {
            let path = std::path::Path::new(&relative);
            if path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                continue;
            }
            let _ = std::fs::remove_file(root.join(path));
        }
    })
    .await;
}

/// Delete finished events older than the configured retention.
pub async fn prune_old(state: &Arc<tokio::sync::RwLock<crate::app_state::AppState>>) {
    let (db, days) = {
        let st = state.read().await;
        (Arc::clone(&st.db), st.config.events.retention_days)
    };
    if days == 0 {
        return;
    }
    let before = now_ms() - i64::from(days) * 24 * 60 * 60 * 1000;
    let pruned = tokio::task::spawn_blocking(move || {
        let conn = db.lock().ok()?;
        store::prune(&conn, before).ok()
    })
    .await
    .ok()
    .flatten();
    if let Some(thumbnails) = pruned {
        remove_thumbnails(thumbnails).await;
    }
}

pub fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// A new event, before the hub numbers it.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub camera_id: Option<CameraId>,
    pub kind: EventKind,
    pub source: EventSource,
    pub topic: Option<String>,
    pub detail: Option<String>,
    pub score: Option<f32>,
    /// Keep the recordings it covers (bookmarks).
    pub kept: bool,
}

impl NewEvent {
    pub fn new(camera_id: Option<CameraId>, kind: EventKind, source: EventSource) -> Self {
        Self {
            camera_id,
            kind,
            source,
            topic: None,
            detail: None,
            score: None,
            kept: false,
        }
    }

    pub fn kept(mut self, kept: bool) -> Self {
        self.kept = kept;
        self
    }

    pub fn topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// `events.wait`'s answer: notices after the client's cursor, or `reset`
/// when the cursor is from another backend run or too old to continue.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitResult {
    pub boot: String,
    pub seq: u64,
    pub reset: bool,
    pub notices: Vec<EventNotice>,
}

enum Write {
    Insert(EventRecord, i64),
    Update(EventRecord, i64),
    Touch(Vec<i64>, i64),
    Flush(mpsc::Sender<()>),
    FlushAsync(tokio::sync::oneshot::Sender<()>),
}

struct Inner {
    seq: u64,
    recent: VecDeque<EventNotice>,
    open: HashMap<i64, EventRecord>,
}

pub struct EventHub {
    boot: String,
    activity_pause: Mutex<HashMap<String, (std::time::Instant, u64)>>,
    activity_generation: std::sync::atomic::AtomicU64,
    next_id: AtomicI64,
    inner: Mutex<Inner>,
    latest: watch::Sender<u64>,
    notices: broadcast::Sender<EventNotice>,
    writer: Mutex<mpsc::Sender<Write>>,
}

impl EventHub {
    /// Close events a previous run left open, and start the writer thread.
    pub fn new(db: Arc<Mutex<rusqlite::Connection>>) -> Self {
        let next_id = match db.lock() {
            Ok(conn) => {
                if let Err(error) = store::close_dangling(&conn) {
                    tracing::warn!(%error, "Could not close events left open by the last run");
                }
                store::max_id(&conn).unwrap_or(0) + 1
            }
            Err(_) => 1,
        };
        let (writer, writes) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("nw-events-db".into())
            .spawn(move || write_loop(&db, &writes));
        if let Err(error) = spawned {
            tracing::error!(%error, "Events database writer did not start; events are not stored");
        }
        let mut boot = [0u8; 8];
        let _ = getrandom::getrandom(&mut boot);
        Self {
            activity_pause: Mutex::new(HashMap::new()),
            activity_generation: std::sync::atomic::AtomicU64::new(0),
            boot: boot.iter().map(|b| format!("{b:02x}")).collect(),
            next_id: AtomicI64::new(next_id),
            inner: Mutex::new(Inner {
                seq: 0,
                recent: VecDeque::new(),
                open: HashMap::new(),
            }),
            latest: watch::channel(0).0,
            notices: broadcast::channel(RECENT_NOTICES).0,
            writer: Mutex::new(writer),
        }
    }

    pub fn pause_activity(&self, camera_id: &str, duration: std::time::Duration) {
        if let Ok(mut pauses) = self.activity_pause.lock() {
            pauses.retain(|_, (until, _)| *until > std::time::Instant::now());
            pauses.insert(
                camera_id.into(),
                (
                    std::time::Instant::now() + duration,
                    self.activity_generation.fetch_add(1, Ordering::SeqCst) + 1,
                ),
            );
        }
        for event in self.open_events().into_iter().filter(|event| {
            event.camera_id.as_deref() == Some(camera_id)
                && matches!(
                    event.kind,
                    EventKind::Motion | EventKind::Intrusion | EventKind::LineCrossing
                )
        }) {
            self.end(event.id);
        }
    }
    pub fn activity_generation(&self, camera_id: &str) -> u64 {
        self.activity_pause
            .lock()
            .ok()
            .and_then(|pauses| pauses.get(camera_id).map(|(_, seq)| *seq))
            .unwrap_or(0)
    }
    pub fn activity_paused(&self, camera_id: &str) -> bool {
        self.activity_pause
            .lock()
            .map(|pauses| {
                pauses
                    .get(camera_id)
                    .is_some_and(|(until, _)| *until > std::time::Instant::now())
            })
            .unwrap_or(true)
    }

    /// Identifies this backend run; cursors from another run start over.
    pub fn boot(&self) -> &str {
        &self.boot
    }

    /// Notices as they happen, for consumers inside the backend. A consumer
    /// that falls behind loses the oldest (it never slows a producer).
    pub fn subscribe(&self) -> broadcast::Receiver<EventNotice> {
        self.notices.subscribe()
    }

    /// Start an event now.
    pub fn start(&self, event: NewEvent) -> EventRecord {
        self.start_at(event, now_ms())
    }

    /// Start an event at `start_ms`.
    pub fn start_at(&self, event: NewEvent, start_ms: i64) -> EventRecord {
        let record = EventRecord {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            camera_id: event.camera_id,
            kind: event.kind,
            source: event.source,
            start_ms,
            end_ms: None,
            topic: event.topic,
            score: event.score,
            zones: Vec::new(),
            bbox: None,
            thumbnail: None,
            detail: event.detail,
            acknowledged: false,
            kept: event.kept,
        };
        self.lock().open.insert(record.id, record.clone());
        self.write(Write::Insert(record.clone(), start_ms));
        self.notify(EventPhase::Start, record.clone());
        record
    }

    /// An event without duration (a line crossed, a bookmark).
    pub fn instant(&self, event: NewEvent, at_ms: i64) -> EventRecord {
        let record = self.start_at(event, at_ms);
        self.end_at(record.id, at_ms).unwrap_or(record)
    }

    /// Change an open event (score, zones, detail). `None` if it is not open.
    pub fn update(&self, id: i64, change: impl FnOnce(&mut EventRecord)) -> Option<EventRecord> {
        let record = {
            let mut inner = self.lock();
            let record = inner.open.get_mut(&id)?;
            change(record);
            record.clone()
        };
        self.write(Write::Update(record.clone(), now_ms()));
        self.notify(EventPhase::Update, record.clone());
        Some(record)
    }

    /// Tell clients about a stored event changed outside the hub (a
    /// bookmark's note or kept footage).
    pub fn changed(&self, record: EventRecord) {
        self.notify(EventPhase::Update, record);
    }

    /// End an open event now.
    pub fn end(&self, id: i64) -> Option<EventRecord> {
        self.end_at(id, now_ms())
    }

    /// End an open event at `end_ms` (never before it started).
    pub fn end_at(&self, id: i64, end_ms: i64) -> Option<EventRecord> {
        let mut record = self.lock().open.remove(&id)?;
        let end_ms = end_ms.max(record.start_ms);
        record.end_ms = Some(end_ms);
        self.write(Write::Update(record.clone(), end_ms));
        self.notify(EventPhase::End, record.clone());
        Some(record)
    }

    /// Events still going on.
    pub fn open_events(&self) -> Vec<EventRecord> {
        let mut open: Vec<EventRecord> = self.lock().open.values().cloned().collect();
        open.sort_by_key(|record| record.id);
        open
    }

    /// Note that open events are still going on, so a backend that stops
    /// closes them at about the right time on its next start.
    pub fn touch_open(&self) {
        let ids: Vec<i64> = self.lock().open.keys().copied().collect();
        if !ids.is_empty() {
            self.write(Write::Touch(ids, now_ms()));
        }
    }

    /// Notices after `after`, waiting up to `timeout` for one. `after: None`
    /// only reports the current cursor.
    pub async fn wait(
        &self,
        boot: Option<&str>,
        after: Option<u64>,
        timeout: Duration,
    ) -> WaitResult {
        let mut latest = self.latest.subscribe();
        let current = *latest.borrow_and_update();
        let after = match (boot, after) {
            (Some(boot), _) if boot != self.boot => return self.reset(current),
            (_, None) => return self.result(current, Vec::new()),
            (_, Some(after)) if after > current => return self.reset(current),
            (_, Some(after)) => after,
        };
        if current == after {
            let waiting = Instant::now();
            let deadline = tokio::time::sleep(timeout.min(MAX_WAIT));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    changed = latest.changed() => {
                        if changed.is_err() || *latest.borrow_and_update() > after {
                            break;
                        }
                    }
                    _ = &mut deadline => break,
                }
            }
            crate::stats::ipc_metrics::record_idle_wait(waiting.elapsed());
        }
        self.since(after)
    }

    /// Notices after `after`, without waiting.
    pub fn since(&self, after: u64) -> WaitResult {
        let inner = self.lock();
        let oldest = inner.recent.front().map_or(inner.seq + 1, |n| n.seq);
        if after + 1 < oldest {
            let seq = inner.seq;
            drop(inner);
            return self.reset(seq);
        }
        let notices = inner
            .recent
            .iter()
            .filter(|notice| notice.seq > after)
            .cloned()
            .collect();
        self.result(inner.seq, notices)
    }

    /// Wait until the writer stored everything sent so far (tests, shutdown).
    pub fn flush(&self) {
        let (done, finished) = mpsc::channel();
        self.write(Write::Flush(done));
        let _ = finished.recv_timeout(Duration::from_secs(5));
    }

    /// Await the writer barrier without blocking a Tokio worker. Used by
    /// history/image/acknowledgement RPCs; synchronous flush is for threads.
    pub async fn flush_async(&self) -> Result<(), &'static str> {
        let (done, finished) = tokio::sync::oneshot::channel();
        self.write(Write::FlushAsync(done));
        tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .map_err(|_| "Timed out waiting for the events writer")?
            .map_err(|_| "The events writer stopped")
    }

    fn result(&self, seq: u64, notices: Vec<EventNotice>) -> WaitResult {
        WaitResult {
            boot: self.boot.clone(),
            seq,
            reset: false,
            notices,
        }
    }

    fn reset(&self, seq: u64) -> WaitResult {
        WaitResult {
            reset: true,
            ..self.result(seq, Vec::new())
        }
    }

    fn notify(&self, phase: EventPhase, event: EventRecord) {
        let notice = {
            let mut inner = self.lock();
            inner.seq += 1;
            let notice = EventNotice {
                seq: inner.seq,
                phase,
                event,
            };
            inner.recent.push_back(notice.clone());
            while inner.recent.len() > RECENT_NOTICES {
                inner.recent.pop_front();
            }
            notice
        };
        tracing::info!(
            seq = notice.seq,
            id = notice.event.id,
            kind = notice.event.kind.as_str(),
            phase = ?notice.phase,
            cam_id = notice.event.camera_id.as_deref().unwrap_or("-"),
            "Event"
        );
        self.latest.send_replace(notice.seq);
        let _ = self.notices.send(notice);
    }

    fn write(&self, write: Write) {
        let sent = self
            .writer
            .lock()
            .map(|writer| writer.send(write).is_ok())
            .unwrap_or(false);
        if !sent {
            tracing::warn!("Events database writer stopped; event not stored");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn write_loop(db: &Arc<Mutex<rusqlite::Connection>>, writes: &mpsc::Receiver<Write>) {
    while let Ok(write) = writes.recv() {
        let Ok(conn) = db.lock() else {
            tracing::error!("Events database lock poisoned; events are no longer stored");
            return;
        };
        let result = match write {
            Write::Insert(record, seen) => store::insert(&conn, &record, seen),
            Write::Update(record, seen) => store::update(&conn, &record, seen),
            Write::Touch(ids, seen) => store::touch(&conn, &ids, seen),
            Write::Flush(done) => {
                let _ = done.send(());
                Ok(())
            }
            Write::FlushAsync(done) => {
                let _ = done.send(());
                Ok(())
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, "Could not store an event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> (EventHub, Arc<Mutex<rusqlite::Connection>>) {
        let db = Arc::new(Mutex::new(
            crate::db::connection::open_in_memory().expect("db"),
        ));
        (EventHub::new(Arc::clone(&db)), db)
    }

    fn motion(cam: &str) -> NewEvent {
        NewEvent::new(Some(cam.into()), EventKind::Motion, EventSource::Onvif)
    }

    #[test]
    fn events_are_numbered_stored_and_closed() {
        let (hub, db) = hub();
        let first = hub.start_at(motion("a"), 1_000);
        let second = hub.start_at(motion("b"), 2_000);
        assert_eq!(second.id, first.id + 1);
        assert_eq!(hub.open_events().len(), 2);
        let ended = hub.end_at(first.id, 500).expect("open");
        assert_eq!(ended.end_ms, Some(1_000), "never ends before it started");
        assert!(hub.end(first.id).is_none(), "already ended");
        hub.flush();
        let conn = db.lock().unwrap();
        let stored = store::get(&conn, first.id).unwrap().expect("stored");
        assert_eq!(stored.end_ms, Some(1_000));
        let open = store::get(&conn, second.id).unwrap().expect("stored");
        assert_eq!(open.end_ms, None);
    }

    #[test]
    fn a_new_hub_closes_events_the_last_run_left_open_and_continues_ids() {
        let (hub, db) = hub();
        let open = hub.start_at(motion("a"), 1_000);
        hub.flush();
        drop(hub);
        db.lock()
            .unwrap()
            .execute("UPDATE events SET last_ts = 4000 WHERE id = ?1", [open.id])
            .unwrap();
        let next = EventHub::new(Arc::clone(&db));
        let closed = store::get(&db.lock().unwrap(), open.id).unwrap().unwrap();
        assert_eq!(closed.end_ms, Some(4_000));
        assert_eq!(next.start(motion("a")).id, open.id + 1);
    }

    #[tokio::test]
    async fn wait_returns_notices_after_the_cursor() {
        let (hub, _db) = hub();
        let cursor = hub.wait(None, None, Duration::ZERO).await;
        assert_eq!((cursor.seq, cursor.reset), (0, false));
        let event = hub.instant(motion("a"), 10);
        let got = hub
            .wait(Some(hub.boot()), Some(cursor.seq), Duration::from_secs(1))
            .await;
        let phases: Vec<_> = got.notices.iter().map(|n| n.phase).collect();
        assert_eq!(phases, [EventPhase::Start, EventPhase::End]);
        assert_eq!(got.notices[1].event.id, event.id);
        assert_eq!(got.seq, 2);
        let other_run = hub.wait(Some("other"), Some(2), Duration::ZERO).await;
        assert!(other_run.reset);
    }

    #[tokio::test]
    async fn wait_wakes_when_an_event_arrives() {
        let (hub, _db) = hub();
        let hub = Arc::new(hub);
        let waiter = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move { hub.wait(None, Some(0), Duration::from_secs(10)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub.start(motion("a"));
        let got = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("woke")
            .expect("joined");
        assert_eq!(got.notices.len(), 1);
    }

    #[test]
    fn a_cursor_older_than_the_kept_notices_starts_over() {
        let (hub, _db) = hub();
        for _ in 0..(RECENT_NOTICES + 10) {
            hub.update(-1, |_| {});
            hub.instant(motion("a"), 1);
        }
        assert!(hub.since(1).reset);
        assert!(!hub.since(hub.since(0).seq).reset);
    }
}
