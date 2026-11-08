//! Durable drawing history. Producers only try a bounded queue; SQLite and
//! readers run separately, so a slow disk cannot stall plugin media or cameras.
use nw_analytics_protocol::messages::OverlayItem;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::{mpsc, Arc, Mutex};

const QUEUE: usize = 256;
pub const PAGE: usize = 128;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_DATABASE_PAGES: u32 = 131_072; // 512 MiB at the default 4 KiB page size

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("drawing database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("drawing writer: {0}")]
    Io(#[from] std::io::Error),
    #[error("drawing database is unavailable")]
    Unavailable,
    #[error("drawing history could not be serialized: {0}")]
    Json(#[from] serde_json::Error),
    #[error("drawing export was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawingRecord {
    pub id: i64,
    pub camera_id: String,
    pub plugin: String,
    pub layer: String,
    pub frame: Option<u64>,
    pub time_us: i64,
    pub ttl_ms: u64,
    pub on_wall: bool,
    pub items: Vec<OverlayItem>,
}

pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS analytics_drawings (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            camera_id TEXT NOT NULL,
            plugin TEXT NOT NULL,
            layer TEXT NOT NULL,
            frame TEXT,
            time_us INTEGER NOT NULL,
            ttl_ms INTEGER NOT NULL,
            on_wall INTEGER NOT NULL,
            items TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS analytics_drawings_camera_time
          ON analytics_drawings(camera_id, time_us, id);",
    )
}

pub struct History {
    db: Arc<Mutex<Connection>>,
    writer: mpsc::SyncSender<Write>,
}

enum Write {
    Drawing(DrawingRecord),
    Flush(mpsc::Sender<()>),
}

impl History {
    pub fn new(reference: Arc<Mutex<Connection>>) -> Result<Self, HistoryError> {
        // Give history its own size-capped database. Its limit must never
        // constrain the recording index. In-memory test databases stay local.
        let path = reference
            .lock()
            .map_err(|_| HistoryError::Unavailable)?
            .path()
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from);
        let db = match path {
            Some(path) => {
                let destination = if path.file_name().is_some_and(|name| name == "neurowatch.db") {
                    path.with_file_name("overlays.db")
                } else {
                    path.with_extension("overlays.db")
                };
                let conn = Connection::open(destination)?;
                conn.pragma_update(None, "max_page_count", MAX_DATABASE_PAGES)?;
                conn.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)?;
                conn.pragma_update(None, "synchronous", "NORMAL")?;
                Arc::new(Mutex::new(conn))
            }
            None => reference,
        };
        {
            let conn = db.lock().map_err(|_| HistoryError::Unavailable)?;
            create_schema(&conn)?;
        }
        let (writer, input) = mpsc::sync_channel(QUEUE);
        let connection = Arc::clone(&db);
        std::thread::Builder::new()
            .name("nw-drawings-db".into())
            .spawn(move || {
                while let Ok(write) = input.recv() {
                    match write {
                        Write::Drawing(drawing) => {
                        let written = connection.lock().ok().and_then(|conn| {
                            match insert(&conn,&drawing) {
                                Ok(()) => Some(()),
                                Err(rusqlite::Error::SqliteFailure(error,_)) if error.code == rusqlite::ErrorCode::DiskFull => {
                                    // Reuse pages from the oldest bounded batch. If the
                                    // disk itself is full, drop history and keep cameras running.
                                    conn.execute("DELETE FROM analytics_drawings WHERE id IN (SELECT id FROM analytics_drawings ORDER BY id LIMIT 4096)",[]).ok()?;
                                    insert(&conn,&drawing).ok()
                                }
                                Err(_) => None,
                            }
                        });
                            if written.is_none() {
                                tracing::warn!("Could not store an analytics drawing");
                            }
                        }
                        Write::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self { db, writer })
    }

    /// A falling-behind history writer loses its own drawings, never media.
    pub fn offer(&self, drawing: DrawingRecord) -> bool {
        if serde_json::to_vec(&drawing).map_or(true, |bytes| bytes.len() > MAX_RECORD_BYTES) {
            return false;
        }
        self.writer.try_send(Write::Drawing(drawing)).is_ok()
    }

    pub fn flush(&self) -> bool {
        let (done, wait) = mpsc::channel();
        self.writer.try_send(Write::Flush(done)).is_ok()
            && wait.recv_timeout(std::time::Duration::from_secs(5)).is_ok()
    }

    pub fn list(
        &self,
        camera: &str,
        from_us: i64,
        to_us: i64,
        after: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<DrawingRecord>, String> {
        let conn = self
            .db
            .lock()
            .map_err(|_| "Drawing history is unavailable".to_owned())?;
        list(&conn, camera, from_us, to_us, after, limit).map_err(|e| e.to_string())
    }

    /// Prune by a snapshot of the recording index, without ever holding its
    /// lock while the drawing database is written.
    pub fn prune(
        &self,
        earliest: &std::collections::HashMap<String, i64>,
    ) -> Result<usize, HistoryError> {
        let conn = self.db.lock().map_err(|_| HistoryError::Unavailable)?;
        let mut total = 0;
        let cameras: Vec<String> = {
            let mut query = conn.prepare("SELECT DISTINCT camera_id FROM analytics_drawings")?;
            let rows = query.query_map([], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for camera in cameras {
            let before = earliest
                .get(&camera)
                .copied()
                .unwrap_or_else(|| crate::events::now_ms().saturating_sub(86_400_000))
                .saturating_mul(1000)
                .saturating_sub(10_000_000);
            total += conn.execute(
                "DELETE FROM analytics_drawings WHERE camera_id=?1 AND time_us<?2",
                params![camera, before],
            )?;
        }
        Ok(total)
    }

    /// A streaming JSON Lines sidecar in capture time. Rows never require the
    /// video to be decoded or altered. The footer reports the bounded size cap.
    pub fn write_sidecar(
        &self,
        path: &std::path::Path,
        camera: &str,
        start_ms: i64,
        end_ms: i64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<bool, HistoryError> {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        if !self.flush() {
            return Err(HistoryError::Unavailable);
        }
        if self
            .list(
                camera,
                start_ms.saturating_mul(1000),
                end_ms.saturating_mul(1000),
                None,
                1,
            )
            .map_err(|_| HistoryError::Unavailable)?
            .is_empty()
        {
            return Ok(false);
        }
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Drawing sidecar already exists",
            )
            .into());
        }
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let part = std::path::PathBuf::from(part);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&part)?;
        let result = (|| {
            let mut output = std::io::BufWriter::new(file);
            serde_json::to_writer(
                &mut output,
                &serde_json::json!({"format":"nw.analytics.v1","camera_id":camera,"start_ms":start_ms,"end_ms":end_ms,"clock":"utc_microseconds"}),
            )?;
            output.write_all(b"\n")?;
            let mut cursor = None;
            let mut written = 0usize;
            let mut bytes = 0usize;
            let mut truncated = false;
            'pages: loop {
                let rows = self
                    .list(
                        camera,
                        start_ms.saturating_mul(1000),
                        end_ms.saturating_mul(1000),
                        cursor,
                        PAGE,
                    )
                    .map_err(|_| HistoryError::Unavailable)?;
                let more = rows.len() == PAGE;
                for row in rows {
                    if cancel.load(Ordering::Acquire) {
                        return Err(HistoryError::Cancelled);
                    }
                    let line = serde_json::to_vec(&row)?;
                    if bytes + line.len() > 64 * 1024 * 1024 {
                        truncated = true;
                        break 'pages;
                    }
                    output.write_all(&line)?;
                    output.write_all(b"\n")?;
                    cursor = Some((row.time_us, row.id));
                    written += 1;
                    bytes += line.len() + 1;
                }
                if !more {
                    break;
                }
            }
            serde_json::to_writer(
                &mut output,
                &serde_json::json!({"end":true,"drawings":written,"truncated":truncated}),
            )?;
            output.write_all(b"\n")?;
            output.flush()?;
            output.get_ref().sync_all()?;
            if cancel.load(Ordering::Acquire) {
                return Err(HistoryError::Cancelled);
            }
            std::fs::rename(&part, path)?;
            Ok(truncated)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(part);
        }
        result
    }
}

fn insert(conn: &Connection, drawing: &DrawingRecord) -> rusqlite::Result<()> {
    let items = serde_json::to_string(&drawing.items)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute("INSERT INTO analytics_drawings(camera_id,plugin,layer,frame,time_us,ttl_ms,on_wall,items) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![drawing.camera_id,drawing.plugin,drawing.layer,drawing.frame.map(|seq|seq.to_string()),drawing.time_us,drawing.ttl_ms.min(10000),drawing.on_wall,items])?;
    Ok(())
}

/// Includes up to 10 s before `from_us`: a drawing can hold into the range.
pub fn list(
    conn: &Connection,
    camera: &str,
    from_us: i64,
    to_us: i64,
    after: Option<(i64, i64)>,
    limit: usize,
) -> rusqlite::Result<Vec<DrawingRecord>> {
    let (after_us, after_id) = after.unwrap_or((i64::MIN, i64::MIN));
    let mut query = conn.prepare(
        "SELECT id,camera_id,plugin,layer,frame,time_us,ttl_ms,on_wall,items
       FROM analytics_drawings WHERE camera_id=?1 AND time_us>=?2 AND time_us<=?3
       AND (time_us>?4 OR (time_us=?4 AND id>?5)) ORDER BY time_us,id LIMIT ?6",
    )?;
    let rows = query.query_map(
        params![
            camera,
            from_us.saturating_sub(10_000_000),
            to_us,
            after_us,
            after_id,
            limit.clamp(1, PAGE) as i64
        ],
        |row| {
            let text: String = row.get(8)?;
            let items = serde_json::from_str(&text).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let frame: Option<String> = row.get(4)?;
            Ok(DrawingRecord {
                id: row.get(0)?,
                camera_id: row.get(1)?,
                plugin: row.get(2)?,
                layer: row.get(3)?,
                frame: frame.and_then(|seq| seq.parse().ok()),
                time_us: row.get(5)?,
                ttl_ms: row.get(6)?,
                on_wall: row.get(7)?,
                items,
            })
        },
    )?;
    rows.collect()
}

/// Recording cleanup calls this after removing footage: keep no drawing older
/// than that camera's earliest remaining segment (including archived footage).
pub fn recording_starts(
    conn: &Connection,
) -> rusqlite::Result<std::collections::HashMap<String, i64>> {
    let mut query = conn.prepare(
        "SELECT camera_id,MIN(start_ts) FROM segments WHERE local_deleted=0 GROUP BY camera_id",
    )?;
    let rows = query.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_analytics_protocol::messages::Style;

    fn record(camera: &str, time_us: i64) -> DrawingRecord {
        DrawingRecord {
            id: 0,
            camera_id: camera.into(),
            plugin: "motion".into(),
            layer: "boxes".into(),
            frame: Some(u64::MAX),
            time_us,
            ttl_ms: 1500,
            on_wall: true,
            items: vec![OverlayItem::Box {
                rect: [0.1, 0.2, 0.3, 0.4],
                label: Some("Motion".into()),
                style: Style::Accent,
                fill: false,
            }],
        }
    }

    #[test]
    fn drawings_survive_reload_and_page_without_crossing_cameras() {
        let db = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        create_schema(&db.lock().unwrap()).unwrap();
        let history = History::new(Arc::clone(&db)).unwrap();
        assert!(history.offer(record("a", 10_000_000)));
        assert!(history.offer(record("b", 10_000_000)));
        assert!(history.offer(record("a", 10_000_000)));
        assert!(history.offer(record("a", 11_000_000)));
        assert!(history.flush());
        drop(history);
        let history = History::new(Arc::clone(&db)).unwrap();
        let first = history.list("a", 10_000_000, 12_000_000, None, 2).unwrap();
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|d| d.camera_id == "a"));
        assert_eq!(first[0].frame, Some(u64::MAX));
        assert_eq!(first[0].items, record("a", 0).items);
        let last = first.last().unwrap();
        let next = history
            .list(
                "a",
                10_000_000,
                12_000_000,
                Some((last.time_us, last.id)),
                2,
            )
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].time_us, 11_000_000);
        assert!(history
            .list("missing", 0, i64::MAX, None, PAGE)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn history_includes_drawings_held_into_a_playback_window() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        insert(&conn, &record("a", 1_000_000)).unwrap();
        insert(&conn, &record("a", 20_000_000)).unwrap();
        let shown = list(&conn, "a", 2_000_000, 3_000_000, None, PAGE).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].time_us, 1_000_000);
    }

    #[test]
    fn sidecar_contains_capture_times_and_never_overwrites_an_existing_file() {
        let db = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let history = History::new(db).unwrap();
        assert!(history.offer(record("a", 1_000_000)));
        let path = std::env::temp_dir().join(format!("nw-drawings-{}.jsonl", uuid::Uuid::new_v4()));
        let cancel = std::sync::atomic::AtomicBool::new(false);
        assert!(!history
            .write_sidecar(&path, "a", 1000, 2000, &cancel)
            .unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["format"], "nw.analytics.v1");
        assert_eq!(rows[1]["time_us"], 1_000_000);
        assert_eq!(rows[2]["drawings"], 1);
        assert!(history
            .write_sidecar(&path, "a", 1000, 2000, &cancel)
            .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_file(&path).unwrap();
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert!(history
            .write_sidecar(&path, "a", 1000, 2000, &cancel)
            .is_err());
        assert!(!path.exists());
        assert!(!std::path::PathBuf::from(format!("{}.part", path.display())).exists());
    }

    #[test]
    fn file_history_has_its_own_capped_database() {
        let dir = std::env::temp_dir().join(format!("nw-history-db-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let db = Arc::new(Mutex::new(
            Connection::open(dir.join("neurowatch.db")).unwrap(),
        ));
        let history = History::new(Arc::clone(&db)).unwrap();
        assert!(dir.join("overlays.db").is_file());
        assert_eq!(
            history
                .db
                .lock()
                .unwrap()
                .query_row::<u32, _, _>("PRAGMA max_page_count", [], |row| row.get(0))
                .unwrap(),
            MAX_DATABASE_PAGES
        );
        assert_eq!(
            db.lock()
                .unwrap()
                .query_row::<u32, _, _>(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='analytics_drawings'",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
            0
        );
        assert!(history.offer(record("a", 0)));
        assert!(history.flush());
        drop(history);
        drop(db);
        // The writer observes channel closure asynchronously.
        let _ = std::fs::remove_dir_all(dir);
    }
}
