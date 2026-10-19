use anyhow::Context as _;

pub fn apply_migrations(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    // ── Schema version 2 — base tables ────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS events (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             ts           INTEGER NOT NULL,
             camera_id    TEXT    NOT NULL,
             event_type   TEXT    NOT NULL,
             thumbnail    TEXT,
             metadata     TEXT    DEFAULT '{}'
         );
         CREATE TABLE IF NOT EXISTS segments (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             camera_id    TEXT    NOT NULL,
             start_ts     INTEGER NOT NULL,
             end_ts       INTEGER,
             path         TEXT    NOT NULL,
             size_bytes   INTEGER DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_events_cam_ts   ON events   (camera_id, ts);
         CREATE INDEX IF NOT EXISTS idx_segments_cam_ts ON segments (camera_id, start_ts);
         INSERT OR IGNORE INTO schema_version VALUES (2);",
    )
    .context("Failed to apply DB migrations v2")?;

    // ── Schema version 3 (retired) — this used to add an `s3_key` column
    // for the direct S3-compatible uploader, which has been removed. New
    // databases no longer get that column. Existing databases created before
    // the removal may still carry `s3_key` and its index; that is harmless
    // and intentionally left in place rather than risking a `DROP COLUMN`
    // against production segment data. Nothing in the runtime reads or
    // writes `s3_key` anymore.

    // ── Schema version 4 — local_deleted flag on segments ────────────
    // Tracks segments whose local file has been removed by quota
    // enforcement. Kept as a general "local file missing" marker; playback
    // and quota queries filter on it regardless of any cloud backup.
    let has_local_deleted: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('segments') WHERE name = 'local_deleted'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_local_deleted {
        conn.execute_batch(
            "ALTER TABLE segments ADD COLUMN local_deleted INTEGER NOT NULL DEFAULT 0;
             INSERT OR REPLACE INTO schema_version VALUES (4);",
        )
        .context("Failed to apply DB migrations v4")?;
    }

    // ── Schema version 5 — segment lookup by path ────────────────────
    // The segment indexer looks every file up by path on each pass; without
    // this index each lookup scanned the whole table.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_segments_path ON segments (path);
         INSERT OR IGNORE INTO schema_version VALUES (5);",
    )
    .context("Failed to apply DB migrations v5")?;

    // ── Schema version 6 — segment timing read from the file ─────────
    // 1: start and end come from the segment's own timestamps and length
    // (`recording::mkv_timing`) and the file is finished; 2: settled but
    // unreadable that way (kept as indexed). 0 (older rows, a segment still
    // being written): read again on the next pass.
    let has_timing: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('segments') WHERE name = 'timing_exact'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_timing {
        conn.execute_batch(
            "ALTER TABLE segments ADD COLUMN timing_exact INTEGER NOT NULL DEFAULT 0;
             INSERT OR REPLACE INTO schema_version VALUES (6);",
        )
        .context("Failed to apply DB migrations v6")?;
    }

    // ── Schema version 7 — events ────────────────────────────────────
    // Motion, camera analytics, camera offline, bookmarks… (`crate::events`).
    // The v2 table required a camera and had no end time; nothing ever
    // wrote it, but copy any rows over rather than dropping them.
    let has_source: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'source'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_source {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE events_v7 (
                 id           INTEGER PRIMARY KEY,
                 ts           INTEGER NOT NULL,
                 end_ts       INTEGER,
                 last_ts      INTEGER,
                 camera_id    TEXT,
                 event_type   TEXT    NOT NULL,
                 source       TEXT    NOT NULL DEFAULT 'system',
                 topic        TEXT,
                 score        REAL,
                 zones        TEXT,
                 bbox         TEXT,
                 thumbnail    TEXT,
                 detail       TEXT,
                 acknowledged INTEGER NOT NULL DEFAULT 0,
                 metadata     TEXT    DEFAULT '{}'
             );
             INSERT INTO events_v7 (id, ts, end_ts, last_ts, camera_id, event_type, thumbnail, metadata)
                 SELECT id, ts, ts, ts, camera_id, event_type, thumbnail, metadata FROM events;
             DROP TABLE events;
             ALTER TABLE events_v7 RENAME TO events;
             CREATE INDEX IF NOT EXISTS idx_events_cam_ts ON events (camera_id, ts);
             CREATE INDEX IF NOT EXISTS idx_events_ts     ON events (ts);
             INSERT OR REPLACE INTO schema_version VALUES (7);
             COMMIT;",
        )
        .context("Failed to apply DB migrations v7")?;
    }

    // Event-retention is opt-in per captured interval. Existing footage starts
    // ineligible; changing a camera's mode must not purge older continuous data.
    for column in ["has_event", "event_retention_eligible"] {
        let present: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('segments') WHERE name=?1",
            [column],
            |r| r.get(0),
        )?;
        if present == 0 {
            conn.execute_batch(&format!(
                "ALTER TABLE segments ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
            ))?;
        }
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_segments_event_priority ON segments(has_event,start_ts,id) WHERE local_deleted=0;
        CREATE INDEX IF NOT EXISTS idx_segments_event_trim ON segments(camera_id,end_ts,start_ts,id) WHERE local_deleted=0 AND event_retention_eligible=1 AND has_event=0 AND timing_exact>0;
        INSERT OR IGNORE INTO schema_version VALUES (8);")?;

    // ── Schema version 9 — kept bookmarks ────────────────────────────
    // A bookmark marked "keep" protects the recordings it covers from every
    // cleanup (quota, drive threshold, event retention) and never expires.
    let has_kept: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name='kept'",
        [],
        |r| r.get(0),
    )?;
    if has_kept == 0 {
        conn.execute_batch("ALTER TABLE events ADD COLUMN kept INTEGER NOT NULL DEFAULT 0;")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_events_kept ON events(camera_id, ts) WHERE kept=1;
         INSERT OR IGNORE INTO schema_version VALUES (9);",
    )?;

    // ── Schema version 10 — storage tiers ────────────────────────────
    // `local`: in the camera's recording location; `archive`: moved to the
    // archive location (`recording::archive`), its `path` updated.
    let has_tier: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('segments') WHERE name='tier'",
        [],
        |r| r.get(0),
    )?;
    if has_tier == 0 {
        conn.execute_batch("ALTER TABLE segments ADD COLUMN tier TEXT NOT NULL DEFAULT 'local';")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_segments_tier ON segments(tier, start_ts, id) WHERE local_deleted=0;
         INSERT OR IGNORE INTO schema_version VALUES (10);",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::apply_migrations;
    use rusqlite::Connection;

    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
            [column],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    }

    #[test]
    fn event_retention_migration_never_makes_existing_footage_eligible_for_deletion() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE segments(id INTEGER PRIMARY KEY,camera_id TEXT NOT NULL,start_ts INTEGER NOT NULL,end_ts INTEGER,path TEXT NOT NULL,size_bytes INTEGER DEFAULT 0); INSERT INTO segments VALUES(1,'cam',100,200,'old.mkv',10);").unwrap();
        apply_migrations(&conn).unwrap();
        let protected: (i64, i64) = conn
            .query_row(
                "SELECT has_event,event_retention_eligible FROM segments WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(protected, (0, 0));
        apply_migrations(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM segments", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn fresh_database_never_gets_an_s3_key_column() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        apply_migrations(&conn).expect("migrations apply");

        assert!(!has_column(&conn, "segments", "s3_key"));
        assert!(has_column(&conn, "segments", "local_deleted"));
        assert!(has_column(&conn, "segments", "timing_exact"));
        assert!(has_column(&conn, "events", "source"));
        assert!(has_column(&conn, "events", "end_ts"));
    }

    #[test]
    fn v2_events_rows_survive_the_v7_rebuild() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             CREATE TABLE events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                 camera_id TEXT NOT NULL, event_type TEXT NOT NULL,
                 thumbnail TEXT, metadata TEXT DEFAULT '{}');
             INSERT INTO events (ts, camera_id, event_type) VALUES (5, 'cam', 'motion');",
        )
        .expect("seed v2 events");
        apply_migrations(&conn).expect("migrations apply");
        apply_migrations(&conn).expect("and again");
        let (ts, end, kind): (i64, i64, String) = conn
            .query_row("SELECT ts, end_ts, event_type FROM events", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .expect("row kept");
        assert_eq!((ts, end, kind.as_str()), (5, 5, "motion"));
        conn.execute(
            "INSERT INTO events (ts, event_type) VALUES (6, 'disk_low')",
            [],
        )
        .expect("events without a camera are allowed");
    }

    #[test]
    fn migrations_are_idempotent_and_preserve_existing_segment_rows() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        apply_migrations(&conn).expect("first apply");
        conn.execute(
            "INSERT INTO segments (camera_id, start_ts, end_ts, path, size_bytes) \
             VALUES ('cam-1', 100, 200, '/rec/cam-1.mkv', 42)",
            [],
        )
        .expect("insert segment");

        // Re-running migrations against an already-migrated DB must not fail
        // or touch existing rows.
        apply_migrations(&conn).expect("second apply");

        let (path, size): (String, i64) = conn
            .query_row(
                "SELECT path, size_bytes FROM segments WHERE camera_id = 'cam-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("segment row preserved");
        assert_eq!(path, "/rec/cam-1.mkv");
        assert_eq!(size, 42);
    }

    /// Simulates a database created before the direct-S3 feature removal:
    /// it already has the legacy `s3_key` column and index. Migrations must
    /// run safely against it without dropping the column or losing data.
    #[test]
    fn legacy_database_with_s3_key_column_migrates_safely() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             CREATE TABLE segments (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 camera_id    TEXT    NOT NULL,
                 start_ts     INTEGER NOT NULL,
                 end_ts       INTEGER,
                 path         TEXT    NOT NULL,
                 size_bytes   INTEGER DEFAULT 0,
                 s3_key       TEXT
             );
             CREATE INDEX idx_segments_s3 ON segments (camera_id, s3_key);
             INSERT INTO schema_version VALUES (3);
             INSERT INTO segments (camera_id, start_ts, end_ts, path, size_bytes, s3_key) \
                 VALUES ('cam-legacy', 10, 20, '/rec/legacy.mkv', 7, 'legacy/key.mkv');",
        )
        .expect("seed legacy schema");

        apply_migrations(&conn).expect("migrations apply against legacy schema");

        assert!(
            has_column(&conn, "segments", "s3_key"),
            "legacy column must be left in place, not dropped"
        );
        assert!(has_column(&conn, "segments", "local_deleted"));
        let path: String = conn
            .query_row(
                "SELECT path FROM segments WHERE camera_id = 'cam-legacy'",
                [],
                |r| r.get(0),
            )
            .expect("legacy segment row preserved");
        assert_eq!(path, "/rec/legacy.mkv");
    }
}
