//! Retention — delete the oldest local segments to keep a camera within its
//! quota and the recording drive under the disk-usage threshold.
//!
//! Only indexed segments (rows of the `segments` table, which the indexer
//! fills from NeuroWatch's own recording folders) are ever deleted: other
//! files on the drive are never touched. A deleted segment loses both its
//! file and its row.
//!
//! `local_deleted` marks a segment whose local file is no longer present.
//! Nothing in the runtime sets it to `1` anymore (it was previously used to
//! keep a row alive for the now-removed direct-S3 uploader's quota tracking);
//! the column remains for schema compatibility with existing databases and
//! is still honored by playback queries as a defensive filter.
//!
//! Recordings on a location that is missing now (an unmounted drive or
//! network share, `crate::storage_guard`) are left alone: their files are
//! out of reach, not gone, so their rows stay and they count as nothing freed.

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use nw_core::camera::CameraId;
use rusqlite::Connection;

/// Segments examined per database read; the lock is not held while files
/// are deleted.
const BATCH: i64 = 128;

/// What a retention pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cleanup {
    /// Segments whose file was deleted.
    pub deleted: usize,
    /// Bytes counted towards the target.
    pub freed_bytes: u64,
    /// The target was met (otherwise nothing more could be deleted).
    pub satisfied: bool,
}

/// How freed space is counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    /// Indexed sizes: a row whose file is already gone counts too.
    Index,
    /// Bytes actually freed on the drive.
    Disk,
}

struct Candidate {
    id: i64,
    start_ts: i64,
    path: String,
    size_bytes: i64,
    has_event: i64,
}

/// Which recordings a pass may delete.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scope<'a> {
    /// Only this storage tier (`local`, `archive`); every tier when `None`.
    pub tier: Option<&'a str>,
    /// Locations that are missing now: recordings under them are skipped.
    pub offline: &'a [PathBuf],
}

impl Scope<'_> {
    fn reachable(&self, path: &Path) -> bool {
        !self.offline.iter().any(|root| path.starts_with(root))
    }
}

/// The next oldest segments after `cursor` (`start_ts`, `id`), of one camera
/// or of every camera.
fn candidates(
    db: &Arc<Mutex<Connection>>,
    camera: Option<&str>,
    tier: Option<&str>,
    cursor: (i64, i64, i64),
) -> Option<Vec<Candidate>> {
    let conn = db.lock().ok()?;
    // Footage a kept bookmark covers is never a candidate.
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id, start_ts, path, COALESCE(size_bytes, 0), has_event FROM segments \
             WHERE local_deleted = 0 AND (?4 IS NULL OR camera_id = ?4) \
               AND (?6 IS NULL OR tier = ?6) \
               AND (has_event,start_ts,id) > (?1,?2,?3) AND {} \
             ORDER BY has_event ASC,start_ts ASC,id ASC LIMIT ?5",
            super::NOT_KEPT
        ))
        .ok()?;
    let rows = stmt
        .query_map(
            rusqlite::params![cursor.0, cursor.1, cursor.2, camera, BATCH, tier],
            |r| {
                Ok(Candidate {
                    id: r.get(0)?,
                    start_ts: r.get(1)?,
                    path: r.get(2)?,
                    size_bytes: r.get(3)?,
                    has_event: r.get(4)?,
                })
            },
        )
        .ok()?;
    Some(rows.flatten().collect())
}

fn remove_rows(db: &Arc<Mutex<Connection>>, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    let Ok(mut guard) = db.lock() else { return };
    let Ok(tx) = guard.transaction() else { return };
    for id in ids {
        let _ = tx.execute("DELETE FROM segments WHERE id = ?1", [id]);
    }
    if let Err(e) = tx.commit() {
        tracing::warn!(error = %e, "Cannot remove deleted segments from the index");
    }
}

