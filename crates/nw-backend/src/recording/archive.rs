//! The archive tier: recordings older than `recording.archive.after_days`
//! move from their recording location to the archive folder (typically a
//! network share), where playback and export still find them.
//!
//! A recording moves in four steps: copy to `<file>.part` (at most the
//! configured speed), flush it, read it back and compare size and SHA-256
//! with the original, rename. Only then, under the index lock (so the
//! segment indexer never sees half of it), is the original deleted and its
//! row pointed at the copy (`tier = 'archive'`). A pass that stops at any
//! step leaves the original in place; the next pass reuses a matching copy.
//!
//! Moves run only within the configured hours and only while the archive
//! (and the recording's own location) is really there: the archive carries
//! a marker like every recording location (`crate::storage_guard`).
//! Layout: `<archive>/<camera id>/<YYYY-MM-DD>/<file>`, as in recording
//! locations, so the indexer reads an archive the same way.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::app_state::{expand_user_path, AppState};
use crate::config::ArchiveConfig;
use crate::recording::quota::{ARCHIVE_TIER, LOCAL_TIER};

/// Bytes read or written at a time.
const CHUNK: usize = 1 << 20;
/// A pass starts this often (and when the settings change).
const PASS_EVERY: Duration = Duration::from_secs(300);
/// The first pass waits for startup (indexing) to settle.
const FIRST_PASS: Duration = Duration::from_secs(60);
/// A file modified this recently may still be written.
const SETTLED: Duration = Duration::from_secs(60);
/// Recordings read from the index at a time.
const BATCH: i64 = 64;
/// A pass gives up after this many failed moves in a row (the archive is
/// full or refuses writes): the next pass tries again.
const MAX_FAILURES_IN_A_ROW: u32 = 3;

/// What the archive is doing, for `recording.archive_status`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Status {
    /// `off`, `waiting` (outside its hours), `missing` (the archive folder is
    /// not there), `error` (cannot run as configured), `moving` or `idle`.
    pub state: &'static str,
    /// Why, for `missing`, `error` and `waiting`.
    pub detail: Option<String>,
    /// Recordings moved since the recorder started.
    pub moved_files: u64,
    pub moved_bytes: u64,
    /// The last move that failed, and why.
    pub last_error: Option<String>,
    /// When the last pass ended (ms since the epoch).
    pub last_pass_ms: Option<i64>,
}

/// The archive's state, shared with the IPC (`AppState::archive`).
#[derive(Default)]
pub struct Archive {
    status: Mutex<Status>,
    cancel: AtomicBool,
    wake: tokio::sync::Notify,
}

impl Archive {
    pub fn status(&self) -> Status {
        let status = self.lock().clone();
        if status.state.is_empty() {
            return Status {
                state: "off",
                ..status
            };
        }
        status
    }

