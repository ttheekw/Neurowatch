use anyhow::Context as _;

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