/// Delete the oldest segments (of `camera`, or of every camera) until `need`
/// bytes are freed. Segments for which `keep` is true are skipped.
fn delete_oldest(
    db: &Arc<Mutex<Connection>>,
    camera: Option<&str>,
    scope: Scope<'_>,
    need: u64,
    count: Count,
    keep: impl Fn(&Path, &Metadata) -> bool,
) -> Cleanup {
    let mut done = Cleanup::default();
    let mut cursor = (i64::MIN, i64::MIN, i64::MIN);
    while done.freed_bytes < need {
        let batch = match candidates(db, camera, scope.tier, cursor) {
            Some(batch) if !batch.is_empty() => batch,
            _ => break,
        };
        let mut removed = Vec::new();
        for segment in batch {
            if done.freed_bytes >= need {
                break;
            }
            cursor = (segment.has_event, segment.start_ts, segment.id);
            if !scope.reachable(Path::new(&segment.path)) {
                continue;
            }
            let indexed_size = u64::try_from(segment.size_bytes).unwrap_or(0);
            let meta = match std::fs::metadata(&segment.path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    removed.push(segment.id);
                    if count == Count::Index {
                        done.freed_bytes += indexed_size;
                    }
                    continue;
                }
                Err(e) => {
                    tracing::debug!(path = %segment.path, error = %e, "Cannot inspect segment");
                    continue;
                }
            };
            if keep(Path::new(&segment.path), &meta) {
                continue;
            }
            match std::fs::remove_file(&segment.path) {
                Ok(()) => {
                    removed.push(segment.id);
                    done.deleted += 1;
                    done.freed_bytes += match count {
                        Count::Index => indexed_size,
                        Count::Disk => meta.len(),
                    };
                    tracing::debug!(path = %segment.path, "Old segment deleted");
                }
                Err(e) => tracing::warn!(
                    path = %segment.path,
                    error = %e,
                    "Failed to delete local segment file"
                ),
            }
        }
        remove_rows(db, &removed);
    }
    done.satisfied = done.freed_bytes >= need;
    done
}

/// A file modified after `active_after` may still be being written.
fn written_after(meta: &Metadata, active_after: Option<SystemTime>) -> bool {
    match (active_after, meta.modified()) {
        (Some(after), Ok(modified)) => modified > after,
        _ => false,
    }
}

/// Enforce the local disk quota for `cam_id`.
///
/// Only locally-present segments (`local_deleted = 0`) of the local tier
/// count toward usage (archived ones have a quota of their own,
/// [`enforce_archive`]).
/// When usage exceeds `quota_bytes`, the oldest segments (by `start_ts`) are
/// deleted — both the file and the database row — until usage is within
/// budget. Files modified after `active_after` are kept (a recording that is
/// still running writes its newest one).
pub fn enforce(
    db: &Arc<Mutex<Connection>>,
    cam_id: &CameraId,
    quota_bytes: i64,
    active_after: Option<SystemTime>,
    offline: &[PathBuf],
) -> Cleanup {
    let scope = Scope {
        tier: Some(LOCAL_TIER),
        offline,
    };
    let Some(total) = indexed_bytes(db, Some(cam_id.as_str()), LOCAL_TIER) else {
        return Cleanup::default();
    };
    let need = u64::try_from(total.saturating_sub(quota_bytes)).unwrap_or(0);
    let done = delete_oldest(
        db,
        Some(cam_id.as_str()),
        scope,
        need,
        Count::Index,
        |_, meta| written_after(meta, active_after),
    );
    if done.deleted > 0 {
        tracing::info!(
            cam_id = %cam_id,
            deleted = done.deleted,
            freed_bytes = done.freed_bytes,
            "Oldest recordings deleted (camera storage quota)"
        );
    }
    done
}

/// The storage tier of recordings in their camera's recording location.
pub const LOCAL_TIER: &str = "local";
/// The storage tier of recordings moved to the archive (`recording::archive`).
pub const ARCHIVE_TIER: &str = "archive";

/// Indexed bytes of one tier, of one camera or of every camera.
fn indexed_bytes(db: &Arc<Mutex<Connection>>, camera: Option<&str>, tier: &str) -> Option<i64> {
    let conn = db.lock().ok()?;
    conn.query_row(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM segments \
         WHERE local_deleted = 0 AND tier = ?2 AND (?1 IS NULL OR camera_id = ?1)",
        rusqlite::params![camera, tier],
        |r| r.get(0),
    )
    .ok()
}

