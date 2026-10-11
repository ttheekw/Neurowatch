//! Segment tracker — scans a camera's recording directory and inserts/updates
//! rows in the `segments` SQLite table.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use nw_core::camera::CameraId;
use rusqlite::Connection;

const MATROSKA_EBML_HEADER: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];

fn is_matroska_segment(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0_u8; 4];
    file.read_exact(&mut header).is_ok() && header == MATROSKA_EBML_HEADER
}

/// Derive the start time from the splitmux filename when possible.
///
/// New recordings use `{camera_id}_{run_epoch_ms}_{run_sequence}_{fragment}.mkv`.
/// Using the run epoch keeps short fragments (for example, a pipeline that was
/// restarted after three seconds) from being presented as a full 60-second
/// recording on the playback timeline. Older filenames retain the duration
/// based fallback.
fn segment_start_ts(filename: &str, camera_id: &str, end_ts: i64, segment_duration_ms: i64) -> i64 {
    let fallback = if segment_duration_ms > 0 {
        end_ts.saturating_sub(segment_duration_ms)
    } else {
        end_ts
    };
    if segment_duration_ms <= 0 {
        return fallback;
    }

    let Some(encoded) = filename
        .strip_prefix(&format!("{camera_id}_"))
        .and_then(|name| name.strip_suffix(".mkv"))
    else {
        return fallback;
    };
    let mut parts = encoded.rsplitn(3, '_');
    let Some(fragment) = parts.next().and_then(|value| value.parse::<i64>().ok()) else {
        return fallback;
    };
    // The middle component is a process-local sequence used to avoid filename
    // collisions. Validate it so legacy names are not mistaken for this format.
    if parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .is_none()
    {
        return fallback;
    }
    let Some(run_epoch_ms) = parts.next().and_then(|value| value.parse::<i64>().ok()) else {
        return fallback;
    };

    let nominal_start = run_epoch_ms.saturating_add(fragment.saturating_mul(segment_duration_ms));
    if run_epoch_ms <= 0 || fragment < 0 || nominal_start >= end_ts {
        return fallback;
    }
    nominal_start
}

/// Scan `storage_dir` for `.mkv` files belonging to `cam_id` (named
/// `{cam_id}_*.mkv`) and ensure each has a row in the `segments` table.
///
/// Idempotent — re-running after files have already been inserted only updates
/// the `size_bytes` / `end_ts` of existing rows.
pub fn scan_and_record(
    db: &Arc<Mutex<Connection>>,
    cam_id: &CameraId,
    storage_dir: &Path,
    segment_duration_ms: i64,
) {
    let entries = match std::fs::read_dir(storage_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(
                cam_id = %cam_id,
                dir = %storage_dir.display(),
                error = %e,
                "Cannot read recording directory"
            );
            return;
        }
    };

    let prefix = format!("{}_", cam_id);
    let mut guard = match db.lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    // One transaction per folder: a commit per file made a periodic rescan of
    // thousands of unchanged segments cost thousands of fsyncs.
    let conn = match guard.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!(cam_id = %cam_id, error = %e, "Cannot start segment index transaction");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !fname.starts_with(&prefix) || !fname.ends_with(".mkv") {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        if !is_matroska_segment(&path) {
            let removed = conn
                .execute("DELETE FROM segments WHERE path = ?1", [&path_str])
                .unwrap_or(0);
            if removed > 0 {
                tracing::warn!(
                    cam_id = %cam_id,
                    path = %path_str,
                    "Removed invalid recording segment from the playback index"
                );
            } else {
                tracing::debug!(
                    cam_id = %cam_id,
                    path = %path_str,
                    "Ignoring file with .mkv extension but no Matroska header"
                );
            }
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let size_bytes = meta.len() as i64;
        let end_ts: i64 = meta
            .modified()
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64
            })
            .unwrap_or(0);
        let start_ts = segment_start_ts(fname, cam_id, end_ts, segment_duration_ms);

        // Refresh an existing row only if the file changed (a segment still
        // being written grows between scans); insert otherwise.
        let known = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM segments WHERE path = ?1)",
                [&path_str],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if known {
            let _ = conn.execute(
                "UPDATE segments SET size_bytes = ?1, start_ts = ?2, end_ts = ?3 \
                 WHERE path = ?4 \
                 AND (size_bytes IS NOT ?1 OR start_ts IS NOT ?2 OR end_ts IS NOT ?3)",
                rusqlite::params![size_bytes, start_ts, end_ts, &path_str],
            );
        } else {
            let _ = conn.execute(
                "INSERT INTO segments \
                 (camera_id, start_ts, end_ts, path, size_bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![cam_id.as_str(), start_ts, end_ts, &path_str, size_bytes],
            );
            tracing::info!(cam_id = %cam_id, path = %path_str, size_bytes, "Segment registered");
        }
    }
    if let Err(e) = conn.commit() {
        tracing::warn!(cam_id = %cam_id, error = %e, "Cannot commit segment index updates");
    }
}

