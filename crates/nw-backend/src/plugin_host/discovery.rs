//! Bounded, administrator-requested discovery. Only a signed hello is read;
//! discovery never authorizes a node or attaches a camera.
use super::{PluginServer, ServerStatus};
use futures_util::{stream, StreamExt};
use nw_analytics_protocol::{address::Address, messages::Manifest};
use serde::Serialize;
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Serialize)]
pub struct Candidate {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_as: Option<String>,
}

pub fn address_key(text: &str) -> Result<String, String> {
    Ok(
        match Address::parse(text).map_err(|error| error.to_string())? {
            Address::Unix(path) => Address::Unix(path.canonicalize().unwrap_or(path)).to_string(),
            Address::Pipe(path) => format!("pipe:{}", path.to_ascii_lowercase()),
            address => address.to_string(),
        },
    )
}

pub fn existing(
    address: &str,
    manifest: Option<&Manifest>,
    servers: &[PluginServer],
    statuses: &[ServerStatus],
) -> Option<String> {
    let key = address_key(address).ok();
    servers
        .iter()
        .find(|server| {
            key.is_some() && address_key(&server.address).ok() == key
                || manifest.is_some_and(|manifest| {
                    server.plugin_id.as_deref() == Some(manifest.id.as_str())
                        || statuses
                            .iter()
                            .find(|status| status.id == server.id)
                            .and_then(|status| status.manifest.as_ref())
                            .is_some_and(|known| known.id == manifest.id)
                        || manifest.identity.as_ref().is_some_and(|identity| {
                            server
                                .approval
                                .as_ref()
                                .is_some_and(|approval| approval.public_key == identity.public_key)
                        })
                })
        })
        .map(|server| server.id.clone())
}

pub async fn inspect(text: String) -> Result<Manifest, String> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = Arc::clone(SLOTS.get_or_init(|| Arc::new(Semaphore::new(4))))
        .try_acquire_owned()
        .map_err(|_| "Plugin discovery is busy; try again")?;
    let address = Address::parse(&text).map_err(|error| error.to_string())?;
    tokio::time::timeout(
        Duration::from_secs(12),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            super::session::inspect(&address)
        }),
    )
    .await
    .map_err(|_| "Plugin inspection timed out")?
    .map_err(|_| "Plugin inspection stopped")?
}

pub async fn discover(
    config: &Path,
    servers: &[PluginServer],
    statuses: &[ServerStatus],
) -> Vec<Candidate> {
    stream::iter(candidates(config, servers))
        .map(|address| async move {
            let (manifest, error) = match inspect(address.clone()).await {
                Ok(manifest) => (Some(manifest), None),
                Err(error) => (None, Some(error)),
            };
            let added_as = existing(&address, manifest.as_ref(), servers, statuses);
            Candidate {
                address,
                manifest,
                error,
                added_as,
            }
        })
        .buffer_unordered(4)
        .take_until(tokio::time::sleep(Duration::from_secs(8)))
        .collect::<Vec<_>>()
        .await
}

fn candidates(config: &Path, servers: &[PluginServer]) -> Vec<String> {
    let mut found = vec![];
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        // Unit tests discover only their private fixture directory, never a
        // daemon the developer has running in their real session.
        let mut directories = if cfg!(test) {
            vec![]
        } else {
            vec![runtime.join("neurowatch/plugins")]
        };
        if let Some(parent) = config.parent() {
            directories.push(parent.join("plugins"));
        }
        for directory in directories {
            if !std::fs::symlink_metadata(&directory)
                .is_ok_and(|meta| meta.is_dir() && meta.uid() == unsafe { libc::geteuid() })
            {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.take(128).flatten() {
                if entry.metadata().is_ok_and(|meta| {
                    meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() }
                }) {
                    found.push(Address::Unix(entry.path()).to_string());
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(entries) = if cfg!(test) {
            Err(std::io::Error::other("test namespace"))
        } else {
            std::fs::read_dir(r"\\.\pipe\")
        } {
            for entry in entries.take(1024).flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("nw-") || name.starts_with("neurowatch-") {
                    found.push(format!(r"pipe:\\.\pipe\{name}"));
                }
            }
        }
        if !cfg!(test) {
            found.push(r"pipe:\\.\pipe\neurowatch-motion".into());
        }
        let _ = config;
    }
    for server in servers {
        if matches!(
            Address::parse(&server.address),
            Ok(Address::Unix(_) | Address::Pipe(_))
        ) {
            found.push(server.address.clone());
        }
    }
    found.sort();
    found.dedup();
    found.truncate(32);
    found
}