/// Keep the archive within `quota_bytes` by deleting its oldest recordings
/// (footage without events first; never what a kept bookmark covers).
pub fn enforce_archive(
    db: &Arc<Mutex<Connection>>,
    quota_bytes: i64,
    offline: &[PathBuf],
) -> Cleanup {
    let Some(total) = indexed_bytes(db, None, ARCHIVE_TIER) else {
        return Cleanup::default();
    };
    let need = u64::try_from(total.saturating_sub(quota_bytes)).unwrap_or(0);
    let scope = Scope {
        tier: Some(ARCHIVE_TIER),
        offline,
    };
    let done = delete_oldest(db, None, scope, need, Count::Index, |_, _| false);
    if done.deleted > 0 {
        tracing::info!(
            deleted = done.deleted,
            freed_bytes = done.freed_bytes,
            "Oldest archived recordings deleted (archive size limit)"
        );
    }
    done
}

/// How full the recording drive was and what a threshold pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskCleanup {
    pub total_bytes: u64,
    /// Used before the pass (size minus space available to NeuroWatch).
    pub used_bytes: u64,
    /// Most the threshold allows.
    pub limit_bytes: u64,
    pub cleanup: Cleanup,
}

/// Keep the drive that holds `storage_root` at most `threshold_percent` full
/// by deleting the oldest recordings of any camera stored on that drive.
///
/// `space` reports `(total, available)` bytes for a path. Returns `None` when
/// the threshold is off, the drive cannot be measured, or the drive is not
/// over the threshold. Files modified after `active_after` are kept.
pub fn enforce_disk_threshold(
    db: &Arc<Mutex<Connection>>,
    storage_root: &Path,
    threshold_percent: u8,
    active_after: SystemTime,
    offline: &[PathBuf],
    space: impl Fn(&Path) -> Option<(u64, u64)>,
) -> Option<DiskCleanup> {
    if threshold_percent == 0 || threshold_percent >= 100 {
        return None;
    }
    let (total_bytes, available) = space(storage_root)?;
    let used_bytes = total_bytes.saturating_sub(available);
    let limit_bytes =
        u64::try_from(u128::from(total_bytes) * u128::from(threshold_percent) / 100).ok()?;
    if used_bytes <= limit_bytes {
        return None;
    }
    let root_meta = std::fs::metadata(storage_root).ok()?;
    let scope = Scope {
        tier: None,
        offline,
    };
    let cleanup = delete_oldest(
        db,
        None,
        scope,
        used_bytes - limit_bytes,
        Count::Disk,
        |path, meta| {
            !same_drive(storage_root, &root_meta, path, meta)
                || written_after(meta, Some(active_after))
        },
    );
    Some(DiskCleanup {
        total_bytes,
        used_bytes,
        limit_bytes,
        cleanup,
    })
}

/// Deleting `file` frees space on the drive of `root`.
#[cfg(unix)]
fn same_drive(_root_path: &Path, root: &Metadata, _file_path: &Path, file: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    root.dev() == file.dev()
}