/// Scan all known local recording folders for a camera.
///
/// Expected layout:
///   <base>/<camera_id>/<YYYY-MM-DD>/*.mkv
/// but this also scans `<base>/<camera_id>` directly for compatibility.
pub fn scan_camera_tree(
    db: &Arc<Mutex<Connection>>,
    cam_id: &CameraId,
    camera_root: &Path,
    segment_duration_ms: i64,
) {
    // Backward-compatible: scan camera root for any directly stored files.
    scan_and_record(db, cam_id, camera_root, segment_duration_ms);

    let entries = match std::fs::read_dir(camera_root) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            scan_and_record(db, cam_id, &path, segment_duration_ms);
        }
    }
}

/// Like [`scan_camera_tree`], but only date folders named `since_day`
/// (`YYYY-MM-DD`) or later, plus files directly under the camera root. Lets a
/// frequent caller pick up new segments without re-reading the whole archive.
pub fn scan_camera_tree_since(
    db: &Arc<Mutex<Connection>>,
    cam_id: &CameraId,
    camera_root: &Path,
    segment_duration_ms: i64,
    since_day: &str,
) {
    scan_and_record(db, cam_id, camera_root, segment_duration_ms);
    let Ok(entries) = std::fs::read_dir(camera_root) else {
        return;
    };
    for entry in entries.flatten() {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && entry.file_name().to_string_lossy().as_ref() >= since_day {
            scan_and_record(db, cam_id, &entry.path(), segment_duration_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use super::{scan_and_record, scan_camera_tree, scan_camera_tree_since, segment_start_ts};

    #[test]
    fn splitmux_filename_preserves_the_duration_of_short_fragments() {
        let cam_id = "camera_with_underscores";
        let start = segment_start_ts(
            "camera_with_underscores_1789659035708_17_00000.mkv",
            cam_id,
            1_789_659_039_223,
            60_000,
        );

        assert_eq!(start, 1_789_659_035_708);
    }

    #[test]
    fn legacy_filename_uses_configured_duration() {
        assert_eq!(
            segment_start_ts("cam-test_00000.mkv", "cam-test", 120_000, 60_000),
            60_000
        );
    }

    #[test]
    fn scan_camera_tree_indexes_dated_subdirs() {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let tmp_root = std::env::temp_dir().join(format!("nw-segscan-{now_ns}"));
        let cam_id = "cam-test-1".to_owned();
        let cam_root = tmp_root.join(&cam_id);
        let dated = cam_root.join("2026-03-07");
        fs::create_dir_all(&dated).expect("create dirs");

        let file_path = dated.join(format!("{cam_id}_00000.mkv"));
        let mut f = File::create(&file_path).expect("create segment");
        let _ = f.write_all(&[0x1a, 0x45, 0xdf, 0xa3]);
        let _ = f.write_all(b"dummy");

        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "\
            CREATE TABLE segments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                camera_id TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER,
                path TEXT NOT NULL,
                size_bytes INTEGER DEFAULT 0,
                local_deleted INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("create schema");
        let db = Arc::new(Mutex::new(conn));

        scan_camera_tree(&db, &cam_id, &cam_root, 60_000);

        let count: i64 = db
            .lock()
            .expect("db lock")
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE camera_id = ?1",
                [&cam_id],
                |r| r.get(0),
            )
            .expect("count rows");
        assert_eq!(count, 1);

        let _ = fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn scan_camera_tree_rejects_mislabeled_mp4_and_removes_stale_row() {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let tmp_root = std::env::temp_dir().join(format!("nw-segscan-invalid-{now_ns}"));
        let cam_id = "cam-invalid".to_owned();
        let cam_root = tmp_root.join(&cam_id);
        fs::create_dir_all(&cam_root).expect("create dirs");

        let file_path = cam_root.join(format!("{cam_id}_00000.mkv"));
        fs::write(&file_path, b"\0\0\0\x18ftypmp42").expect("write invalid segment");

        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "\
            CREATE TABLE segments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                camera_id TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER,
                path TEXT NOT NULL,
                size_bytes INTEGER DEFAULT 0,
                local_deleted INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("create schema");
        conn.execute(
            "INSERT INTO segments (camera_id, start_ts, end_ts, path, size_bytes) \
             VALUES (?1, 0, 1, ?2, 12)",
            rusqlite::params![&cam_id, file_path.to_string_lossy().as_ref()],
        )
        .expect("insert stale row");
        let db = Arc::new(Mutex::new(conn));

        scan_camera_tree(&db, &cam_id, &cam_root, 60_000);

        let count: i64 = db
            .lock()
            .expect("db lock")
            .query_row("SELECT COUNT(*) FROM segments", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(count, 0);
        assert!(
            file_path.exists(),
            "scanner must not delete user recordings"
        );

        let _ = fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn a_recent_scan_skips_older_date_folders() {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let cam_root = std::env::temp_dir()
            .join(format!("nw-segscan-recent-{now_ns}"))
            .join("cam-r");
        for day in ["2026-03-06", "2026-03-07"] {
            let dated = cam_root.join(day);
            fs::create_dir_all(&dated).expect("create dirs");
            let mut file =
                File::create(dated.join(format!("cam-r_{day}_00000.mkv"))).expect("segment");
            file.write_all(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0])
                .expect("matroska magic");
        }
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch(
            "CREATE TABLE segments (id INTEGER PRIMARY KEY, camera_id TEXT NOT NULL,
             start_ts INTEGER NOT NULL, end_ts INTEGER, path TEXT NOT NULL UNIQUE,
             size_bytes INTEGER DEFAULT 0, local_deleted INTEGER NOT NULL DEFAULT 0);",
        )
        .expect("schema");
        let db = Arc::new(Mutex::new(conn));

        scan_camera_tree_since(&db, &"cam-r".to_owned(), &cam_root, 60_000, "2026-03-07");

        let paths: Vec<String> = db
            .lock()
            .unwrap()
            .prepare("SELECT path FROM segments")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert!(paths[0].contains("2026-03-07"));
        let _ = fs::remove_dir_all(cam_root.parent().unwrap());
    }

    #[test]
    fn rescanning_keeps_one_row_per_segment_and_tracks_growth() {
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nw-segscan-rescan-{now_ns}"));
        fs::create_dir_all(&dir).expect("create dir");
        let segment = dir.join("cam-g_00000.mkv");
        let mut file = File::create(&segment).expect("segment");
        file.write_all(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0])
            .expect("matroska magic");
        let conn = Connection::open_in_memory().expect("db");
        crate::db::schema::apply_migrations(&conn).expect("schema");
        let db = Arc::new(Mutex::new(conn));
        let cam_id = "cam-g".to_owned();
        let rows = |db: &Arc<Mutex<Connection>>| -> Vec<i64> {
            db.lock()
                .unwrap()
                .prepare("SELECT size_bytes FROM segments")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };

        scan_and_record(&db, &cam_id, &dir, 60_000);
        scan_and_record(&db, &cam_id, &dir, 60_000);
        assert_eq!(rows(&db), vec![8]);

        file.write_all(&[0; 8]).expect("grow segment");
        drop(file);
        scan_and_record(&db, &cam_id, &dir, 60_000);
        assert_eq!(rows(&db), vec![16]);
        let _ = fs::remove_dir_all(&dir);
    }
}
