//! Credentials are scoped to a pinned key, outside the public configuration.
use std::path::{Path, PathBuf};
pub const PURPOSE: &[u8] = b"NeuroWatch analytics vendor credential v1";
pub fn path(config: &Path, server: &str, key: &str) -> Result<PathBuf, String> {
    let fingerprint = nw_analytics_sdk::identity::fingerprint(key).ok_or("Invalid plugin key")?;
    Ok(config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("plugins")
        .join(format!("{server}-{fingerprint}.credential")))
}
pub fn forget(config: &Path, server: &str, key: &str) -> Result<(), String> {
    match std::fs::remove_file(path(config, server, key)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Plugin settings were saved, but its stored credential could not be removed: {error}"
        )),
    }
}
