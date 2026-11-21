use anyhow::Context as _;

/// The recording index: `<local data dir>/neurowatch/neurowatch.db`.
pub fn open_db() -> anyhow::Result<rusqlite::Connection> {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("neurowatch");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create DB directory: {}", dir.display()))?;
    private_dir(&dir);
    let path = dir.join("neurowatch.db");
    tracing::info!(path = %path.display(), "Opening SQLite database");
    make_private_db(&path).with_context(|| format!("Cannot create DB: {}", path.display()))?;
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

/// Make `dir` owner-only (Unix), tightening one an earlier version left loose.
/// It holds the recording index and the desktop's own logs; a failure only
/// leaves it as it was.
pub(crate) fn private_dir(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(dir = %dir.display(), error = %e, "Could not make the folder private");
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Create the SQLite file at `path` owner-only (Unix) before SQLite opens it,
/// and tighten one (and its `-wal`, `-shm` and `-journal` files) an earlier
/// version left loose. SQLite gives the files it adds later the database
/// file's own permissions. The database lists cameras, events and recordings.
pub(crate) fn make_private_db(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let name = path.as_os_str().to_owned();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut file = name.clone();
            file.push(suffix);
            match std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(all(test, unix))]
mod private_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("exists")
            .permissions()
            .mode()
            & 0o777
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nw-private-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_new_database_and_the_files_sqlite_adds_are_owner_only() {
        let dir = scratch("new");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).expect("loose");
        private_dir(&dir);
        assert_eq!(mode(&dir), 0o700);
        let path = dir.join("index.db");
        make_private_db(&path).expect("created");
        let conn = rusqlite::Connection::open(&path).expect("opens");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE t (x); INSERT INTO t VALUES (1);",
        )
        .expect("writes");
        for file in ["index.db", "index.db-wal", "index.db-shm"] {
            assert_eq!(mode(&dir.join(file)), 0o600, "{file}");
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_database_an_earlier_version_left_readable_is_tightened_and_kept() {
        let dir = scratch("old");
        let path = dir.join("index.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("opens");
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE t (x); INSERT INTO t VALUES (7);",
            )
            .expect("writes");
            // Leave -wal and -shm behind, as a running or crashed recorder does.
            std::mem::forget(conn);
        }
        for file in ["index.db", "index.db-wal", "index.db-shm"] {
            std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o644))
                .expect("loose");
        }
        make_private_db(&path).expect("tightened");
        for file in ["index.db", "index.db-wal", "index.db-shm"] {
            assert_eq!(mode(&dir.join(file)), 0o600, "{file}");
        }
        let conn = rusqlite::Connection::open(&path).expect("opens");
        let kept: i64 = conn
            .query_row("SELECT x FROM t", [], |row| row.get(0))
            .expect("kept");
        assert_eq!(kept, 7);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
