use anyhow::Context as _;

/// The recording index: `<local data dir>/neurowatch/neurowatch.db`.
pub fn open_db() -> anyhow::Result<rusqlite::Connection> {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neurowatch");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create DB directory: {}", dir.display()))?;
    let path = dir.join("neurowatch.db");
    tracing::info!(path = %path.display(), "Opening SQLite database");
    let conn = rusqlite::Connection::open(&path)
        .with_context(|| format!("Cannot open DB: {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("Failed to enable WAL mode")?;
    crate::db::schema::apply_migrations(&conn)?;
    Ok(conn)
}

/// An empty in-memory database with the schema, private to its connection.
/// For tests: they must never open the user's real database, and tests that
/// run in parallel must not share one file (enabling WAL on a shared file
/// fails with "database is locked").
pub fn open_in_memory() -> anyhow::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_in_memory().context("Cannot open in-memory DB")?;
    crate::db::schema::apply_migrations(&conn)?;
    Ok(conn)
}
