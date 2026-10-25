//! Recording locations that might not be there.
//!
//! The classic failure of recording to a drive or network share: it is not
//! mounted (after a reboot, a network outage), the mount point is an empty
//! folder on the local disk, and recordings silently fill that disk instead.
//! Every recording location (and the archive) therefore carries a marker
//! file, [`MARKER_FILE`], holding a random id that the config remembers
//! (`recording.storage_ids`). Recordings are written, and old ones cleaned
//! up, only while the marker with that id is there.
//!
//! A location is missing when:
//! - its marker is gone, or holds another id (another drive is mounted there);
//! - it lies under a mount point listed in `/etc/fstab` that is not mounted
//!   (this also protects a location's first use, before it has a marker);
//! - checking it takes longer than [`CHECK_TIMEOUT`] (a hung network mount).
//!
//! A plain folder whose marker was deleted looks exactly like an empty mount
//! point, so nothing is re-marked automatically: the user confirms that the
//! folder is the right one ([`use_location`], IPC `recording.use_location`).
//!
//! Checks touch the file system, so they run off the async runtime and the
//! state lock; [`StorageGuard`] caches each location's last result for the
//! decisions that must not wait (the recording scheduler, cleanup).

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The marker file at the top of a recording location.
pub const MARKER_FILE: &str = ".neurowatch-storage";
/// A location that does not answer within this long is treated as missing.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const MARKER_HEADER: &str = "NeuroWatch recording location. NeuroWatch records here only while \
this file is present; do not delete it.";
/// Markers are tiny; anything bigger is not ours.
const MARKER_MAX_BYTES: u64 = 4096;

/// Whether a location can be recorded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Missing(String),
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ready => None,
            Self::Missing(reason) => Some(reason),
        }
    }
}

/// The id in `root`'s marker, if it has one.
fn marker_id(root: &Path) -> std::io::Result<Option<String>> {
    let path = root.join(MARKER_FILE);
    let meta = std::fs::metadata(&path)?;
    if !meta.is_file() || meta.len() > MARKER_MAX_BYTES {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .find_map(|line| line.strip_prefix("id="))
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty()))
}

fn write_marker(root: &Path, id: &str) -> std::io::Result<()> {
    let path = root.join(MARKER_FILE);
    let temporary = root.join(format!("{MARKER_FILE}.tmp"));
    {
        let mut file = std::fs::File::create(&temporary)?;
        writeln!(file, "{MARKER_HEADER}")?;
        writeln!(file, "id={id}")?;
        file.sync_all()?;
    }
    std::fs::rename(temporary, path)
}