    /// The settings changed: stop a running pass and start a new one.
    pub fn reconfigured(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_state(&self, state: &'static str, detail: Option<String>) {
        let mut status = self.lock();
        status.state = state;
        status.detail = detail;
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// Recordings due for the archive and already there.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Totals {
    pub pending_files: u64,
    pub pending_bytes: u64,
    pub archived_files: u64,
    pub archived_bytes: u64,
}

/// [`Totals`] for recordings that end before `cutoff_ms`.
pub fn totals(conn: &Connection, cutoff_ms: i64) -> Totals {
    let count = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> (u64, u64) {
        conn.query_row(sql, params, |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .map(|(files, bytes)| {
            (
                u64::try_from(files).unwrap_or(0),
                u64::try_from(bytes).unwrap_or(0),
            )
        })
        .unwrap_or_default()
    };
    let (pending_files, pending_bytes) = count(
        "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM segments \
         WHERE tier = ?1 AND local_deleted = 0 AND timing_exact > 0 \
           AND end_ts IS NOT NULL AND end_ts < ?2",
        &[&LOCAL_TIER, &cutoff_ms],
    );
    let (archived_files, archived_bytes) = count(
        "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM segments \
         WHERE tier = ?1 AND local_deleted = 0",
        &[&ARCHIVE_TIER],
    );
    Totals {
        pending_files,
        pending_bytes,
        archived_files,
        archived_bytes,
    }
}

/// Recordings ending before this are due for the archive.
pub fn cutoff_ms(config: &ArchiveConfig, now_ms: i64) -> i64 {
    now_ms.saturating_sub(i64::from(config.after_days) * 86_400_000)
}

/// Why `archive` cannot be used with these recording locations: one inside
/// the other would move recordings into a folder the indexer reads as a
/// recording location (or the reverse).
pub fn overlap(archive: &Path, recording_roots: &[PathBuf]) -> Option<String> {
    recording_roots
        .iter()
        .find(|root| archive.starts_with(root) || root.starts_with(archive))
        .map(|root| {
            format!(
                "the archive folder must be outside the recording location {}",
                root.display()
            )
        })
}

/// Every recording location of `st`'s configuration.
pub fn recording_roots(st: &AppState) -> Vec<PathBuf> {
    let mut roots = vec![st.global_storage_root()];
    for cam in &st.config.cameras {
        let root = st.camera_storage_root(cam);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn within_hours(config: &ArchiveConfig) -> bool {
    match crate::recording::policy::local_calendar() {
        Some((_, minute)) => config.allows(minute as u32),
        None => true,
    }
}

/// Move due recordings every [`PASS_EVERY`] (and soon after a settings
/// change) until shutdown.
pub async fn run(state: Arc<RwLock<AppState>>, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let archive = Arc::clone(&state.read().await.archive);
    tokio::select! {
        _ = tokio::time::sleep(FIRST_PASS) => {}
        _ = shutdown.recv() => return,
    }
    loop {
        // Runs as a task of its own so shutdown can cancel the copy at once.
        let pass = tokio::spawn(pass(Arc::clone(&state)));
        tokio::pin!(pass);
        tokio::select! {
            _ = &mut pass => {}
            _ = shutdown.recv() => {
                archive.cancel.store(true, Ordering::SeqCst);
                let _ = pass.await;
                return;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(PASS_EVERY) => {}
            _ = archive.wake.notified() => {
                // Let a burst of settings saves settle.
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            _ = shutdown.recv() => return,
        }
    }
}

/// One pass: move what is due, as far as the hours and the archive allow.
pub async fn pass(state: Arc<RwLock<AppState>>) {
    let (config, recording_roots, db, archive) = {
        let st = state.read().await;
        st.archive.cancel.store(false, Ordering::SeqCst);
        (
            st.config.recording.archive.clone(),
            recording_roots(&st),
            Arc::clone(&st.db),
            Arc::clone(&st.archive),
        )
    };
    if !config.enabled || config.path.trim().is_empty() {
        archive.set_state("off", None);
        return;
    }
    let root = expand_user_path(&config.path);
    if let Some(reason) = overlap(&root, &recording_roots) {
        archive.set_state("error", Some(reason));
        return;
    }
    if !within_hours(&config) {
        let detail = config
            .hours
            .as_ref()
            .map(|hours| format!("archiving runs from {} to {}", hours.start, hours.end));
        archive.set_state("waiting", detail);
        return;
    }
    if let Err(reason) = crate::storage_guard::ensure(&state, &root).await {
        archive.set_state("missing", Some(reason));
        return;
    }
    let offline = state.read().await.storage.missing_roots();
    archive.set_state("moving", None);
    let plan = Plan {
        cutoff_ms: cutoff_ms(&config, now_ms()),
        bytes_per_sec: config.max_mbps.map(|mbps| u64::from(mbps) * 1_000_000 / 8),
        root,
        config,
        offline,
    };
    let worker = Arc::clone(&archive);
    let outcome = tokio::task::spawn_blocking(move || move_due(&db, &plan, &worker))
        .await
        .unwrap_or_default();
    if outcome.moved > 0 {
        tracing::info!(
            moved = outcome.moved,
            bytes = outcome.bytes,
            "Recordings moved to the archive"
        );
    }
    let mut status = archive.lock();
    status.state = "idle";
    status.detail = None;
    status.moved_files += outcome.moved;
    status.moved_bytes += outcome.bytes;
    if outcome.error.is_some() {
        status.last_error = outcome.error;
    }
    status.last_pass_ms = Some(now_ms());
}

struct Plan {
    root: PathBuf,
    cutoff_ms: i64,
    bytes_per_sec: Option<u64>,
    config: ArchiveConfig,
    offline: Vec<PathBuf>,
}

#[derive(Default)]
struct Outcome {
    moved: u64,
    bytes: u64,
    error: Option<String>,
}

struct Due {
    id: i64,
    camera_id: String,
    path: String,
    start_ts: i64,
}

fn due(db: &Arc<Mutex<Connection>>, cutoff_ms: i64, cursor: (i64, i64)) -> Option<Vec<Due>> {
    let conn = db.lock().ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, camera_id, path, start_ts FROM segments \
             WHERE tier = ?1 AND local_deleted = 0 AND timing_exact > 0 \
               AND end_ts IS NOT NULL AND end_ts < ?2 AND (start_ts, id) > (?3, ?4) \
             ORDER BY start_ts ASC, id ASC LIMIT ?5",
        )
        .ok()?;
    let rows = stmt
        .query_map(
            rusqlite::params![LOCAL_TIER, cutoff_ms, cursor.0, cursor.1, BATCH],
            |r| {
                Ok(Due {
                    id: r.get(0)?,
                    camera_id: r.get(1)?,
                    path: r.get(2)?,
                    start_ts: r.get(3)?,
                })
            },
        )
        .ok()?;
    Some(rows.flatten().collect())
}

fn move_due(db: &Arc<Mutex<Connection>>, plan: &Plan, archive: &Archive) -> Outcome {
    let mut outcome = Outcome::default();
    let mut cursor = (i64::MIN, i64::MIN);
    let mut failures = 0;
    let settled_before = SystemTime::now() - SETTLED;
    'pass: loop {
        let batch = match due(db, plan.cutoff_ms, cursor) {
            Some(batch) if !batch.is_empty() => batch,
            _ => break,
        };
        for segment in batch {
            if archive.cancelled() || !within_hours(&plan.config) {
                break 'pass;
            }
            cursor = (segment.start_ts, segment.id);
            let source = PathBuf::from(&segment.path);
            if plan.offline.iter().any(|root| source.starts_with(root)) {
                continue;
            }
            // A missing original is left to cleanup; a fresh one may be written.
            let Ok(meta) = std::fs::metadata(&source) else {
                continue;
            };
            if meta
                .modified()
                .map_or(true, |modified| modified > settled_before)
            {
                continue;
            }
            let Some(target) = destination(&plan.root, &segment.camera_id, &source) else {
                continue;
            };
            let result = copy_verified(&source, &target, meta.len(), plan.bytes_per_sec, archive)
                .and_then(|()| commit(db, &segment, &source, &target).map_err(Failure::Failed));
            match result {
                Ok(true) => {
                    failures = 0;
                    outcome.moved += 1;
                    outcome.bytes += meta.len();
                    tracing::debug!(from = %source.display(), to = %target.display(), "Recording archived");
                }
                Ok(false) => {}
                Err(Failure::Cancelled) => break 'pass,
                Err(Failure::Failed(error)) => {
                    tracing::warn!(path = %source.display(), %error, "Cannot archive recording");
                    outcome.error = Some(error);
                    failures += 1;
                    if failures >= MAX_FAILURES_IN_A_ROW {
                        break 'pass;
                    }
                }
            }
        }
    }
    outcome
}

/// Where `source` (a recording of `camera_id`) goes in the archive, or
/// `None` for names that are not plain folder or file names.
fn destination(root: &Path, camera_id: &str, source: &Path) -> Option<PathBuf> {
    fn plain(name: &str) -> bool {
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
    }
    let file = source.file_name()?.to_str().filter(|name| plain(name))?;
    if !plain(camera_id) {
        return None;
    }
    let mut target = root.join(camera_id);
    // `<camera>/<day>/<file>`; older recordings lie in `<camera>/` itself.
    if let Some(day) = source
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|day| *day != camera_id && plain(day))
    {
        target.push(day);
    }
    Some(target.join(file))
}

enum Failure {
    Cancelled,
    Failed(String),
}

fn failed(what: &str, path: &Path, error: impl std::fmt::Display) -> Failure {
    Failure::Failed(format!("cannot {what} {}: {error}", path.display()))
}

/// Read `path` to its end through `sink`, at most `bytes_per_sec`. Returns
/// its SHA-256 and length.
fn stream(
    path: &Path,
    bytes_per_sec: Option<u64>,
    archive: &Archive,
    mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<(Vec<u8>, u64), Failure> {
    let mut file = std::fs::File::open(path).map_err(|e| failed("read", path, e))?;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; CHUNK];
    let (started, mut total) = (Instant::now(), 0u64);
    loop {
        if archive.cancelled() {
            return Err(Failure::Cancelled);
        }
        let read = file
            .read(&mut buffer)
            .map_err(|e| failed("read", path, e))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        sink(&buffer[..read])
            .map_err(|e| Failure::Failed(format!("cannot write the archive copy: {e}")))?;
        total += read as u64;
        if let Some(rate) = bytes_per_sec.filter(|rate| *rate > 0) {
            let due = Duration::from_secs_f64(total as f64 / rate as f64);
            // Short sleeps, so a cancel is noticed at once.
            while started.elapsed() < due {
                if archive.cancelled() {
                    return Err(Failure::Cancelled);
                }
                std::thread::sleep((due - started.elapsed()).min(Duration::from_millis(200)));
            }
        }
    }
    Ok((digest.finish().as_ref().to_vec(), total))
}

/// Copy `source` (`len` bytes) to `target` through a `.part` file, flushed
/// and read back to compare its SHA-256 before the rename. A matching copy
/// already there (an earlier pass stopped after the rename) is kept.
fn copy_verified(
    source: &Path,
    target: &Path,
    len: u64,
    bytes_per_sec: Option<u64>,
    archive: &Archive,
) -> Result<(), Failure> {
    let parent = target
        .parent()
        .ok_or_else(|| Failure::Failed("no archive folder".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| failed("create", parent, e))?;
    if target.exists() {
        let (theirs, their_len) = stream(target, bytes_per_sec, archive, |_| Ok(()))?;
        let (ours, our_len) = stream(source, None, archive, |_| Ok(()))?;
        return if theirs == ours && their_len == our_len && our_len == len {
            Ok(())
        } else {
            Err(Failure::Failed(format!(
                "a different file is already at {}",
                target.display()
            )))
        };
    }
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    let part = target.with_file_name(name);
    let result = (|| {
        let mut out = std::fs::File::create(&part).map_err(|e| failed("create", &part, e))?;
        let (hash, copied) = stream(source, bytes_per_sec, archive, |chunk| out.write_all(chunk))?;
        out.sync_all().map_err(|e| failed("flush", &part, e))?;
        drop(out);
        if copied != len {
            return Err(Failure::Failed(format!(
                "{} changed while it was copied",
                source.display()
            )));
        }
        let (check, checked) = stream(&part, bytes_per_sec, archive, |_| Ok(()))?;
        if check != hash || checked != len {
            return Err(Failure::Failed(format!(
                "the copy at {} does not match its original",
                part.display()
            )));
        }
        std::fs::rename(&part, target).map_err(|e| failed("rename", &part, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// The copy is verified: delete the original and point its row at the copy,
/// under the index lock. `Ok(false)`: the recording changed meanwhile (it was
/// deleted by cleanup), so the copy is removed instead.
fn commit(
    db: &Arc<Mutex<Connection>>,
    segment: &Due,
    source: &Path,
    target: &Path,
) -> Result<bool, String> {
    let conn = db
        .lock()
        .map_err(|_| "the recording index is unavailable".to_owned())?;
    let target_text = target.to_string_lossy().into_owned();
    let query = |sql: &str, value: &dyn rusqlite::ToSql| -> Result<Option<String>, String> {
        conn.query_row(sql, [value], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())
    };
    let current = query("SELECT path FROM segments WHERE id = ?1", &segment.id)?;
    let indexed_copy = query(
        "SELECT CAST(id AS TEXT) FROM segments WHERE path = ?1",
        &target_text,
    )?;
    if current.as_deref() != Some(segment.path.as_str()) {
        if indexed_copy.is_none() {
            let _ = std::fs::remove_file(target);
        }
        return Ok(false);
    }
    match std::fs::remove_file(source) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // In use (Windows): the next pass tries again and reuses the copy.
        Err(e) => return Err(format!("cannot remove {}: {e}", source.display())),
    }
    if indexed_copy.is_some() {
        conn.execute("DELETE FROM segments WHERE id = ?1", [segment.id])
    } else {
        conn.execute(
            "UPDATE segments SET path = ?1, tier = ?2 WHERE id = ?3",
            rusqlite::params![target_text, ARCHIVE_TIER, segment.id],
        )
    }
    .map_err(|e| e.to_string())?;
    drop(conn);
    // An emptied day folder goes too (never the camera's own folder).
    if let Some(day) = source
        .parent()
        .filter(|day| day.file_name().and_then(|n| n.to_str()) != Some(segment.camera_id.as_str()))
    {
        let _ = std::fs::remove_dir(day);
    }
    Ok(true)
}

/// Mark rows indexed from the archive folder of `camera_id` as archived
/// (an archive read after the index was lost or moved).
pub fn mark_archived(conn: &Connection, camera_id: &str, archive_camera_root: &Path) {
    let mut prefix = archive_camera_root.to_string_lossy().into_owned();
    prefix.push(std::path::MAIN_SEPARATOR);
    if let Err(error) = conn.execute(
        "UPDATE segments SET tier = ?1 WHERE camera_id = ?2 AND tier = ?3 \
           AND substr(path, 1, length(?4)) = ?4",
        rusqlite::params![ARCHIVE_TIER, camera_id, LOCAL_TIER, prefix],
    ) {
        tracing::warn!(camera_id, %error, "Cannot mark archived recordings");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::apply_migrations;

    struct Fixture {
        dir: PathBuf,
        db: Arc<Mutex<Connection>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("nw-archive-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let conn = Connection::open_in_memory().unwrap();
            apply_migrations(&conn).unwrap();
            Self {
                dir,
                db: Arc::new(Mutex::new(conn)),
            }
        }

        fn recording(
            &self,
            camera: &str,
            day: &str,
            name: &str,
            bytes: usize,
            end_ms: i64,
        ) -> PathBuf {
            write_recording(&self.dir, &self.db, camera, day, name, bytes, end_ms)
        }

        fn plan(&self, cutoff_ms: i64) -> Plan {
            Plan {
                root: self.dir.join("archive"),
                cutoff_ms,
                bytes_per_sec: None,
                config: ArchiveConfig::default(),
                offline: Vec::new(),
            }
        }

        fn rows(&self) -> Vec<(String, String)> {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT path, tier FROM segments ORDER BY start_ts")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .flatten()
                .collect()
        }
    }

    /// A finished recording of `camera` on `day`, last modified long ago.
    fn write_recording(
        dir: &Path,
        db: &Arc<Mutex<Connection>>,
        camera: &str,
        day: &str,
        name: &str,
        bytes: usize,
        end_ms: i64,
    ) -> PathBuf {
        let folder = dir.join("local").join(camera).join(day);
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(name);
        std::fs::write(&path, vec![7u8; bytes]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO segments (camera_id, start_ts, end_ts, path, size_bytes, timing_exact) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                rusqlite::params![camera, end_ms - 1000, end_ms, path.to_string_lossy(), bytes as i64],
            )
            .unwrap();
        path
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn due_recordings_move_verified_and_the_index_follows_them() {
        let fx = Fixture::new();
        let old = fx.recording(
            "cam-a",
            "2026-10-01",
            "cam-a_1.mkv",
            3 * CHUNK + 17,
            1_000_000,
        );
        let recent = fx.recording("cam-a", "2026-10-17", "cam-a_2.mkv", 100, 9_000_000);
        let outcome = move_due(&fx.db, &fx.plan(5_000_000), &Archive::default());

        let target = fx.dir.join("archive/cam-a/2026-10-01/cam-a_1.mkv");
        assert_eq!(outcome.moved, 1);
        assert_eq!(outcome.bytes, (3 * CHUNK + 17) as u64);
        assert!(
            !old.exists() && !old.parent().unwrap().exists(),
            "original and its empty day folder go"
        );
        assert_eq!(std::fs::read(&target).unwrap(), vec![7u8; 3 * CHUNK + 17]);
        assert!(recent.exists());
        assert_eq!(
            fx.rows(),
            vec![
                (
                    target.to_string_lossy().into_owned(),
                    ARCHIVE_TIER.to_owned()
                ),
                (recent.to_string_lossy().into_owned(), LOCAL_TIER.to_owned()),
            ]
        );
        let totals = totals(&fx.db.lock().unwrap(), 10_000_000);
        assert_eq!((totals.archived_files, totals.pending_files), (1, 1));
        // Nothing is left to move a second time.
        assert_eq!(
            move_due(&fx.db, &fx.plan(5_000_000), &Archive::default()).moved,
            0
        );
    }

    #[test]
    fn a_stopped_pass_leaves_the_original_and_a_matching_copy_is_reused() {
        let fx = Fixture::new();
        let source = fx.recording("cam-b", "2026-10-01", "cam-b_1.mkv", 2 * CHUNK, 1_000_000);
        let archive = Archive::default();
        archive.cancel.store(true, Ordering::SeqCst);
        assert_eq!(move_due(&fx.db, &fx.plan(5_000_000), &archive).moved, 0);
        let target = fx.dir.join("archive/cam-b/2026-10-01/cam-b_1.mkv");
        assert!(source.exists() && !target.exists());
        assert!(
            !target.with_file_name("cam-b_1.mkv.part").exists(),
            "no partial copy left"
        );

        // A pass that copied and renamed but stopped before the index changed.
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(&source, &target).unwrap();
        let outcome = move_due(&fx.db, &fx.plan(5_000_000), &Archive::default());
        assert_eq!(outcome.moved, 1);
        assert!(!source.exists());
        assert_eq!(fx.rows()[0].1, ARCHIVE_TIER);
    }

    #[test]
    fn a_different_file_in_the_archive_is_never_overwritten() {
        let fx = Fixture::new();
        let source = fx.recording("cam-c", "2026-10-01", "cam-c_1.mkv", 1000, 1_000_000);
        let target = fx.dir.join("archive/cam-c/2026-10-01/cam-c_1.mkv");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"someone else's").unwrap();
        let outcome = move_due(&fx.db, &fx.plan(5_000_000), &Archive::default());
        assert_eq!(outcome.moved, 0);
        assert!(outcome.error.unwrap().contains("different file"));
        assert!(source.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"someone else's");
        assert_eq!(fx.rows()[0].1, LOCAL_TIER);
    }

    #[test]
    fn recordings_on_a_missing_location_or_deleted_meanwhile_stay_as_they_are() {
        let fx = Fixture::new();
        let source = fx.recording("cam-d", "2026-10-01", "cam-d_1.mkv", 1000, 1_000_000);
        let mut plan = fx.plan(5_000_000);
        plan.offline = vec![fx.dir.join("local")];
        assert_eq!(move_due(&fx.db, &plan, &Archive::default()).moved, 0);
        assert!(source.exists());

        // Cleanup deleted the recording while it was being copied.
        let target = fx.dir.join("archive/cam-d/2026-10-01/cam-d_1.mkv");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(&source, &target).unwrap();
        let segment = Due {
            id: 1,
            camera_id: "cam-d".into(),
            path: source.to_string_lossy().into_owned(),
            start_ts: 0,
        };
        fx.db
            .lock()
            .unwrap()
            .execute("DELETE FROM segments", [])
            .unwrap();
        assert_eq!(commit(&fx.db, &segment, &source, &target), Ok(false));
        assert!(!target.exists(), "the orphaned copy goes");
    }

    #[test]
    fn the_speed_limit_paces_the_copy() {
        let fx = Fixture::new();
        fx.recording("cam-e", "2026-10-01", "cam-e_1.mkv", CHUNK, 1_000_000);
        let mut plan = fx.plan(5_000_000);
        // 1 MiB copied and read back at 4 MiB/s: at least half a second.
        plan.bytes_per_sec = Some(4 * CHUNK as u64);
        let started = Instant::now();
        assert_eq!(move_due(&fx.db, &plan, &Archive::default()).moved, 1);
        assert!(
            started.elapsed() >= Duration::from_millis(450),
            "{:?}",
            started.elapsed()
        );
    }

    /// A whole pass: outside its hours nothing moves; inside them the
    /// archive is claimed (marker, id saved) and due recordings move; an
    /// archive whose marker went (unmounted share) is missing and untouched.
    #[tokio::test]
    async fn a_pass_respects_hours_and_the_archive_marker() {
        let fx = Fixture::new();
        let mut config = crate::config::AppConfig::default();
        config.recording.storage_path = fx.dir.join("local").to_string_lossy().into_owned();
        let archive_root = fx.dir.join("archive");
        config.recording.archive = ArchiveConfig {
            enabled: true,
            path: archive_root.to_string_lossy().into_owned(),
            after_days: 1,
            // A window that never includes now.
            hours: crate::recording::policy::local_calendar().map(|(_, minute)| {
                let at = |m: usize| format!("{:02}:{:02}", (m / 60) % 24, m % 60);
                crate::config::ArchiveHours {
                    start: at(minute + 120),
                    end: at(minute + 180),
                }
            }),
            ..ArchiveConfig::default()
        };
        let state = Arc::new(RwLock::new(AppState::new(
            config,
            fx.dir.join("config.json"),
            crate::pipeline::hwaccel::HwAccelKind::Software,
        )));
        let db = Arc::clone(&state.read().await.db);
        let old = write_recording(
            &fx.dir,
            &db,
            "cam-p",
            "2026-09-01",
            "cam-p_1.mkv",
            1000,
            1_000_000,
        );
        let archive = Arc::clone(&state.read().await.archive);

        pass(Arc::clone(&state)).await;
        assert_eq!(archive.status().state, "waiting");
        assert!(old.exists() && !archive_root.exists());

        state.write().await.config.recording.archive.hours = None;
        pass(Arc::clone(&state)).await;
        let status = archive.status();
        assert_eq!(
            (status.state, status.moved_files),
            ("idle", 1),
            "{status:?}"
        );
        assert!(!old.exists());
        assert!(archive_root
            .join(crate::storage_guard::MARKER_FILE)
            .is_file());
        let key = archive_root.to_string_lossy().into_owned();
        assert!(state
            .read()
            .await
            .config
            .recording
            .storage_ids
            .contains_key(&key));

        // The share is unmounted: an empty folder where it was.
        std::fs::remove_dir_all(&archive_root).unwrap();
        std::fs::create_dir_all(&archive_root).unwrap();
        pass(Arc::clone(&state)).await;
        assert_eq!(archive.status().state, "missing");
        assert_eq!(
            std::fs::read_dir(&archive_root).unwrap().count(),
            0,
            "nothing written"
        );
    }

    #[test]
    fn archive_paths_keep_the_recording_layout_and_refuse_odd_names() {
        let root = Path::new("/nas/archive");
        assert_eq!(
            destination(root, "cam", Path::new("/rec/cam/2026-10-01/cam_1.mkv")),
            Some(PathBuf::from("/nas/archive/cam/2026-10-01/cam_1.mkv"))
        );
        assert_eq!(
            destination(root, "cam", Path::new("/rec/cam/cam_1.mkv")),
            Some(PathBuf::from("/nas/archive/cam/cam_1.mkv"))
        );
        assert_eq!(destination(root, "..", Path::new("/rec/x/a.mkv")), None);
        assert!(overlap(Path::new("/rec/archive"), &[PathBuf::from("/rec")]).is_some());
        assert!(overlap(Path::new("/"), &[PathBuf::from("/rec")]).is_some());
        assert!(overlap(Path::new("/nas"), &[PathBuf::from("/rec")]).is_none());
    }

    #[test]
    fn rows_read_from_the_archive_folder_are_archived() {
        let fx = Fixture::new();
        let conn = fx.db.lock().unwrap();
        for path in [
            "/nas/cam/2026-10-01/a.mkv",
            "/nas/camera/2026-10-01/b.mkv",
            "/rec/cam/c.mkv",
        ] {
            conn.execute(
                "INSERT INTO segments (camera_id, start_ts, path) VALUES ('cam', 0, ?1)",
                [path],
            )
            .unwrap();
        }
        mark_archived(&conn, "cam", Path::new("/nas/cam"));
        let tiers: Vec<String> = conn
            .prepare("SELECT tier FROM segments ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(tiers, ["archive", "local", "local"]);
    }
}
