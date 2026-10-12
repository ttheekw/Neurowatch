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
    fn fresh_database_never_gets_an_s3_key_column() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        apply_migrations(&conn).expect("migrations apply");

        assert!(!has_column(&conn, "segments", "s3_key"));
        assert!(has_column(&conn, "segments", "local_deleted"));
        assert!(has_column(&conn, "segments", "timing_exact"));
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