/// Make `root` a recording location: create it if needed and give it a
/// marker (keeping one it already has). Returns its id. Refused under an
/// unmounted mount point: that folder is on the wrong disk.
pub fn claim(root: &Path) -> Result<String, String> {
    if let Some(mount) = unmounted_mount_point(root) {
        return Err(not_mounted(&mount));
    }
    std::fs::create_dir_all(root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    if let Ok(Some(id)) = marker_id(root) {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    write_marker(root, &id)
        .map_err(|error| format!("cannot write to {}: {error}", root.display()))?;
    Ok(id)
}

/// Whether `root` is the location whose marker holds `expected`.
pub fn check(root: &Path, expected: &str) -> Readiness {
    if let Some(mount) = unmounted_mount_point(root) {
        return Readiness::Missing(not_mounted(&mount));
    }
    match marker_id(root) {
        Ok(Some(id)) if id == expected => Readiness::Ready,
        Ok(Some(_)) => Readiness::Missing(format!(
            "a different drive is mounted at {} (its NeuroWatch marker does not match)",
            root.display()
        )),
        Ok(None) => Readiness::Missing(gone(root)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Readiness::Missing(gone(root))
        }
        Err(error) => Readiness::Missing(format!("cannot read {}: {error}", root.display())),
    }
}

/// Make `root` a recording location again on the user's word that it is the
/// right folder (its marker was deleted, or the folder restored without it).
/// Keeps `expected`, the id the config remembers, so nothing else changes; a
/// marker already there (another drive the user now wants to record to) is
/// kept and its id returned. Still refused under an unmounted mount point,
/// which is certainly the wrong disk, and when the folder does not exist.
pub fn reclaim(root: &Path, expected: Option<&str>) -> Result<String, String> {
    if let Some(mount) = unmounted_mount_point(root) {
        return Err(not_mounted(&mount));
    }
    if !root.is_dir() {
        return Err(gone(root));
    }
    let present = match marker_id(root) {
        Ok(present) => present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read {}: {error}", root.display())),
    };
    if let Some(id) = present {
        return Ok(id);
    }
    let id = expected
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    write_marker(root, &id)
        .map_err(|error| format!("cannot write to {}: {error}", root.display()))?;
    Ok(id)
}

fn gone(root: &Path) -> String {
    format!(
        "{} is not there: is its drive or network share connected and mounted?",
        root.display()
    )
}

fn not_mounted(mount: &Path) -> String {
    format!(
        "{} is not mounted: connect its drive or network share",
        mount.display()
    )
}

/// The `/etc/fstab` mount point `path` lies under (itself or an ancestor,
/// never `/`) when that mount point is not mounted now.
pub fn unmounted_mount_point(path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let fstab = std::fs::read_to_string("/etc/fstab").ok()?;
        let mounted = std::fs::read_to_string("/proc/self/mounts").ok()?;
        unmounted_in(path, &fstab, &mounted)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

/// [`unmounted_mount_point`] for the given `/etc/fstab` and mount table.
#[cfg(any(target_os = "linux", test))]
fn unmounted_in(path: &Path, fstab: &str, mounted: &str) -> Option<PathBuf> {
    let table: Vec<String> = crate::storage_drives::parse_mounts(mounted)
        .into_iter()
        .map(|mount| mount.mount_point)
        .collect();
    let lines: String = fstab
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    crate::storage_drives::parse_mounts(&lines)
        .into_iter()
        .filter(|entry| entry.filesystem != "swap" && entry.mount_point.starts_with('/'))
        .map(|entry| PathBuf::from(entry.mount_point))
        .filter(|mount| mount != Path::new("/") && path.starts_with(mount))
        // The deepest one decides: /mnt mounted but /mnt/nas not is missing.
        .max_by_key(|mount| mount.components().count())
        .filter(|mount| !table.iter().any(|mounted| Path::new(mounted) == mount))
}

/// [`check`] (with an id) or [`claim`] (without one), given at most
/// [`CHECK_TIMEOUT`]: a hung network mount must not hang the caller. A check
/// still running from an earlier timeout is not started again.
pub fn bounded(
    root: &Path,
    expected: Option<&str>,
    guard: &StorageGuard,
) -> Result<String, String> {
    let id = expected.map(str::to_owned);
    bounded_with(root, guard, move |path| match &id {
        Some(id) => match check(path, id) {
            Readiness::Ready => Ok(id.clone()),
            Readiness::Missing(reason) => Err(reason),
        },
        None => claim(path),
    })
}

/// Run `operation` on `root` given at most [`CHECK_TIMEOUT`] (see [`bounded`]).
fn bounded_with(
    root: &Path,
    guard: &StorageGuard,
    operation: impl FnOnce(&Path) -> Result<String, String> + Send + 'static,
) -> Result<String, String> {
    if !guard.begin(root) {
        return Err(format!("{} is not responding", root.display()));
    }
    let (done, wait) = std::sync::mpsc::channel();
    let path = root.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("storage-check".into())
        .spawn(move || {
            let _ = done.send(operation(&path));
        });
    if spawned.is_err() {
        guard.end(root);
        return Err("cannot check the recording location".into());
    }
    match wait.recv_timeout(CHECK_TIMEOUT) {
        Ok(result) => {
            guard.end(root);
            result
        }
        // The thread ends the check when (if) the file system answers; until
        // then this location stays "not responding".
        Err(_) => Err(format!("{} is not responding", root.display())),
    }
}

/// Each recording location's last known readiness.
#[derive(Default)]
pub struct StorageGuard {
    status: Mutex<HashMap<PathBuf, Readiness>>,
    in_flight: Mutex<HashSet<PathBuf>>,
}

impl StorageGuard {
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The last result for `root`, if it was checked.
    pub fn status(&self, root: &Path) -> Option<Readiness> {
        Self::lock(&self.status).get(root).cloned()
    }

    /// Whether `root` may be written to, by its last check; a location never
    /// checked yet counts as ready (its first use checks it).
    pub fn is_ready(&self, root: &Path) -> bool {
        self.status(root).map_or(true, |status| status.is_ready())
    }

    /// Record `root`'s readiness; whether it changed.
    pub fn set(&self, root: &Path, readiness: Readiness) -> bool {
        Self::lock(&self.status).insert(root.to_path_buf(), readiness.clone()) != Some(readiness)
    }

    /// Locations that are missing now, with why.
    pub fn missing(&self) -> Vec<(PathBuf, String)> {
        let mut missing: Vec<_> = Self::lock(&self.status)
            .iter()
            .filter_map(|(root, status)| Some((root.clone(), status.reason()?.to_owned())))
            .collect();
        missing.sort();
        missing
    }

    /// The locations that are missing now.
    pub fn missing_roots(&self) -> Vec<PathBuf> {
        self.missing().into_iter().map(|(root, _)| root).collect()
    }

    fn begin(&self, root: &Path) -> bool {
        Self::lock(&self.in_flight).insert(root.to_path_buf())
    }

    fn end(&self, root: &Path) {
        Self::lock(&self.in_flight).remove(root);
    }
}

/// Check `root` (claiming it on first use, its id saved in the config)
/// without holding the state lock during the check, and record the result.
pub async fn ensure(
    state: &std::sync::Arc<tokio::sync::RwLock<crate::app_state::AppState>>,
    root: &Path,
) -> Result<(), String> {
    let key = root.to_string_lossy().into_owned();
    let (expected, guard) = {
        let st = state.read().await;
        (
            st.config.recording.storage_ids.get(&key).cloned(),
            std::sync::Arc::clone(&st.storage),
        )
    };
    let (path, checker, wanted) = (
        root.to_path_buf(),
        std::sync::Arc::clone(&guard),
        expected.clone(),
    );
    let result = tokio::task::spawn_blocking(move || bounded(&path, wanted.as_deref(), &checker))
        .await
        .unwrap_or_else(|_| Err("cannot check the location".into()));
    if let (Ok(id), None) = (&result, &expected) {
        let mut st = state.write().await;
        st.config.recording.storage_ids.insert(key, id.clone());
        if let Err(error) = st.save_config() {
            tracing::warn!(storage = %root.display(), %error, "Cannot save the location's id");
        }
        tracing::info!(storage = %root.display(), "Location marked");
    }
    let readiness = match &result {
        Ok(_) => Readiness::Ready,
        Err(reason) => Readiness::Missing(reason.clone()),
    };
    if guard.set(root, readiness) {
        if let Err(reason) = &result {
            tracing::warn!(storage = %root.display(), reason = %reason, "Location missing");
        }
    }
    result.map(|_| ())
}

/// Record to `root` again although its marker is missing: the user confirmed
/// it is the right folder (see [`reclaim`]). Only a recording location in use
/// (`locations`) qualifies. Cameras recording there restart at once.
pub async fn use_location(
    state: &std::sync::Arc<tokio::sync::RwLock<crate::app_state::AppState>>,
    root: &Path,
) -> Result<(), String> {
    let (expected, guard) = {
        let st = state.read().await;
        let Some((_, expected)) = locations(&st).into_iter().find(|(path, _)| path == root) else {
            return Err(format!("{} is not a recording location", root.display()));
        };
        (expected, std::sync::Arc::clone(&st.storage))
    };
    let (path, checker) = (root.to_path_buf(), std::sync::Arc::clone(&guard));
    let id = tokio::task::spawn_blocking(move || {
        bounded_with(&path, &checker, move |path| {
            reclaim(path, expected.as_deref())
        })
    })
    .await
    .unwrap_or_else(|_| Err("cannot check the location".into()))?;
    let mut st = state.write().await;
    let key = root.to_string_lossy().into_owned();
    if st.config.recording.storage_ids.get(&key) != Some(&id) {
        st.config.recording.storage_ids.insert(key, id);
        st.save_config()
            .map_err(|error| format!("cannot save the location: {error}"))?;
    }
    tracing::info!(storage = %root.display(), "Recording location confirmed by the user; recording there resumes");
    if guard.set(root, Readiness::Ready) {
        restart_cameras_at(&mut st, &[root.to_path_buf()]);
    }
    Ok(())
}

/// Restart the ingest of every camera that records under one of `roots`, so
/// recording there stops or resumes.
fn restart_cameras_at(st: &mut crate::app_state::AppState, roots: &[PathBuf]) {
    for cam in st.config.cameras.clone() {
        let own = [st.camera_storage_root(&cam), st.global_storage_root()];
        if own.iter().any(|root| roots.contains(root)) {
            st.restart_ingest(&cam);
        }
    }
}

/// How often every recording location is checked again.
pub const RECHECK: Duration = Duration::from_secs(30);

/// The recording locations in use: the global one, cameras' own folders and
/// the archive, each with the id its marker should hold.
pub fn locations(st: &crate::app_state::AppState) -> Vec<(PathBuf, Option<String>)> {
    let mut roots = vec![st.global_storage_root()];
    for cam in &st.config.cameras {
        let root = st.camera_storage_root(cam);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let archive = &st.config.recording.archive;
    if archive.enabled && !archive.path.is_empty() {
        let root = crate::app_state::expand_user_path(&archive.path);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
        .into_iter()
        .map(|root| {
            let id = st
                .config
                .recording
                .storage_ids
                .get(&root.to_string_lossy().into_owned())
                .cloned();
            (root, id)
        })
        .collect()
}

/// Check every recording location every [`RECHECK`], off the runtime and
/// the state lock. When one goes missing or comes back, the cameras that
/// record there restart their ingest, which stops or resumes recording.
pub async fn watch(
    state: std::sync::Arc<tokio::sync::RwLock<crate::app_state::AppState>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut tick = tokio::time::interval(RECHECK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.recv() => return,
        }
        let (roots, guard) = {
            let st = state.read().await;
            (locations(&st), std::sync::Arc::clone(&st.storage))
        };
        let checker = std::sync::Arc::clone(&guard);
        let results = tokio::task::spawn_blocking(move || {
            roots
                .into_iter()
                .map(|(root, id)| {
                    let readiness = match &id {
                        Some(id) => match bounded(&root, Some(id), &checker) {
                            Ok(_) => Readiness::Ready,
                            Err(reason) => Readiness::Missing(reason),
                        },
                        // Not claimed yet (nothing recorded there): only an
                        // unmounted mount point makes it missing.
                        None => match unmounted_mount_point(&root) {
                            Some(mount) => Readiness::Missing(not_mounted(&mount)),
                            None => Readiness::Ready,
                        },
                    };
                    (root, readiness)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let changed: Vec<PathBuf> = results
            .into_iter()
            .filter_map(|(root, readiness)| {
                let reason = readiness.reason().map(str::to_owned);
                guard.set(&root, readiness).then(|| {
                    match &reason {
                        Some(reason) => tracing::warn!(storage = %root.display(), reason = %reason, "Recording location missing; recording there stops"),
                        None => tracing::info!(storage = %root.display(), "Recording location is back; recording there resumes"),
                    }
                    root
                })
            })
            .collect();
        if changed.is_empty() {
            continue;
        }
        let mut st = state.write().await;
        restart_cameras_at(&mut st, &changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nw-guard-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_claimed_location_is_ready_until_its_marker_goes() {
        let dir = temp();
        let root = dir.join("recordings");
        let id = claim(&root).unwrap();
        assert!(root.join(MARKER_FILE).is_file());
        assert_eq!(check(&root, &id), Readiness::Ready);
        assert_eq!(claim(&root).unwrap(), id, "claiming again keeps the marker");
        // An empty mount point: the folder is there, its marker is not.
        std::fs::remove_file(root.join(MARKER_FILE)).unwrap();
        assert!(check(&root, &id).reason().unwrap().contains("not there"));
        // The folder itself is gone (a removed USB drive).
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!check(&root, &id).is_ready());
        assert!(!root.exists(), "checking never creates the folder");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn another_drive_at_the_same_place_is_not_ours() {
        let dir = temp();
        let id = claim(&dir).unwrap();
        write_marker(&dir, "someone-else").unwrap();
        assert!(check(&dir, &id)
            .reason()
            .unwrap()
            .contains("different drive"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_path_under_an_unmounted_fstab_entry_is_missing() {
        let fstab =
            "# comment\nUUID=abc / ext4 defaults 0 1\nnas:/video /mnt/nas nfs4 defaults 0 0\n\
                     /dev/sdb1 /mnt ext4 defaults 0 2\n/swapfile none swap sw 0 0\n";
        let mounted = "/dev/nvme0n1p2 / ext4 rw 0 0\n/dev/sdb1 /mnt ext4 rw 0 0\n";
        assert_eq!(
            unmounted_in(Path::new("/mnt/nas/neurowatch"), fstab, mounted),
            Some(PathBuf::from("/mnt/nas")),
            "the deepest fstab entry decides"
        );
        assert_eq!(unmounted_in(Path::new("/mnt/usb"), fstab, mounted), None);
        assert_eq!(
            unmounted_in(Path::new("/home/me/Videos"), fstab, mounted),
            None
        );
        let all = format!("{mounted}nas:/video /mnt/nas nfs4 rw 0 0\n");
        assert_eq!(
            unmounted_in(Path::new("/mnt/nas/neurowatch"), fstab, &all),
            None
        );
    }

    #[test]
    fn the_guard_caches_results_and_lists_what_is_missing() {
        let guard = StorageGuard::default();
        let root = Path::new("/srv/recordings");
        assert!(guard.is_ready(root), "unchecked counts as ready");
        assert!(guard.set(root, Readiness::Missing("gone".into())));
        assert!(
            !guard.set(root, Readiness::Missing("gone".into())),
            "no change"
        );
        assert!(!guard.is_ready(root));
        assert_eq!(
            guard.missing(),
            vec![(root.to_path_buf(), "gone".to_owned())]
        );
        assert!(guard.set(root, Readiness::Ready));
        assert!(guard.missing().is_empty());
    }

    #[test]
    fn a_location_whose_marker_was_deleted_can_be_used_again() {
        let dir = temp();
        let root = dir.join("recordings");
        let id = claim(&root).unwrap();
        std::fs::remove_file(root.join(MARKER_FILE)).unwrap();
        assert!(!check(&root, &id).is_ready());
        assert_eq!(reclaim(&root, Some(&id)).unwrap(), id, "the remembered id");
        assert_eq!(check(&root, &id), Readiness::Ready);
        // Another drive's marker is kept: the config takes its id.
        write_marker(&root, "other-drive").unwrap();
        assert_eq!(reclaim(&root, Some(&id)).unwrap(), "other-drive");
        // A folder that is not there is never created.
        std::fs::remove_dir_all(&root).unwrap();
        assert!(reclaim(&root, Some(&id)).unwrap_err().contains("not there"));
        assert!(!root.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_bounded_check_claims_or_checks() {
        let dir = temp();
        let guard = StorageGuard::default();
        let id = bounded(&dir, None, &guard).unwrap();
        assert_eq!(bounded(&dir, Some(&id), &guard).unwrap(), id);
        assert!(bounded(&dir, Some("other"), &guard).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