/// Deleting `file` frees space on the drive of `root`: same volume prefix
/// (`C:`, `\\server\share`).
#[cfg(not(unix))]
fn same_drive(root_path: &Path, _root: &Metadata, file_path: &Path, _file: &Metadata) -> bool {
    fn volume(path: &Path) -> Option<std::ffi::OsString> {
        match path.components().next()? {
            std::path::Component::Prefix(prefix) => Some(prefix.as_os_str().to_ascii_lowercase()),
            _ => None,
        }
    }
    volume(root_path).is_some_and(|root| volume(file_path) == Some(root))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rusqlite::{params, Connection};

    use super::{enforce, enforce_disk_threshold};
    use crate::db::schema::apply_migrations;

    const CAM: &str = "cam-quota";

    #[test]
    fn quota_prefers_unmarked_footage_but_event_marks_do_not_override_disk_limits() {
        let fixture = Fixture::new("event-priority");
        let event = fixture.segment("event.mkv", 1000, 10, true);
        let plain = fixture.segment("plain.mkv", 2000, 10, true);
        fixture
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE segments SET has_event=1 WHERE path=?1",
                [event.to_string_lossy()],
            )
            .unwrap();
        enforce(&fixture.db, &CAM.to_owned(), 10, None, &[]);
        assert!(event.exists());
        assert!(!plain.exists());
        enforce(&fixture.db, &CAM.to_owned(), 0, None, &[]);
        assert!(!event.exists());
    }

    #[test]
    fn footage_a_kept_bookmark_covers_is_never_deleted() {
        let fixture = Fixture::new("kept");
        // Segments end 1 ms after they start; the bookmark (with 30 s either
        // side) covers only the one starting at 100 s.
        let old = fixture.segment("old.mkv", 1_000, 10, true);
        let kept = fixture.segment("kept.mkv", 100_000, 10, true);
        let later = fixture.segment("later.mkv", 200_000, 10, true);
        let conn = fixture.db.lock().unwrap();
        conn.execute(
            "INSERT INTO events (id, ts, end_ts, camera_id, event_type, source, kept) \
             VALUES (1, 120000, 125000, ?1, 'bookmark', 'user', 1)",
            [CAM],
        )
        .unwrap();
        drop(conn);
        enforce(&fixture.db, &CAM.to_owned(), 0, None, &[]);
        assert!(!old.exists() && !later.exists());
        assert!(kept.exists(), "kept footage survives an over-full quota");
        fixture
            .db
            .lock()
            .unwrap()
            .execute("UPDATE events SET kept = 0", [])
            .unwrap();
        enforce(&fixture.db, &CAM.to_owned(), 0, None, &[]);
        assert!(
            !kept.exists(),
            "released once the bookmark is no longer kept"
        );
    }

    #[test]
    fn rows_on_a_missing_location_are_kept_and_tiers_have_their_own_quotas() {
        let fixture = Fixture::new("tiers");
        // On a share that is not mounted: the file is out of reach, not gone.
        let unreachable = fixture.segment("share/old.mkv", 1_000, 10, false);
        let local = fixture.segment("local.mkv", 2_000, 10, true);
        let archived = fixture.segment("archived.mkv", 500, 10, true);
        let newer_archived = fixture.segment("archived-2.mkv", 3_000, 10, true);
        fixture
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE segments SET tier = 'archive' WHERE path LIKE '%archived%'",
                [],
            )
            .unwrap();
        let offline = [fixture.root.join("share")];
        // The local quota counts local recordings only (30 bytes with the
        // unreachable one) and skips what it cannot reach.
        let done = enforce(&fixture.db, &CAM.to_owned(), 20, None, &offline);
        assert_eq!(
            done.deleted, 0,
            "archived recordings are not the camera's to delete"
        );
        enforce(&fixture.db, &CAM.to_owned(), 0, None, &offline);
        assert!(!local.exists());
        assert!(archived.exists() && newer_archived.exists());
        let rows: i64 = fixture
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE path = ?1",
                [unreachable.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "an unreachable recording keeps its row");
        // The archive's own limit deletes its oldest.
        let done = super::enforce_archive(&fixture.db, 10, &offline);
        assert_eq!(done.deleted, 1);
        assert!(!archived.exists() && newer_archived.exists());
    }

    struct Fixture {
        root: PathBuf,
        db: Arc<Mutex<Connection>>,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("nw-quota-{tag}-{}-{now_ns}", std::process::id()));
            fs::create_dir_all(&root).expect("create temp dir");
            let conn = Connection::open_in_memory().expect("open sqlite");
            apply_migrations(&conn).expect("apply migrations");
            Self {
                root,
                db: Arc::new(Mutex::new(conn)),
            }
        }

        /// Insert a segment row; writes a local file of `size` bytes when `on_disk`.
        fn segment(&self, name: &str, start_ts: i64, size: i64, on_disk: bool) -> PathBuf {
            self.camera_segment(CAM, name, start_ts, size, on_disk)
        }

        fn camera_segment(
            &self,
            camera: &str,
            name: &str,
            start_ts: i64,
            size: i64,
            on_disk: bool,
        ) -> PathBuf {
            let path = self.root.join(name);
            if on_disk {
                fs::write(&path, vec![0u8; size as usize]).expect("write segment");
            }
            self.db
                .lock()
                .expect("db lock")
                .execute(
                    "INSERT INTO segments (camera_id, start_ts, end_ts, path, size_bytes) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![camera, start_ts, start_ts + 1, path.to_string_lossy(), size],
                )
                .expect("insert segment");
            path
        }

        /// Paths of remaining rows, ordered by `start_ts`.
        fn paths(&self) -> Vec<String> {
            let conn = self.db.lock().expect("db lock");
            let mut stmt = conn
                .prepare("SELECT path FROM segments ORDER BY start_ts")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("collect rows")
        }

        fn local_usage(&self) -> i64 {
            self.db
                .lock()
                .expect("db lock")
                .query_row(
                    "SELECT COALESCE(SUM(size_bytes), 0) FROM segments \
                     WHERE camera_id = ?1 AND local_deleted = 0",
                    [CAM],
                    |r| r.get(0),
                )
                .expect("usage")
        }

        fn enforce(&self, quota_bytes: i64) {
            enforce(&self.db, &CAM.to_owned(), quota_bytes, None, &[]);
        }

        /// A drive of `total` bytes with `available` free, at `percent`.
        fn threshold(
            &self,
            total: u64,
            available: u64,
            percent: u8,
            active_after: SystemTime,
        ) -> Option<super::DiskCleanup> {
            enforce_disk_threshold(&self.db, &self.root, percent, active_after, &[], |_| {
                Some((total, available))
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn path_str(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn under_quota_is_a_no_op() {
        let fx = Fixture::new("noop");
        let a = fx.segment("a.mkv", 100, 40, true);
        let b = fx.segment("b.mkv", 200, 60, true);

        fx.enforce(100);

        assert!(a.exists());
        assert!(b.exists());
        assert_eq!(fx.paths(), vec![path_str(&a), path_str(&b)]);
    }

    #[test]
    fn oldest_segment_is_removed_first() {
        let fx = Fixture::new("oldest-first");
        let oldest = fx.segment("oldest.mkv", 100, 50, true);
        let newer = fx.segment("newer.mkv", 200, 50, true);

        fx.enforce(50);

        assert!(!oldest.exists(), "oldest file should be deleted");
        assert!(newer.exists());
        assert_eq!(fx.paths(), vec![path_str(&newer)]);
        assert_eq!(fx.local_usage(), 50);
    }

    #[test]
    fn quota_deletion_removes_both_file_and_row() {
        let fx = Fixture::new("delete-row");
        let oldest = fx.segment("oldest.mkv", 100, 60, true);
        let newer = fx.segment("newer.mkv", 200, 60, true);

        fx.enforce(60);

        assert!(!oldest.exists(), "oldest local file should be deleted");
        assert!(newer.exists());
        assert_eq!(fx.paths(), vec![path_str(&newer)]);
    }

    #[test]
    fn already_missing_local_file_is_handled() {
        let fx = Fixture::new("missing-file");
        let gone = fx.segment("gone.mkv", 100, 70, false);
        let kept = fx.segment("kept.mkv", 200, 30, true);

        fx.enforce(30);

        assert!(kept.exists());
        assert_eq!(fx.paths(), vec![path_str(&kept)]);
        assert!(!gone.exists());
        assert_eq!(fx.local_usage(), 30);
    }

    #[test]
    fn removal_continues_until_byte_cap_is_met() {
        let fx = Fixture::new("until-cap");
        let s1 = fx.segment("s1.mkv", 100, 25, true);
        let s2 = fx.segment("s2.mkv", 200, 25, true);
        let s3 = fx.segment("s3.mkv", 300, 25, true);
        let s4 = fx.segment("s4.mkv", 400, 25, true);

        // 100 bytes used, 55 allowed: two deletions leave 50 <= 55.
        fx.enforce(55);

        assert!(!s1.exists());
        assert!(!s2.exists());
        assert!(s3.exists());
        assert!(s4.exists());
        assert_eq!(fx.paths(), vec![path_str(&s3), path_str(&s4)]);
        assert_eq!(fx.local_usage(), 50);
    }

    #[test]
    fn empty_camera_is_a_no_op() {
        let fx = Fixture::new("empty");

        fx.enforce(0);

        assert!(fx.paths().is_empty());
        assert_eq!(fx.local_usage(), 0);
    }

    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(3600)
    }

    #[test]
    fn quota_keeps_a_segment_still_being_written() {
        let fx = Fixture::new("quota-active");
        let active = fx.segment("active.mkv", 100, 80, true);

        let done = enforce(&fx.db, &CAM.to_owned(), 10, Some(UNIX_EPOCH), &[]);

        assert!(active.exists());
        assert_eq!(done.deleted, 0);
        assert!(!done.satisfied);
    }

    #[test]
    fn drive_under_the_threshold_is_left_alone() {
        let fx = Fixture::new("disk-under");
        let a = fx.segment("a.mkv", 100, 50, true);

        assert!(fx.threshold(1000, 300, 80, later()).is_none());
        assert!(a.exists());
    }

    #[test]
    fn threshold_off_never_deletes() {
        let fx = Fixture::new("disk-off");
        let a = fx.segment("a.mkv", 100, 50, true);

        assert!(fx.threshold(1000, 0, 0, later()).is_none());
        assert!(a.exists());
    }

    #[test]
    fn oldest_recordings_of_any_camera_go_first_until_under_the_threshold() {
        let fx = Fixture::new("disk-over");
        let a1 = fx.camera_segment("cam-a", "a1.mkv", 100, 60, true);
        let b1 = fx.camera_segment("cam-b", "b1.mkv", 150, 60, true);
        let a2 = fx.camera_segment("cam-a", "a2.mkv", 200, 60, true);
        let b2 = fx.camera_segment("cam-b", "b2.mkv", 250, 60, true);

        // 900 of 1000 used, 800 allowed: 100 to free, two 60-byte files.
        let pass = fx
            .threshold(1000, 100, 80, later())
            .expect("over threshold");

        assert_eq!(pass.used_bytes, 900);
        assert_eq!(pass.limit_bytes, 800);
        assert_eq!(pass.cleanup.deleted, 2);
        assert_eq!(pass.cleanup.freed_bytes, 120);
        assert!(pass.cleanup.satisfied);
        assert!(!a1.exists());
        assert!(!b1.exists());
        assert!(a2.exists());
        assert!(b2.exists());
        assert_eq!(fx.paths(), vec![path_str(&a2), path_str(&b2)]);
    }

    #[test]
    fn segments_still_being_written_are_never_deleted_for_space() {
        let fx = Fixture::new("disk-active");
        let a = fx.segment("a.mkv", 100, 60, true);

        let pass = fx
            .threshold(1000, 0, 50, UNIX_EPOCH)
            .expect("over threshold");

        assert!(a.exists());
        assert_eq!(pass.cleanup.deleted, 0);
        assert!(!pass.cleanup.satisfied);
        assert_eq!(fx.paths(), vec![path_str(&a)]);
    }

    #[test]
    fn missing_files_are_dropped_from_the_index_without_counting_as_freed() {
        let fx = Fixture::new("disk-missing");
        let gone = fx.segment("gone.mkv", 100, 500, false);
        let kept = fx.segment("kept.mkv", 200, 40, true);

        let pass = fx
            .threshold(1000, 100, 80, later())
            .expect("over threshold");

        assert!(!kept.exists());
        assert!(!gone.exists());
        assert_eq!(pass.cleanup.freed_bytes, 40);
        assert!(!pass.cleanup.satisfied);
        assert!(fx.paths().is_empty());
    }
}
