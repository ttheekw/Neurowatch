//! Optional outbound-only NeuroWatch cloud-v1 node link.
//!
//! This module deliberately owns no camera, media, retention, or recording state.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
#[cfg(test)]
use ed25519_dalek::{Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use time::{Duration as TimeDuration, OffsetDateTime};

pub const ENROLLMENT_AUDIENCE: &str = "neurowatch-cloud-enrollment";
pub const NODE_AUDIENCE: &str = "neurowatch-cloud-node";
const NODE_CHALLENGE_AUDIENCE: &str = "neurowatch-cloud-node-challenge";
const OPERATION: &str = "metadata_check_in";
pub const INVENTORY_OPERATION: &str = "inventory_snapshot";
pub const PUBLISH_DIRECTIVES_OPERATION: &str = "publish_directives";
pub const COVERAGE_OPERATION: &str = "recording_coverage";
pub const THUMBNAIL_DEMAND_OPERATION: &str = "thumbnail_demand";
pub const THUMBNAIL_BATCH_OPERATION: &str = "thumbnail_batch";
pub const DEPARTURE_OPERATION: &str = "departure";
pub const CONFIG_PULL_OPERATION: &str = "config_pull";
pub const CONFIG_RESULT_OPERATION: &str = "config_result";
pub const DIAGNOSTICS_REPORT_OPERATION: &str = "diagnostics_report";

mod clock;
mod diagnostics;
#[cfg(windows)]
mod dpapi;
mod exports;
mod inventory;
pub mod sealed;
mod websocket;
pub use clock::CloudClock;
pub use diagnostics::{diagnostics_snapshot, SystemInfo, MAX_DIAGNOSTICS_BYTES};
pub use exports::{ExportSource, SegmentLookup, EXPORT_PULL_OPERATION, EXPORT_REPORT_OPERATION};
#[cfg(test)]
pub use inventory::camera_inventory;
pub use inventory::{
    camera_coverage, camera_inventory_with_live, reported_camera_ids, CameraCoverage,
    CameraSnapshot, MAX_COVERAGE_RANGES,
};

/// Supplies the current allowlisted camera inventory to the scheduler.
pub type InventorySource = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<CameraSnapshot>> + Send>>
        + Send
        + Sync,
>;
/// Supplies when each camera has local recordings (times only).
pub type CoverageSource = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<CameraCoverage>> + Send>>
        + Send
        + Sync,
>;
/// Runs one allowlisted portal request (settings, events, PTZ; desktop or headless alike).
/// The flag says whether it arrived sealed (see [`sealed`]).
pub type RemoteConfigHandler = Arc<
    dyn Fn(
            String,
            Value,
            bool,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::result::Result<Value, String>> + Send>,
        > + Send
        + Sync,
>;

/// A configuration request a browser queued for this node.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteConfigRequest {
    pub request_id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Longest the cloud holds a configuration pull open; below the cloud
/// client's 20 s request timeout.
pub const CONFIG_PULL_WAIT: Duration = Duration::from_secs(15);
/// A pull that came back at once is not repeated sooner than this, which
/// keeps a busy editing session inside the cloud's challenge allowance.
const CONFIG_PULL_MIN_CYCLE: Duration = Duration::from_secs(2);
/// After a pull that brought requests, the next comes this soon: a held PTZ
/// direction renews about once a second and its Stop must not wait a whole
/// cycle. The cloud's per-operation challenge allowance still bounds the rate.
const CONFIG_PULL_BUSY_CYCLE: Duration = Duration::from_millis(200);
/// Wait before asking a cloud that does not offer remote configuration again.
const CONFIG_UNSUPPORTED_RETRY: Duration = Duration::from_secs(300);
const MAX_CONFIG_RESULT_BYTES: usize = 262_144;
const MAX_CONFIG_ERROR_CHARS: usize = 500;

/// Collects one live diagnostics snapshot (see [`diagnostics_snapshot`]).
pub type DiagnosticsSource = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> + Send + Sync,
>;

/// Longest the cloud holds a diagnostics report open while no page watches;
/// it answers the moment one opens. Below the cloud client's 20 s timeout.
pub const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(15);
/// How often a watched node sends a snapshot.
pub const DIAGNOSTICS_INTERVAL: Duration = Duration::from_secs(3);
/// An unwatched report that came back at once is not repeated sooner than
/// this, so the node never spins against a cloud that does not wait.
const DIAGNOSTICS_MIN_CYCLE: Duration = Duration::from_secs(4);
/// Wait before asking a cloud that does not offer diagnostics again.
const DIAGNOSTICS_UNSUPPORTED_RETRY: Duration = Duration::from_secs(300);
/// Longest collecting one snapshot may take before the report goes without it.
const DIAGNOSTICS_COLLECT_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize)]
pub struct ThumbnailImage {
    pub camera_id: String,
    pub jpeg_b64: String,
}

/// Longest the cloud holds a thumbnail-demand poll open waiting for a page
/// to ask for snapshots; it answers the moment demand changes. Below the
/// cloud client's 20 s request timeout.
pub const THUMBNAIL_DEMAND_WAIT: Duration = Duration::from_secs(15);
/// How often a camera that stays on screen gets a new snapshot.
pub const THUMBNAIL_REFRESH: Duration = Duration::from_secs(30);
/// A snapshot counts as due this much early, so it is not late by the time
/// it has been captured and uploaded.
pub const THUMBNAIL_DUE_SLACK: Duration = Duration::from_secs(1);
/// Retry interval for a camera on screen that could not be captured (for
/// example because it is not streaming).
pub const THUMBNAIL_MISSED_RETRY: Duration = Duration::from_secs(5);
/// A demand poll that came back quickly is not repeated sooner than this,
/// so the node never spins against a cloud that does not wait.
const THUMBNAIL_MIN_CYCLE: Duration = Duration::from_secs(4);

/// When each demanded camera next needs a snapshot: new demand at once, then
/// every [`THUMBNAIL_REFRESH`]. Cameras that leave the demand are forgotten,
/// so coming back on screen counts as new demand.
#[derive(Default)]
pub(crate) struct ThumbnailSchedule {
    next_at: std::collections::HashMap<String, std::time::Instant>,
}

impl ThumbnailSchedule {
    pub(crate) fn due(&mut self, demand: &[String], now: std::time::Instant) -> Vec<String> {
        self.next_at.retain(|id, _| demand.contains(id));
        demand
            .iter()
            .filter(|id| {
                self.next_at
                    .get(*id)
                    .map_or(true, |at| now + THUMBNAIL_DUE_SLACK >= *at)
            })
            .cloned()
            .collect()
    }

    /// How long a demand poll may wait before a demanded camera is due.
    pub(crate) fn next_due_in(
        &self,
        demand: &[String],
        now: std::time::Instant,
    ) -> Option<Duration> {
        demand
            .iter()
            .map(|id| {
                self.next_at.get(id).map_or(Duration::ZERO, |at| {
                    at.saturating_duration_since(now + THUMBNAIL_DUE_SLACK)
                })
            })
            .min()
    }

    pub(crate) fn uploaded(&mut self, ids: &[String], now: std::time::Instant) {
        for id in ids {
            self.next_at.insert(id.clone(), now + THUMBNAIL_REFRESH);
        }
    }

    pub(crate) fn missed(&mut self, ids: &[String], now: std::time::Instant) {
        for id in ids {
            self.next_at
                .insert(id.clone(), now + THUMBNAIL_MISSED_RETRY);
        }
    }
}

/// `v1/nodes/{node_id}/thumbnail-demands?wait=…&known=…`, where `known` is
/// the demand the node already has (sorted ids joined by ","). Both only
/// shape how long the cloud holds the answer; authorization is the proof.
pub(crate) fn thumbnail_demand_url(
    endpoint: &reqwest::Url,
    node_id: &str,
    wait: Duration,
    known: &[String],
) -> Result<reqwest::Url> {
    let mut url = endpoint
        .join(&format!("v1/nodes/{node_id}/thumbnail-demands"))
        .map_err(|e| CloudError::Invalid(e.to_string()))?;
    let mut sorted = known.to_vec();
    sorted.sort();
    url.query_pairs_mut()
        .append_pair("wait", &wait.as_secs().to_string())
        .append_pair("known", &sorted.join(","));
    Ok(url)
}

/// How often the node looks for camera status changes to report early.
pub const INVENTORY_CHANGE_CHECK: Duration = Duration::from_secs(5);
/// Minimum spacing of change-driven inventory reports, so a flapping camera
/// stays well inside the cloud's per-window challenge allowance.
pub const INVENTORY_CHANGE_MIN_GAP: Duration = Duration::from_secs(10);
/// How often the node looks for newly finished recordings to report ahead of
/// the once-a-minute cycle, so the portal's timeline catches up sooner. Also
/// the minimum spacing of those reports.
pub const COVERAGE_CHANGE_CHECK: Duration = Duration::from_secs(20);
/// While the portal sends events, how long a demand, configuration or
/// diagnostics loop waits for one before asking anyway. It matches the
/// long-poll waits, so the cloud still hears from the node as often.
pub const EVENT_FALLBACK: Duration = Duration::from_secs(15);
/// While the portal sends events, directives are still pulled this often:
/// grants are refreshed with 30 s to spare, and a session a browser abandoned
/// without closing stops within this.
pub const DIRECTIVE_EVENT_FALLBACK: Duration = Duration::from_secs(10);
/// Directive pulls are at least this far apart however often events arrive,
/// which keeps them inside the cloud's per-node challenge allowance.
pub const DIRECTIVE_MIN_GAP: Duration = Duration::from_secs(2);

/// Pause a scheduler loop until `deadline`, or until `wake` fires, but never
/// resume before `earliest`: an event that arrives sooner is kept and acted
/// on then. `false` once the scheduler is stopping.
pub(crate) async fn rest(
    earliest: tokio::time::Instant,
    deadline: tokio::time::Instant,
    wake: Option<&tokio::sync::Notify>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    // Stopping wins over a wake-up or a deadline that is ready at the same time.
    let Some(wake) = wake else {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() { return false; }
            }
            _ = tokio::time::sleep_until(deadline.max(earliest)) => {}
        }
        return true;
    };
    tokio::select! {
        biased;
        changed = stop.changed() => {
            if changed.is_err() || *stop.borrow() { return false; }
        }
        _ = tokio::time::sleep_until(earliest) => {}
    }
    tokio::select! {
        biased;
        changed = stop.changed() => {
            if changed.is_err() || *stop.borrow() { return false; }
        }
        _ = wake.notified() => {}
        _ = tokio::time::sleep_until(deadline) => {}
    }
    true
}

/// Report the inventory ahead of the once-a-minute cycle when a camera's
/// status (or the camera list) changed since the last accepted report, e.g.
/// all cameras going from `connecting` to `streaming` right after start-up.
pub(crate) fn inventory_change_due(
    last: Option<&(Vec<CameraSnapshot>, std::time::Instant)>,
    current: &[CameraSnapshot],
    now: std::time::Instant,
) -> bool {
    last.is_some_and(|(reported, at)| {
        reported.as_slice() != current
            && now.saturating_duration_since(*at) >= INVENTORY_CHANGE_MIN_GAP
    })
}

pub type PreviewSource = Arc<
    dyn Fn(
            Vec<String>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ThumbnailImage>> + Send>>
        + Send
        + Sync,
>;
pub type DirectiveSink = Arc<
    dyn Fn(
            nw_cloud_protocol::media_session::PublishDirectiveBatch,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;
const UNLINK_OPERATION: &str = "unlink";

#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("unsafe or invalid cloud configuration: {0}")]
    Invalid(String),
    #[error("cloud operation is not permitted in the current state")]
    State,
    /// No cloud URL is configured, so linking is unavailable. Distinct from an
    /// unsafe or rejected URL: there is nothing to fix on this node's side.
    #[error("cloud linking is not configured for this installation")]
    NotConfigured,
    #[error("protected cloud store error: {0}")]
    Store(String),
    #[error("cloud request failed: {0}")]
    Network(String),
    /// The cloud answered but refused this node's credentials (HTTP 401/403),
    /// e.g. because it was unlinked or revoked in the web portal.
    #[error("the cloud refused this node (HTTP {0})")]
    Refused(u16),
}

type Result<T> = std::result::Result<T, CloudError>;

pub fn canonical_json(value: &Value) -> Result<String> {
    serde_json::to_string(value).map_err(|e| CloudError::Invalid(e.to_string()))
}

fn sign_value(key: &SigningKey, value: &Value) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(key.sign(canonical_json(value)?.as_bytes()).to_bytes()))
}

#[cfg(test)]
fn verify_signed(public: &str, signature: &str, value: &Value) -> bool {
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(public) else {
        return false;
    };
    let Ok(public): std::result::Result<[u8; 32], _> = bytes.try_into() else {
        return false;
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    let Ok(signature): std::result::Result<[u8; 64], _> = bytes.try_into() else {
        return false;
    };
    VerifyingKey::from_bytes(&public)
        .map(|key| {
            key.verify(
                canonical_json(value).unwrap().as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&signature),
            )
            .is_ok()
        })
        .unwrap_or(false)
}

pub struct ProtectedKeyStore {
    path: PathBuf,
}

impl ProtectedKeyStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(store_err)?;
        }
        Ok(Self { path })
    }
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn load_or_generate(&self) -> Result<SigningKey> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&self.path) {
            Ok(mut file) => {
                validate_private_file(&file, &self.path)?;
                let mut stored = Vec::new();
                file.read_to_end(&mut stored).map_err(store_err)?;
                Ok(SigningKey::from_bytes(&unseal_key(&stored)?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.generate(),
            Err(e) => Err(store_err(e)),
        }
    }
    fn generate(&self) -> Result<SigningKey> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|e| CloudError::Store(e.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| CloudError::Store("key parent missing".into()))?;
        let temp = parent.join(format!(".node-key-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let result = (|| {
            let mut file = options.open(&temp).map_err(store_err)?;
            file.write_all(&seal_key(&seed)?)
                .and_then(|_| file.sync_all())
                .map_err(store_err)?;
            validate_private_file(&file, &temp)?;
            // Windows: the handle must be closed before the file is linked and removed.
            drop(file);
            fs::hard_link(&temp, &self.path).map_err(store_err)?;
            fs::remove_file(&temp).map_err(store_err)?;
            sync_dir(parent)?;
            let key_file = OpenOptions::new()
                .read(true)
                .open(&self.path)
                .map_err(store_err)?;
            validate_private_file(&key_file, &self.path)?;
            Ok(SigningKey::from_bytes(&seed))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

/// The key file's contents: the 32-byte seed itself in a private (0600) file
/// on Unix; on Windows the seed encrypted for the current user (DPAPI), since
/// file modes do not protect it there.
fn seal_key(seed: &[u8; 32]) -> Result<Vec<u8>> {
    #[cfg(windows)]
    return dpapi::protect(seed).map_err(CloudError::Store);
    #[cfg(not(windows))]
    Ok(seed.to_vec())
}

fn unseal_key(stored: &[u8]) -> Result<[u8; 32]> {
    #[cfg(windows)]
    let seed = dpapi::unprotect(stored).map_err(CloudError::Store)?;
    #[cfg(not(windows))]
    let seed = stored.to_vec();
    <[u8; 32]>::try_from(seed.as_slice())
        .map_err(|_| CloudError::Store("invalid key length".into()))
}

/// Makes a rename or new link in `dir` durable. Windows cannot open a
/// directory as a file (and NTFS journals the rename), so it is skipped there.
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
    #[cfg(not(windows))]
    fs::File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(store_err)
}

fn store_err(e: impl std::fmt::Display) -> CloudError {
    CloudError::Store(e.to_string())
}

fn validate_private_file(file: &fs::File, path: &Path) -> Result<()> {
    let metadata = file.metadata().map_err(store_err)?;
    if !metadata.is_file() {
        return Err(CloudError::Store(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(CloudError::Store(
                "private key permissions or links are unsafe".into(),
            ));
        }
    }
    Ok(())
}

fn prepare_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(CloudError::Store("cloud store is not a directory".into()));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if metadata.permissions().mode() & 0o077 != 0
                    || metadata.uid() != unsafe { libc::geteuid() }
                {
                    return Err(CloudError::Store(
                        "cloud directory permissions or ownership are unsafe".into(),
                    ));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // On a fresh install nothing has created the config directory yet.
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(store_err)?;
            }
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            builder.create(path).map_err(store_err)?;
        }
        Err(error) => return Err(store_err(error)),
    }
    Ok(())
}

fn load_state(path: &Path) -> Result<Persisted> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(mut file) => {
            validate_private_file(&file, path)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(store_err)?;
            let persisted: Persisted =
                serde_json::from_slice(&bytes).map_err(|e| CloudError::Store(e.to_string()))?;
            if let Some(pending) = &persisted.pending {
                // Expiry in the past is valid state (discarded after load);
                // only malformed or non-canonical values are rejected.
                let canonical = canonical_cloud_time(&pending.expires_at, None)
                    .map_err(|_| CloudError::Store("invalid persisted cloud timestamp".into()))?;
                if canonical != pending.expires_at {
                    return Err(CloudError::Store(
                        "non-canonical persisted cloud timestamp".into(),
                    ));
                }
            }
            if let Some(last_check_in) = &persisted.status.last_check_in {
                let canonical = canonical_cloud_time(last_check_in, None)
                    .map_err(|_| CloudError::Store("invalid persisted cloud timestamp".into()))?;
                if canonical != *last_check_in {
                    return Err(CloudError::Store(
                        "non-canonical persisted cloud timestamp".into(),
                    ));
                }
            }
            Ok(persisted)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Persisted::default()),
        Err(error) => Err(store_err(error)),
    }
}

/// Which cloud endpoints a node may be linked to.
///
/// Production nodes only accept credential-free HTTPS on a public hostname.
/// `allow_loopback_http` is a local-development opt-in (set only by
/// [`EndpointPolicy::DEV_ENV`]) that additionally accepts plain HTTP on the
/// literal `127.0.0.1`, so a node can link to a cloud stack on the same host.
///
/// `pinned`, when set, is the only endpoint accepted: for a new link, a
/// saved link and a link started with an explicit URL alike. Release builds
/// pin [`NEUROWATCH_CLOUD`] (see [`CloudOptions::from_build_and_env`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndpointPolicy {
    pub allow_loopback_http: bool,
    pub pinned: Option<&'static str>,
}
impl EndpointPolicy {
    pub const DEV_ENV: &'static str = "NW_CLOUD_DEV_ALLOW_LOOPBACK_HTTP";

    pub fn from_env_value(value: Option<&str>) -> Self {
        Self {
            allow_loopback_http: value == Some("1"),
            pinned: None,
        }
    }
}

/// NeuroWatch Cloud: the only cloud a release build links to.
pub const NEUROWATCH_CLOUD: &str = "https://neurowatch.app";

/// Node-side cloud settings: which cloud to link to and which endpoints are allowed.
///
/// `endpoint` is `None` when no cloud URL is configured. That is a distinct,
/// safe state — linking is unavailable and the UI says so. There is
/// deliberately no localhost default, so a node never tries to link to
/// whatever happens to listen on this machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudOptions {
    pub endpoint: Option<String>,
    pub policy: EndpointPolicy,
}
impl CloudOptions {
    pub const ENDPOINT_ENV: &'static str = "NW_CLOUD_ENDPOINT";

    pub fn from_env() -> Self {
        Self::from_env_values(
            std::env::var(Self::ENDPOINT_ENV).ok().as_deref(),
            std::env::var(EndpointPolicy::DEV_ENV).ok().as_deref(),
        )
    }

    /// The cloud this build links to: [`NEUROWATCH_CLOUD`], and only it.
    ///
    /// Development builds (debug assertions) may instead name a development
    /// cloud in `NW_CLOUD_ENDPOINT` (with [`EndpointPolicy::DEV_ENV`] for one
    /// on `127.0.0.1`); release builds read no setting at all.
    pub fn from_build_and_env() -> Self {
        Self::for_build(
            cfg!(debug_assertions),
            std::env::var(Self::ENDPOINT_ENV).ok().as_deref(),
            std::env::var(EndpointPolicy::DEV_ENV).ok().as_deref(),
        )
    }

    /// [`Self::from_build_and_env`] with the build kind and settings given.
    pub fn for_build(development: bool, endpoint: Option<&str>, loopback: Option<&str>) -> Self {
        let dev_cloud = endpoint.filter(|value| !value.trim().is_empty());
        match dev_cloud {
            Some(endpoint) if development => Self::from_env_values(Some(endpoint), loopback),
            _ => Self::pinned_to(Some(NEUROWATCH_CLOUD)),
        }
    }

    /// Only `endpoint` may be linked to; with none, nothing may.
    pub fn pinned_to(endpoint: Option<&'static str>) -> Self {
        let endpoint = endpoint
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_end_matches('/'));
        Self {
            endpoint: endpoint.map(str::to_owned),
            policy: EndpointPolicy {
                allow_loopback_http: false,
                // An empty pin matches no URL.
                pinned: Some(endpoint.unwrap_or("")),
            },
        }
    }

    pub fn from_env_values(endpoint: Option<&str>, loopback: Option<&str>) -> Self {
        Self {
            endpoint: endpoint
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.trim_end_matches('/').to_owned()),
            policy: EndpointPolicy::from_env_value(loopback),
        }
    }
}

/// The single code shown by the web portal after an enrollment is created:
/// `<tenant_id>.<enrollment_id>.<transfer_proof>`. The proof is the one-time
/// secret; the tenant scopes the cloud lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollmentCode {
    pub tenant_id: String,
    pub enrollment_id: String,
    pub transfer_proof: String,
}
impl EnrollmentCode {
    pub fn parse(raw: &str) -> Result<Self> {
        let invalid = || CloudError::Invalid("invalid enrollment code".into());
        let mut parts = raw.trim().split('.');
        let (Some(tenant_id), Some(enrollment_id), Some(transfer_proof), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        for id in [tenant_id, enrollment_id] {
            let parsed = uuid::Uuid::parse_str(id).map_err(|_| invalid())?;
            if parsed.to_string() != id {
                return Err(invalid());
            }
        }
        if transfer_proof.len() < 8
            || transfer_proof.len() > 128
            || !transfer_proof
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(invalid());
        }
        Ok(Self {
            tenant_id: tenant_id.into(),
            enrollment_id: enrollment_id.into(),
            transfer_proof: transfer_proof.into(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ClaimParameters {
    protocol_version: u8,
    audience: String,
    enrollment_id: String,
    tenant_id: String,
    expires_at: String,
    challenge: String,
    /// The portal's clock when it answered (newer portals), so a node with a
    /// wrong clock learns the offset before it judges `expires_at`.
    #[serde(default)]
    server_time: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CloudEndpoint(reqwest::Url);
impl CloudEndpoint {
    pub fn parse_with(raw: &str, policy: EndpointPolicy) -> Result<Self> {
        if let Some(pinned) = policy.pinned {
            if pinned.is_empty() || raw.trim().trim_end_matches('/') != pinned {
                return Err(CloudError::Invalid(if pinned.is_empty() {
                    "this build is not linked to a cloud".into()
                } else {
                    format!("this build links only to NeuroWatch Cloud at {pinned}")
                }));
            }
        }
        if raw.starts_with("http://") {
            if policy.allow_loopback_http {
                return Self::loopback(raw);
            }
            if Self::loopback(raw).is_ok() {
                return Err(CloudError::Invalid(format!(
                    "the local development cloud URL {raw} needs the backend started with {}=1",
                    EndpointPolicy::DEV_ENV
                )));
            }
        }
        Self::parse(raw)
    }
    pub fn parse(raw: &str) -> Result<Self> {
        let url = reqwest::Url::parse(raw).map_err(|e| CloudError::Invalid(e.to_string()))?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(CloudError::Invalid(
                "endpoint must be credential-free HTTPS".into(),
            ));
        }
        validate_host(&url)?;
        Ok(Self(url))
    }
    /// Plain HTTP on the literal 127.0.0.1 only; see [`EndpointPolicy`].
    fn loopback(raw: &str) -> Result<Self> {
        let url = reqwest::Url::parse(raw).map_err(|e| CloudError::Invalid(e.to_string()))?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(CloudError::Invalid(
                "development endpoint must be credential-free http://127.0.0.1".into(),
            ));
        }
        Ok(Self(url))
    }
    #[cfg(test)]
    fn loopback_test(raw: &str) -> Result<Self> {
        Self::loopback(raw)
    }
    fn join(&self, path: &str) -> Result<reqwest::Url> {
        self.0
            .join(path)
            .map_err(|e| CloudError::Invalid(e.to_string()))
    }
}

fn validate_host(url: &reqwest::Url) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| CloudError::Invalid("endpoint host required".into()))?;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return validate_resolved_ips(&[ip]);
    }
    if host.eq_ignore_ascii_case("localhost")
        || !host.contains('.')
        || [".local", ".lan", ".internal"]
            .iter()
            .any(|suffix| host.to_ascii_lowercase().ends_with(suffix))
    {
        Err(CloudError::Invalid("public cloud hostname required".into()))
    } else {
        Ok(())
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 240)
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_global_v4(mapped);
    }
    let segments = ip.segments();
    (segments[0] & 0xe000) == 0x2000
        && !(segments[0] == 0x2001 && segments[1] <= 0x01ff)
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && !(segments[0] == 0x2001 && segments[1] == 0x0002)
        && (segments[0] & 0xfff0) != 0x3ff0
}

fn validate_resolved_ips(addresses: &[IpAddr]) -> Result<()> {
    if addresses.is_empty()
        || addresses.iter().any(|ip| match ip {
            IpAddr::V4(ip) => !is_global_v4(*ip),
            IpAddr::V6(ip) => !is_global_v6(*ip),
        })
    {
        return Err(CloudError::Invalid(
            "cloud hostname resolved to a non-global address".into(),
        ));
    }
    Ok(())
}

async fn resolved_client(endpoint: &CloudEndpoint) -> Result<reqwest::Client> {
    let host = endpoint
        .0
        .host_str()
        .ok_or_else(|| CloudError::Invalid("endpoint host required".into()))?;
    let port = endpoint
        .0
        .port_or_known_default()
        .ok_or_else(|| CloudError::Invalid("endpoint port could not be determined".into()))?;
    // An http endpoint can only be constructed by `CloudEndpoint::loopback`
    // (test builds or the explicit development policy), so pin it locally.
    if endpoint.0.scheme() == "http" && host == "127.0.0.1" {
        return build_pinned_client(host, &[SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)]);
    }
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| CloudError::Network(format!("cloud DNS resolution failed: {e}")))?
        .collect();
    let ips: Vec<IpAddr> = resolved.iter().map(SocketAddr::ip).collect();
    validate_resolved_ips(&ips)?;
    build_pinned_client(host, &resolved)
}

fn build_pinned_client(host: &str, addresses: &[SocketAddr]) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))
}

pub fn validate_cloud_data(value: &Value) -> Result<()> {
    const FORBIDDEN: &[&str] = &[
        "camera",
        "rtsp",
        "credential",
        "password",
        "secret",
        "private",
        "lan",
        "ip",
        "ipc",
        "storage",
        "bucket",
        "media",
        "recording",
        "playback",
        "retention",
    ];
    fn walk(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.iter().any(|(k, v)| {
                FORBIDDEN
                    .iter()
                    .any(|word| k.to_ascii_lowercase().contains(word))
                    || walk(v)
            }),
            Value::Array(items) => items.iter().any(walk),
            _ => false,
        }
    }
    if walk(value) {
        Err(CloudError::Invalid(
            "local/media fields are forbidden".into(),
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrollmentInput {
    pub endpoint: String,
    pub enrollment_id: String,
    pub tenant_id: String,
    pub expires_at: String,
    pub challenge: String,
    pub transfer_proof: String,
    pub display_label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub protocol_version: u8,
    pub enrollment_id: String,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub expires_at: String,
    pub challenge: String,
    pub transfer_proof: String,
    pub public_key: String,
    pub signature: String,
    pub display_label: String,
}
impl ClaimRequest {
    pub fn signed_payload(&self) -> Value {
        json!({"protocol_version":self.protocol_version,"enrollment_id":self.enrollment_id,"tenant_id":self.tenant_id,"node_id":self.node_id,"audience":self.audience,"expires_at":self.expires_at,"challenge":self.challenge})
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct ClaimResponse {
    pub claim_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfirmRequest {
    pub protocol_version: u8,
    pub enrollment_id: String,
    pub tenant_id: String,
    pub node_id: String,
    pub audience: String,
    pub expires_at: String,
    pub challenge: String,
    pub claim_id: String,
    pub signature: String,
}
impl ConfirmRequest {
    pub fn signed_payload(&self) -> Value {
        json!({"protocol_version":self.protocol_version,"enrollment_id":self.enrollment_id,"tenant_id":self.tenant_id,"node_id":self.node_id,"audience":self.audience,"expires_at":self.expires_at,"challenge":self.challenge,"claim_id":self.claim_id})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkPhase {
    Unlinked,
    AwaitingClaim,
    AwaitingLocalConfirmation,
    Linked,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CloudStatus {
    pub phase: LinkPhase,
    pub endpoint: Option<String>,
    pub tenant_id: Option<String>,
    pub node_id: Option<String>,
    pub auth_epoch: Option<u64>,
    pub last_check_in: Option<String>,
}
impl Default for CloudStatus {
    fn default() -> Self {
        Self {
            phase: LinkPhase::Unlinked,
            endpoint: None,
            tenant_id: None,
            node_id: None,
            auth_epoch: None,
            last_check_in: None,
        }
    }
}

/// Error code the UI turns into "create a new enrollment code".
pub const ENROLLMENT_EXPIRED_ERROR: &str = "enrollment_expired";

/// What the local UI is told about the link: the persisted state plus facts
/// derived at read time and never written to disk — which cloud this node can
/// talk to, whether a *new* link could be started, and whether a started link
/// attempt outlived its enrollment.
///
/// The two configuration flags are deliberately separate. A node linked (or
/// part-way through linking) earlier keeps a cloud even if this process was
/// started without `NW_CLOUD_ENDPOINT`: confirming, checking in and unlinking
/// all use its saved endpoint. Only starting a *new* link needs a configured
/// one.
#[derive(Clone, Debug, Serialize)]
pub struct CloudStatusReport {
    #[serde(flatten)]
    pub status: CloudStatus,
    pub pending_expired: bool,
    /// This node has a cloud: one configured for new links, one it saved while
    /// linking, or both.
    pub cloud_configured: bool,
    /// A cloud is configured for starting a new link.
    pub new_link_configured: bool,
    /// The cloud to show: the configured one, else the saved one.
    pub cloud_endpoint: Option<String>,
    /// Linked, and a signed request succeeded within [`CLOUD_REACHABLE_WITHIN`].
    pub cloud_reachable: bool,
    /// Configured, but something is wrong: `link_refused` (the cloud no longer
    /// accepts this node, e.g. unlinked in the web portal) or
    /// `endpoint_unusable` (the saved cloud URL is not allowed by current
    /// settings).
    pub cloud_problem: Option<&'static str>,
    /// NeuroWatch Cloud's clock minus this computer's, in whole seconds, once
    /// a cloud answer taught it; signed times are corrected by it.
    pub clock_offset_sec: Option<i64>,
    /// Plain-language advice when this computer's clock is more than a
    /// minute off NeuroWatch Cloud's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A linked node talks to the cloud at least every [`THUMBNAIL_DEMAND_WAIT`]
/// (15 s); three missed rounds mean the cloud is not reachable.
pub const CLOUD_REACHABLE_WITHIN: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Pending {
    enrollment_id: String,
    tenant_id: String,
    node_id: String,
    expires_at: String,
    challenge: String,
    claim_id: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Persisted {
    status: CloudStatus,
    pending: Option<Pending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_unlink: Option<PendingUnlink>,
    /// Directive generations consumed locally but not yet confirmed by a
    /// successful authenticated pull.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_directive_acknowledgements: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingUnlink {
    endpoint: String,
    tenant_id: String,
    node_id: String,
    auth_epoch: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeChallengeResponse {
    protocol_version: u8,
    audience: String,
    operation: String,
    challenge: String,
    expires_at: String,
    /// Tolerated so a portal may add its clock here later; this node learns
    /// the cloud's time from the transport instead.
    #[serde(default, rename = "server_time")]
    _server_time: Option<String>,
}

fn parse_challenge_response(
    value: Value,
    expected_operation: &str,
    now: OffsetDateTime,
) -> Result<NodeChallengeResponse> {
    let response: NodeChallengeResponse = serde_json::from_value(value)
        .map_err(|_| CloudError::Network("invalid challenge response".into()))?;
    let expires = OffsetDateTime::parse(
        &response.expires_at,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|_| CloudError::Network("invalid challenge expiry".into()))?;
    if response.protocol_version != 1
        || response.audience != NODE_AUDIENCE
        || response.operation != expected_operation
        || response.challenge.is_empty()
        || expires <= now
    {
        return Err(CloudError::Network("invalid challenge response".into()));
    }
    Ok(response)
}
struct Inner {
    dir: PathBuf,
    key: SigningKey,
    persisted: Persisted,
    endpoint: Option<CloudEndpoint>,
    endpoint_problem: Option<String>,
    policy: EndpointPolicy,
    cloud_endpoint: Option<String>,
    /// Last successful signed request to the cloud, in memory only: it says
    /// whether the cloud is reachable now, not whether it ever was.
    last_contact: Option<std::time::Instant>,
    /// Set when the cloud refused this node's credentials; cleared by the next
    /// accepted request.
    refused: bool,
    /// Sealed web-portal requests already opened, so none is run twice.
    sealed_replay: sealed::ReplayGuard,
    /// The cloud's clock: what every signed or checked cloud time uses.
    clock: Arc<CloudClock>,
}
impl Inner {
    /// Whether a started link attempt outlived its enrollment, so the operator
    /// needs a new enrollment code. A pure read: it never writes state.
    fn pending_expired(&self) -> bool {
        self.persisted.pending.as_ref().is_some_and(|pending| {
            OffsetDateTime::parse(
                &pending.expires_at,
                &time::format_description::well_known::Rfc3339,
            )
            .map(|expires| expires <= self.clock.now())
            .unwrap_or(true)
        })
    }
    fn report(&self) -> CloudStatusReport {
        let pending_expired = self.pending_expired();
        // The endpoint saved while linking is the one confirm, check-in and
        // unlink already use, so a node that has one is not "no cloud here"
        // even when this process was started without a configured cloud.
        let saved_endpoint = self.persisted.status.endpoint.clone();
        CloudStatusReport {
            pending_expired,
            cloud_configured: self.cloud_endpoint.is_some() || saved_endpoint.is_some(),
            new_link_configured: self.cloud_endpoint.is_some(),
            cloud_endpoint: self.cloud_endpoint.clone().or(saved_endpoint),
            cloud_reachable: self.persisted.status.phase == LinkPhase::Linked
                && !self.refused
                && self
                    .last_contact
                    .is_some_and(|at| at.elapsed() < CLOUD_REACHABLE_WITHIN),
            cloud_problem: if self.persisted.status.phase == LinkPhase::Linked && self.refused {
                Some("link_refused")
            } else if self.endpoint_problem.is_some() {
                Some("endpoint_unusable")
            } else {
                None
            },
            clock_offset_sec: self
                .clock
                .measured_offset()
                .map(|offset| offset.whole_seconds()),
            clock_warning: self.clock.warning(),
            error: pending_expired.then(|| ENROLLMENT_EXPIRED_ERROR.to_owned()),
            status: self.persisted.status.clone(),
        }
    }
    /// Drop a claim/confirmation that outlived its enrollment so a new code can
    /// be used. Only an explicit new link attempt may call this: expiry alone
    /// must never erase state behind the operator's back.
    fn discard_expired_pending(&mut self) -> Result<()> {
        if self.persisted.pending.is_none() || !self.pending_expired() {
            return Ok(());
        }
        tracing::info!("Discarding an expired, unconfirmed cloud link attempt");
        let candidate = Persisted {
            status: CloudStatus::default(),
            pending: None,
            pending_unlink: self.persisted.pending_unlink.clone(),
            pending_directive_acknowledgements: Vec::new(),
        };
        persist_state(&self.dir, &candidate)?;
        self.persisted = candidate;
        self.endpoint = None;
        self.endpoint_problem = None;
        Ok(())
    }
    fn active_endpoint(&self) -> Result<CloudEndpoint> {
        match (&self.endpoint, &self.endpoint_problem) {
            (Some(endpoint), _) => Ok(endpoint.clone()),
            (None, Some(problem)) => Err(CloudError::Invalid(problem.clone())),
            (None, None) => Err(CloudError::State),
        }
    }
}

/// Wake-ups the portal sends over the control WebSocket. Each names a request
/// a scheduler loop already makes on its own timer, and only makes that loop
/// ask now: an event carries no data or authority, so the signed pull stays
/// the source of truth and a lost event only costs the loop's fallback wait.
#[derive(Default)]
pub(crate) struct CloudSignals {
    directives: tokio::sync::Notify,
    thumbnails: tokio::sync::Notify,
    config: tokio::sync::Notify,
    diagnostics: tokio::sync::Notify,
    exports: tokio::sync::Notify,
    /// The connection delivering events, or 0 while none is.
    pushing: std::sync::atomic::AtomicU64,
    connections: std::sync::atomic::AtomicU64,
}

impl CloudSignals {
    /// Wake the loop for `topic`; `None` for a topic this node does not know.
    fn wake(&self, topic: &str) -> Option<&'static str> {
        let (notify, name) = match topic {
            PUBLISH_DIRECTIVES_OPERATION => (&self.directives, PUBLISH_DIRECTIVES_OPERATION),
            THUMBNAIL_DEMAND_OPERATION => (&self.thumbnails, THUMBNAIL_DEMAND_OPERATION),
            CONFIG_PULL_OPERATION => (&self.config, CONFIG_PULL_OPERATION),
            DIAGNOSTICS_REPORT_OPERATION => (&self.diagnostics, DIAGNOSTICS_REPORT_OPERATION),
            EXPORT_PULL_OPERATION => (&self.exports, EXPORT_PULL_OPERATION),
            _ => return None,
        };
        // A stored permit: an event that arrives mid-request is not lost.
        notify.notify_one();
        Some(name)
    }

    /// A connection that delivers events is up. Every loop asks once, for
    /// whatever changed while no events could arrive.
    fn connected(&self) -> u64 {
        use std::sync::atomic::Ordering;
        let id = self.connections.fetch_add(1, Ordering::Relaxed) + 1;
        self.pushing.store(id, Ordering::Release);
        for notify in [
            &self.directives,
            &self.thumbnails,
            &self.config,
            &self.diagnostics,
            &self.exports,
        ] {
            notify.notify_one();
        }
        id
    }

    fn disconnected(&self, connection: u64) {
        use std::sync::atomic::Ordering;
        if connection != 0 {
            let _ =
                self.pushing
                    .compare_exchange(connection, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    fn pushed(&self) -> bool {
        self.pushing.load(std::sync::atomic::Ordering::Acquire) != 0
    }
}

#[derive(Clone)]
pub struct CloudNode {
    inner: Arc<Mutex<Inner>>,
    websocket: Option<Arc<websocket::WsTransport>>,
    signals: Arc<CloudSignals>,
    clock: Arc<CloudClock>,
}
impl CloudNode {
    /// Production-policy open for tests; the binary uses `open_with_options`.
    #[cfg(test)]
    pub fn open(dir: PathBuf) -> Result<Self> {
        let mut node = Self::open_with_options(dir, CloudOptions::default())?;
        // Historical scripted HTTP fixtures explicitly use the legacy transport.
        node.websocket = None;
        Ok(node)
    }
    pub fn open_with_options(dir: PathBuf, options: CloudOptions) -> Result<Self> {
        let CloudOptions {
            endpoint: cloud_endpoint,
            policy,
        } = options;
        prepare_private_dir(&dir)?;
        let key = ProtectedKeyStore::open(dir.join("node.key"))?.load_or_generate()?;
        let state_path = dir.join("cloud-state.json");
        let persisted = load_state(&state_path)?;
        // A saved URL the current settings reject (e.g. a development link reopened
        // without the opt-in) must not disable status or unlink; remember why instead.
        let (endpoint, endpoint_problem) = match persisted.status.endpoint.as_deref() {
            None => (None, None),
            Some(raw) => match parse_persisted_endpoint(raw, policy) {
                Ok(endpoint) => (Some(endpoint), None),
                Err(error) => {
                    tracing::warn!(error = %error, "Saved cloud URL is not usable with the current settings");
                    (
                        None,
                        Some(format!("the saved cloud URL {raw} is not usable: {error}")),
                    )
                }
            },
        };
        // An expired, unconfirmed attempt is kept: startup reports it as an
        // actionable state instead of silently clearing the operator's link.
        let clock = Arc::new(CloudClock::new());
        let inner = Inner {
            dir,
            key,
            persisted,
            endpoint,
            endpoint_problem,
            policy,
            cloud_endpoint,
            last_contact: None,
            refused: false,
            sealed_replay: sealed::ReplayGuard::default(),
            clock: Arc::clone(&clock),
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            websocket: Some(Arc::new(websocket::WsTransport::with_events())),
            signals: Arc::default(),
            clock,
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| CloudError::Store("cloud state lock poisoned".into()))
    }
    pub fn shared(&self) -> Self {
        self.clone()
    }
    /// The cloud's clock, shared with everything that checks cloud-issued
    /// times (media grants, sealed requests).
    pub fn clock(&self) -> Arc<CloudClock> {
        Arc::clone(&self.clock)
    }
    /// Whether the portal currently wakes this node with events, so its loops
    /// can ask without long-polling and wait for an event instead.
    pub fn events_pushed(&self) -> bool {
        self.signals.pushed()
    }
    /// The cloud URL new links are made against (`NW_CLOUD_ENDPOINT`), or
    /// `None` when this installation has no cloud configured.
    pub fn cloud_endpoint(&self) -> Option<String> {
        self.lock().ok().and_then(|i| i.cloud_endpoint.clone())
    }
    /// The persisted link state. A pure read: reporting status never rewrites
    /// or erases what the operator set up.
    pub fn status(&self) -> CloudStatus {
        self.lock()
            .map(|i| i.persisted.status.clone())
            .unwrap_or_default()
    }
    /// The persisted state plus what the local UI needs to explain it.
    ///
    /// A failure here is an internal fault and is returned as one: reporting a
    /// default "no cloud configured" status instead would tell the operator
    /// their link is gone and hide a bug behind a deliberate-looking state.
    pub fn report(&self) -> Result<CloudStatusReport> {
        Ok(self.lock()?.report())
    }

    /// Note a successful signed request, for [`CloudStatusReport::cloud_reachable`].
    fn mark_contact(&self) {
        if let Ok(mut inner) = self.lock() {
            inner.last_contact = Some(std::time::Instant::now());
            inner.refused = false;
        }
    }

    /// Record how a signed request to the cloud went, for the status report.
    fn note<T>(&self, outcome: &Result<T>) {
        match outcome {
            Ok(_) => self.mark_contact(),
            Err(CloudError::Refused(_)) => {
                if let Ok(mut inner) = self.lock() {
                    inner.refused = true;
                }
            }
            Err(_) => {}
        }
    }
    pub fn prepare_claim(&mut self, mut input: EnrollmentInput) -> Result<ClaimRequest> {
        let policy = self.lock()?.policy;
        let endpoint = CloudEndpoint::parse_with(&input.endpoint, policy)?;
        let now = self.clock.now();
        input.expires_at = canonical_cloud_time(&input.expires_at, Some(now))?;
        validate_enrollment(&input)?;
        let mut inner = self.lock()?;
        inner.discard_expired_pending()?;
        if inner.persisted.status.phase != LinkPhase::Unlinked {
            return Err(CloudError::State);
        }
        let node_id = uuid::Uuid::new_v4().to_string();
        let public_key = URL_SAFE_NO_PAD.encode(inner.key.verifying_key().as_bytes());
        let mut claim = ClaimRequest {
            protocol_version: 1,
            enrollment_id: input.enrollment_id.clone(),
            tenant_id: input.tenant_id.clone(),
            node_id: node_id.clone(),
            audience: ENROLLMENT_AUDIENCE.into(),
            expires_at: input.expires_at.clone(),
            challenge: input.challenge.clone(),
            transfer_proof: input.transfer_proof,
            public_key,
            signature: String::new(),
            display_label: input.display_label,
        };
        claim.signature = sign_value(&inner.key, &claim.signed_payload())?;
        let mut candidate = inner.persisted.clone();
        candidate.status = CloudStatus {
            phase: LinkPhase::AwaitingClaim,
            endpoint: Some(input.endpoint),
            tenant_id: Some(input.tenant_id.clone()),
            node_id: Some(node_id.clone()),
            auth_epoch: None,
            last_check_in: None,
        };
        candidate.pending = Some(Pending {
            enrollment_id: input.enrollment_id,
            tenant_id: input.tenant_id,
            node_id,
            expires_at: input.expires_at,
            challenge: input.challenge,
            claim_id: None,
        });
        persist_state(&inner.dir, &candidate)?;
        inner.persisted = candidate;
        inner.endpoint = Some(endpoint);
        inner.endpoint_problem = None;
        Ok(claim)
    }
    pub fn record_claim_response(&mut self, response: ClaimResponse) -> Result<()> {
        uuid::Uuid::parse_str(&response.claim_id)
            .map_err(|_| CloudError::Invalid("invalid claim id".into()))?;
        let mut inner = self.lock()?;
        if inner.persisted.status.phase != LinkPhase::AwaitingClaim {
            return Err(CloudError::State);
        }
        let mut candidate = inner.persisted.clone();
        candidate
            .pending
            .as_mut()
            .ok_or(CloudError::State)?
            .claim_id = Some(response.claim_id);
        candidate.status.phase = LinkPhase::AwaitingLocalConfirmation;
        persist_state(&inner.dir, &candidate)?;
        inner.persisted = candidate;
        Ok(())
    }
    fn confirm(&mut self) -> Result<ConfirmRequest> {
        let inner = self.lock()?;
        if inner.persisted.status.phase != LinkPhase::AwaitingLocalConfirmation {
            return Err(CloudError::State);
        }
        // The cloud would reject a confirmation signed against a dead
        // enrollment; say so here instead of returning its opaque error.
        if inner.pending_expired() {
            return Err(CloudError::Invalid(
                "the enrollment expired before this link finished; create a new enrollment code and link again".into(),
            ));
        }
        let p = inner.persisted.pending.clone().ok_or(CloudError::State)?;
        let mut request = ConfirmRequest {
            protocol_version: 1,
            enrollment_id: p.enrollment_id,
            tenant_id: p.tenant_id,
            node_id: p.node_id,
            audience: ENROLLMENT_AUDIENCE.into(),
            expires_at: p.expires_at,
            challenge: p.challenge,
            claim_id: p.claim_id.ok_or(CloudError::State)?,
            signature: String::new(),
        };
        request.signature = sign_value(&inner.key, &request.signed_payload())?;
        Ok(request)
    }
    pub fn record_confirm_response(&mut self, response: &Value) -> Result<()> {
        let mut inner = self.lock()?;
        if inner.persisted.status.phase != LinkPhase::AwaitingLocalConfirmation
            || !matches!(response["status"].as_str(), Some("confirmed" | "linked"))
            || response["node_id"].as_str() != inner.persisted.status.node_id.as_deref()
        {
            return Err(CloudError::Network("invalid confirmation response".into()));
        }
        let mut candidate = inner.persisted.clone();
        candidate.status.phase = LinkPhase::Linked;
        candidate.status.auth_epoch = Some(1);
        candidate.pending = None;
        persist_state(&inner.dir, &candidate)?;
        inner.persisted = candidate;
        Ok(())
    }
    pub async fn begin_remote(&mut self, input: EnrollmentInput) -> Result<CloudStatus> {
        let claim = self.prepare_claim(input)?;
        let endpoint = {
            let i = self.lock()?;
            i.active_endpoint()?
        };
        let client = resolved_client(&endpoint).await?;
        let url = endpoint.join(&format!(
            "v1/node-enrollments/{}/claim",
            claim.enrollment_id
        ))?;
        let response: ClaimResponse = post_typed(&self.clock, &client, url, &claim).await?;
        self.record_claim_response(response)?;
        Ok(self.status())
    }
    /// Link from a caller that carries the whole claim binding (the request
    /// shape used before enrollment codes).
    ///
    /// The caller does not get to choose the cloud: the request may only target
    /// the endpoint this installation is configured for, and that configured
    /// endpoint — never the caller's string — is what gets persisted. Without a
    /// configured cloud the request is refused outright, so a local caller can
    /// never enroll an unconfigured node into a cloud of its own choosing.
    pub async fn begin_legacy(&mut self, input: EnrollmentInput) -> Result<CloudStatus> {
        let configured = self.lock()?.cloud_endpoint.clone();
        let Some(configured) = configured else {
            return Err(CloudError::NotConfigured);
        };
        if !same_endpoint(&configured, &input.endpoint) {
            return Err(CloudError::Invalid(format!(
                "this node links to {configured} only; the request named a different cloud"
            )));
        }
        self.begin_remote(EnrollmentInput {
            endpoint: configured,
            ..input
        })
        .await
    }

    /// Link using the portal's single enrollment code: fetch the claim binding
    /// from the configured cloud with the one-time proof, then claim.
    pub async fn begin_with_code(
        &mut self,
        code: &str,
        display_label: String,
    ) -> Result<CloudStatus> {
        let code = EnrollmentCode::parse(code)?;
        let (raw_endpoint, policy) = {
            let mut i = self.lock()?;
            i.discard_expired_pending()?;
            if i.persisted.status.phase != LinkPhase::Unlinked {
                return Err(CloudError::State);
            }
            // No configured cloud is its own state: there is no URL to judge,
            // so this is neither an unsafe nor a rejected endpoint.
            let Some(raw_endpoint) = i.cloud_endpoint.clone() else {
                return Err(CloudError::NotConfigured);
            };
            (raw_endpoint, i.policy)
        };
        let endpoint = CloudEndpoint::parse_with(&raw_endpoint, policy)?;
        let client = resolved_client(&endpoint).await?;
        let url = endpoint.join(&format!(
            "v1/node-enrollments/{}/claim-parameters",
            code.enrollment_id
        ))?;
        let sent = self.clock.system_now();
        let binding: ClaimParameters = post_typed(
            &self.clock,
            &client,
            url,
            &json!({"tenant_id": code.tenant_id, "transfer_proof": code.transfer_proof}),
        )
        .await?;
        // The expiry is judged by the cloud's clock: learn it first, from the
        // portal's own time when it sends one (the `Date` header otherwise).
        if let Some(server_time) = binding.server_time.as_deref() {
            self.clock
                .observe_server_time(server_time, sent, self.clock.system_now());
        }
        if binding.protocol_version != 1
            || binding.audience != ENROLLMENT_AUDIENCE
            || binding.enrollment_id != code.enrollment_id
            || binding.tenant_id != code.tenant_id
        {
            return Err(CloudError::Network("invalid claim parameters".into()));
        }
        self.begin_remote(EnrollmentInput {
            endpoint: raw_endpoint,
            enrollment_id: code.enrollment_id,
            tenant_id: code.tenant_id,
            expires_at: binding.expires_at,
            challenge: binding.challenge,
            transfer_proof: code.transfer_proof,
            display_label,
        })
        .await
    }
    pub async fn confirm_remote(&mut self) -> Result<CloudStatus> {
        let request = self.confirm()?;
        let endpoint = {
            let i = self.lock()?;
            i.active_endpoint()?
        };
        let client = resolved_client(&endpoint).await?;
        let url = endpoint.join(&format!(
            "v1/node-enrollments/{}/confirm",
            request.enrollment_id
        ))?;
        let response = post_json(
            &self.clock,
            &client,
            url,
            &serde_json::to_value(request).map_err(|e| CloudError::Invalid(e.to_string()))?,
        )
        .await?;
        self.record_confirm_response(&response)?;
        Ok(self.status())
    }
    pub fn unlink_local(&mut self) -> Result<()> {
        let mut inner = self.lock()?;
        if inner.persisted.status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let status = &inner.persisted.status;
        let pending_unlink = PendingUnlink {
            endpoint: status.endpoint.clone().ok_or(CloudError::State)?,
            tenant_id: status.tenant_id.clone().ok_or(CloudError::State)?,
            node_id: status.node_id.clone().ok_or(CloudError::State)?,
            auth_epoch: status.auth_epoch.ok_or(CloudError::State)?,
        };
        let candidate = Persisted {
            status: CloudStatus::default(),
            pending: None,
            pending_unlink: Some(pending_unlink),
            pending_directive_acknowledgements: Vec::new(),
        };
        persist_state(&inner.dir, &candidate)?;
        inner.persisted = candidate;
        inner.endpoint = None;
        inner.endpoint_problem = None;
        Ok(())
    }
    pub fn has_pending_unlink(&self) -> bool {
        self.lock()
            .map(|inner| inner.persisted.pending_unlink.is_some())
            .unwrap_or(false)
    }
    pub async fn retry_pending_unlink(&self) -> Result<()> {
        // A tombstone may refer to an earlier link. Its refusal or cleanup must
        // never replace the current link's control socket or connectivity status.
        let mut cleanup = self.shared();
        let separate = self.status().phase == LinkPhase::Linked;
        if separate {
            cleanup.websocket = self
                .websocket
                .as_ref()
                .map(|_| Arc::new(websocket::WsTransport::default()));
        }
        let outcome = cleanup.request_pending_unlink().await;
        if separate {
            cleanup.close_websocket().await;
        }
        outcome
    }

    async fn request_pending_unlink(&self) -> Result<()> {
        let (pending, key, policy) = {
            let inner = self.lock()?;
            (
                inner
                    .persisted
                    .pending_unlink
                    .clone()
                    .ok_or(CloudError::State)?,
                inner.key.clone(),
                inner.policy,
            )
        };
        let endpoint = parse_persisted_endpoint(&pending.endpoint, policy)?;
        let client = self.control_client(&endpoint).await?;
        let issued = self.clock.now();
        let mut request = json!({
            "protocol_version": 1,
            "tenant_id": pending.tenant_id,
            "node_id": pending.node_id,
            "audience": NODE_CHALLENGE_AUDIENCE,
            "operation": UNLINK_OPERATION,
            "auth_epoch": pending.auth_epoch,
            "issued_at": fmt_time(issued)?,
            "expires_at": fmt_time(issued + TimeDuration::seconds(60))?,
            "nonce": URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes()),
            "signature": ""
        });
        request["signature"] = Value::String(sign_value(&key, &without_signature(&request))?);
        let response = self
            .control_post_json(
                &endpoint,
                &client,
                endpoint.join("v1/node-auth/challenges")?,
                &request,
            )
            .await?;
        let challenge = parse_challenge_response(response, UNLINK_OPERATION, self.clock.now())?;
        let proof_issued = self.clock.now();
        let mut proof = json!({
            "protocol_version": 1,
            "tenant_id": pending.tenant_id,
            "node_id": pending.node_id,
            "audience": NODE_AUDIENCE,
            "auth_epoch": pending.auth_epoch,
            "issued_at": fmt_time(proof_issued)?,
            "expires_at": fmt_time(proof_issued + TimeDuration::seconds(60))?,
            "challenge": challenge.challenge,
            "signature": ""
        });
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        self.control_post_no_content(
            &endpoint,
            &client,
            endpoint.join(&format!("v1/nodes/{}/unlink-indications", pending.node_id))?,
            &proof,
        )
        .await?;
        let mut inner = self.lock()?;
        let still_same = inner
            .persisted
            .pending_unlink
            .as_ref()
            .is_some_and(|current| {
                current.node_id == pending.node_id && current.auth_epoch == pending.auth_epoch
            });
        if !still_same {
            return Err(CloudError::State);
        }
        let mut candidate = inner.persisted.clone();
        candidate.pending_unlink = None;
        persist_state(&inner.dir, &candidate)?;
        inner.persisted = candidate;
        Ok(())
    }
    /// Best-effort shutdown, bounded so cloud cleanup cannot delay local recording finalization.
    pub async fn close_websocket(&self) {
        if let Some(transport) = &self.websocket {
            let _ = tokio::time::timeout(Duration::from_secs(1), transport.close()).await;
        }
    }

    async fn control_client(&self, endpoint: &CloudEndpoint) -> Result<Option<reqwest::Client>> {
        if self.websocket.is_some() {
            Ok(None)
        } else {
            Ok(Some(resolved_client(endpoint).await?))
        }
    }

    async fn control_post_json(
        &self,
        endpoint: &CloudEndpoint,
        client: &Option<reqwest::Client>,
        url: reqwest::Url,
        body: &Value,
    ) -> Result<Value> {
        let outcome = if let Some(transport) = &self.websocket {
            let method = if url.path().ends_with("/node-auth/challenges") {
                "challenge"
            } else {
                match url.path().rsplit('/').next().unwrap_or("") {
                    "metadata-check-ins" => "metadata_check_in",
                    "inventory-snapshots" => INVENTORY_OPERATION,
                    "recording-coverage" => COVERAGE_OPERATION,
                    "publish-directives" => PUBLISH_DIRECTIVES_OPERATION,
                    "thumbnail-demands" => THUMBNAIL_DEMAND_OPERATION,
                    "thumbnail-batches" => THUMBNAIL_BATCH_OPERATION,
                    "pull" => CONFIG_PULL_OPERATION,
                    "config-results" => CONFIG_RESULT_OPERATION,
                    "diagnostics-reports" => DIAGNOSTICS_REPORT_OPERATION,
                    "export-pulls" => EXPORT_PULL_OPERATION,
                    "export-reports" => EXPORT_REPORT_OPERATION,
                    "departures" => DEPARTURE_OPERATION,
                    "unlink-indications" => UNLINK_OPERATION,
                    _ => return Err(CloudError::Invalid("unsupported control operation".into())),
                }
            };
            let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            let wait = params
                .get("wait")
                .map_or(Ok(0.0), |v| v.parse::<f64>())
                .map_err(|_| CloudError::Invalid("invalid control wait".into()))?;
            transport
                .request(
                    self,
                    endpoint,
                    method,
                    body,
                    websocket::RequestOptions {
                        wait,
                        known: params.get("known").map(String::as_str),
                        events: params.get("events").is_some_and(|v| v == "true"),
                        events_only: params.get("events_only").is_some_and(|v| v == "true"),
                    },
                )
                .await
        } else {
            post_json(
                &self.clock,
                client.as_ref().ok_or(CloudError::State)?,
                url,
                body,
            )
            .await
        };
        let current = self.status();
        if current.phase == LinkPhase::Linked
            && body["tenant_id"].as_str() == current.tenant_id.as_deref()
            && body["node_id"].as_str() == current.node_id.as_deref()
            && body["auth_epoch"].as_u64() == current.auth_epoch
        {
            self.note(&outcome);
        }
        outcome
    }

    async fn control_post_no_content(
        &self,
        endpoint: &CloudEndpoint,
        client: &Option<reqwest::Client>,
        url: reqwest::Url,
        body: &Value,
    ) -> Result<()> {
        if self.websocket.is_some() {
            self.control_post_json(endpoint, client, url, body)
                .await
                .map(|_| ())
        } else {
            post_no_content(
                &self.clock,
                client.as_ref().ok_or(CloudError::State)?,
                url,
                body,
            )
            .await
        }
    }

    /// Obtain a one-shot cloud challenge for `operation` with a signed, nonce-bound request.
    async fn node_challenge(
        &self,
        client: &Option<reqwest::Client>,
        endpoint: &CloudEndpoint,
        key: &SigningKey,
        status: &CloudStatus,
        operation: &str,
    ) -> Result<String> {
        let tenant_id = status.tenant_id.clone().ok_or(CloudError::State)?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let epoch = status.auth_epoch.ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut request = json!({"protocol_version":1,"tenant_id":tenant_id,"node_id":node_id,"audience":NODE_CHALLENGE_AUDIENCE,"operation":operation,"auth_epoch":epoch,"issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued + TimeDuration::seconds(60))?,"nonce":URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes()),"signature":""});
        request["signature"] = Value::String(sign_value(key, &without_signature(&request))?);
        let response = self
            .control_post_json(
                endpoint,
                client,
                endpoint.join("v1/node-auth/challenges")?,
                &request,
            )
            .await?;
        Ok(parse_challenge_response(response, operation, self.clock.now())?.challenge)
    }

    /// Tell the cloud this node is shutting down cleanly, so the portal shows
    /// it offline at once instead of after its presence timeout. The next
    /// check-in after a restart supersedes it. Best effort: callers bound it.
    pub async fn announce_departure(&self) -> Result<()> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, DEPARTURE_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut proof = json!({
            "protocol_version":1,"tenant_id":status.tenant_id,"node_id":node_id,
            "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
            "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
            "challenge":challenge,"signature":""
        });
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        self.control_post_no_content(
            &endpoint,
            &client,
            endpoint.join(&format!("v1/nodes/{node_id}/departures"))?,
            &proof,
        )
        .await
    }

    /// Report the allowlisted camera inventory; `Ok(false)` if the cloud kept a newer one.
    pub async fn push_inventory(&self, cameras: Vec<CameraSnapshot>) -> Result<bool> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, INVENTORY_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut snapshot = json!({
            "protocol_version": 1,
            "tenant_id": status.tenant_id,
            "node_id": node_id,
            "audience": NODE_AUDIENCE,
            "auth_epoch": status.auth_epoch,
            "issued_at": fmt_time(issued)?,
            "expires_at": fmt_time(issued + TimeDuration::seconds(60))?,
            "challenge": challenge,
            "cameras": cameras,
            "signature": ""
        });
        snapshot["signature"] = Value::String(sign_value(&key, &without_signature(&snapshot))?);
        let response = self
            .control_post_json(
                &endpoint,
                &client,
                endpoint.join(&format!("v1/nodes/{node_id}/inventory-snapshots"))?,
                &snapshot,
            )
            .await?;
        if response["node_id"] != node_id {
            return Err(CloudError::Network("invalid inventory response".into()));
        }
        Ok(response["accepted"].as_bool() == Some(true))
    }

    /// Ask the cloud which cameras have a currently viewing browser page.
    /// Cameras browsers are looking at. The cloud holds the answer for up to
    /// `wait` while it still equals `known`, and answers at once on a change.
    pub async fn pull_thumbnail_demands(
        &self,
        wait: Duration,
        known: &[String],
    ) -> Result<Vec<String>> {
        let outcome = self.request_thumbnail_demands(wait, known).await;
        self.note(&outcome);
        outcome
    }

    async fn request_thumbnail_demands(
        &self,
        wait: Duration,
        known: &[String],
    ) -> Result<Vec<String>> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(
                &client,
                &endpoint,
                &key,
                &status,
                THUMBNAIL_DEMAND_OPERATION,
            )
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut proof = json!({
            "protocol_version":1,"tenant_id":status.tenant_id,"node_id":node_id,
            "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
            "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
            "challenge":challenge,"signature":""
        });
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        let response = self
            .control_post_json(
                &endpoint,
                &client,
                thumbnail_demand_url(&endpoint.0, &node_id, wait, known)?,
                &proof,
            )
            .await?;
        let ids = response["camera_ids"]
            .as_array()
            .ok_or_else(|| CloudError::Network("invalid thumbnail demand".into()))?;
        self.mark_contact();
        if ids.len() > 256 {
            return Err(CloudError::Network("too many thumbnails requested".into()));
        }
        let mut cameras = Vec::with_capacity(ids.len());
        for id in ids {
            let id = id
                .as_str()
                .ok_or_else(|| CloudError::Network("invalid camera id".into()))?;
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(CloudError::Network("invalid camera id".into()));
            }
            if !cameras.iter().any(|seen| seen == id) {
                cameras.push(id.to_owned());
            }
        }
        Ok(cameras)
    }

    /// Send only viewer-requested JPEGs, signed over the exact image bytes.
    pub async fn push_thumbnail_batch(&self, images: Vec<ThumbnailImage>) -> Result<usize> {
        if images.is_empty() || images.len() > 16 {
            return Err(CloudError::State);
        }
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, THUMBNAIL_BATCH_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut batch = json!({
            "protocol_version":1,"tenant_id":status.tenant_id,"node_id":node_id,
            "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
            "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
            "challenge":challenge,"images":images,"signature":""
        });
        batch["signature"] = Value::String(sign_value(&key, &without_signature(&batch))?);
        let response = self
            .control_post_json(
                &endpoint,
                &client,
                endpoint.join(&format!("v1/nodes/{node_id}/thumbnail-batches"))?,
                &batch,
            )
            .await?;
        let accepted = response["accepted"]
            .as_u64()
            .ok_or_else(|| CloudError::Network("invalid thumbnail response".into()))?
            as usize;
        if accepted > images.len() {
            return Err(CloudError::Network("invalid thumbnail response".into()));
        }
        Ok(accepted)
    }

    /// Report when each camera has recordings, for the cloud playback timeline.
    pub async fn push_coverage(&self, cameras: Vec<CameraCoverage>) -> Result<bool> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, COVERAGE_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut report = json!({
            "protocol_version": 1,
            "tenant_id": status.tenant_id,
            "node_id": node_id,
            "audience": NODE_AUDIENCE,
            "auth_epoch": status.auth_epoch,
            "issued_at": fmt_time(issued)?,
            "expires_at": fmt_time(issued + TimeDuration::seconds(60))?,
            "challenge": challenge,
            "cameras": cameras,
            "signature": ""
        });
        report["signature"] = Value::String(sign_value(&key, &without_signature(&report))?);
        let response = self
            .control_post_json(
                &endpoint,
                &client,
                endpoint.join(&format!("v1/nodes/{node_id}/recording-coverage"))?,
                &report,
            )
            .await?;
        if response["node_id"] != node_id {
            return Err(CloudError::Network("invalid coverage response".into()));
        }
        Ok(response["accepted"].as_bool() == Some(true))
    }

    /// Configuration requests waiting for this node, held open by the cloud
    /// for up to `wait` until one arrives.
    pub async fn pull_config_requests(&self, wait: Duration) -> Result<Vec<RemoteConfigRequest>> {
        self.pull_portal_requests(wait, false).await
    }

    pub async fn pull_portal_requests(
        &self,
        wait: Duration,
        events_only: bool,
    ) -> Result<Vec<RemoteConfigRequest>> {
        let outcome = self.request_config_requests(wait, events_only).await;
        self.note(&outcome);
        outcome
    }

    async fn request_config_requests(
        &self,
        wait: Duration,
        events_only: bool,
    ) -> Result<Vec<RemoteConfigRequest>> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, CONFIG_PULL_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut proof = json!({
            "protocol_version":1,"tenant_id":status.tenant_id,"node_id":node_id,
            "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
            "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
            "challenge":challenge,"signature":""
        });
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        let mut url = endpoint.join(&format!("v1/nodes/{node_id}/config-requests/pull"))?;
        url.query_pairs_mut()
            .append_pair("wait", &wait.as_secs().to_string())
            .append_pair("events", "true")
            .append_pair("events_only", if events_only { "true" } else { "false" });
        let response = self
            .control_post_json(&endpoint, &client, url, &proof)
            .await?;
        self.mark_contact();
        let requests: Vec<RemoteConfigRequest> =
            serde_json::from_value(response["requests"].clone())
                .map_err(|_| CloudError::Network("invalid configuration requests".into()))?;
        if requests.len() > 8
            || requests.iter().any(|request| {
                request.request_id.len() != 32
                    || !request
                        .request_id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err(CloudError::Network("invalid configuration requests".into()));
        }
        Ok(requests)
    }

    /// Send the outcome of one configuration request back to the portal.
    pub async fn push_config_result(
        &self,
        request_id: &str,
        outcome: &std::result::Result<Value, String>,
    ) -> Result<()> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(&client, &endpoint, &key, &status, CONFIG_RESULT_OPERATION)
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let body = config_result_body(
            &key,
            &status,
            &challenge,
            request_id,
            outcome,
            self.clock.now(),
        )?;
        let url = endpoint.join(&format!("v1/nodes/{node_id}/config-results"))?;
        self.control_post_no_content(&endpoint, &client, url, &body)
            .await?;
        self.mark_contact();
        Ok(())
    }

    /// Send a diagnostics snapshot (or none) and learn whether a portal page
    /// is watching. The cloud holds the answer up to `wait` until one opens.
    pub async fn push_diagnostics(&self, snapshot: Option<&Value>, wait: Duration) -> Result<bool> {
        let outcome = self.request_diagnostics(snapshot, wait).await;
        self.note(&outcome);
        outcome
    }

    async fn request_diagnostics(&self, snapshot: Option<&Value>, wait: Duration) -> Result<bool> {
        let snapshot_json = snapshot
            .map(|value| {
                serde_json::to_string(value).map_err(|e| CloudError::Invalid(e.to_string()))
            })
            .transpose()?;
        if snapshot_json
            .as_ref()
            .is_some_and(|text| text.len() > MAX_DIAGNOSTICS_BYTES)
        {
            return Err(CloudError::Invalid("diagnostics snapshot too large".into()));
        }
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(
                &client,
                &endpoint,
                &key,
                &status,
                DIAGNOSTICS_REPORT_OPERATION,
            )
            .await?;
        let node_id = status.node_id.clone().ok_or(CloudError::State)?;
        let body =
            diagnostics_report_body(&key, &status, &challenge, snapshot_json, self.clock.now())?;
        let mut url = endpoint.join(&format!("v1/nodes/{node_id}/diagnostics-reports"))?;
        url.query_pairs_mut()
            .append_pair("wait", &wait.as_secs().to_string());
        let response = self
            .control_post_json(&endpoint, &client, url, &body)
            .await?;
        self.mark_contact();
        response["wanted"]
            .as_bool()
            .ok_or_else(|| CloudError::Network("invalid diagnostics response".into()))
    }

    /// Open a sealed web-portal request for `method`. Each opens only once.
    pub fn open_sealed(&self, method: &str, envelope: &Value) -> Result<sealed::Opened> {
        let mut inner = self.lock()?;
        let node_id = inner
            .persisted
            .status
            .node_id
            .clone()
            .ok_or(CloudError::State)?;
        let key = inner.key.clone();
        let now = self.clock.now().unix_timestamp();
        sealed::open_request(
            &key,
            &node_id,
            method,
            envelope,
            now,
            &mut inner.sealed_replay,
        )
        .map_err(|e| CloudError::Invalid(e.to_string()))
    }

    /// Seal an answer for the browser that sent `opened`.
    pub fn seal_reply(
        &self,
        opened: &sealed::Opened,
        outcome: &std::result::Result<Value, String>,
    ) -> Result<Value> {
        let node_id = self
            .lock()?
            .persisted
            .status
            .node_id
            .clone()
            .ok_or(CloudError::State)?;
        sealed::seal_reply(&node_id, opened, outcome)
            .map_err(|e| CloudError::Invalid(e.to_string()))
    }

    pub async fn check_in_once(&self) -> Result<()> {
        let outcome = self.request_check_in().await;
        self.note(&outcome);
        outcome
    }

    async fn request_check_in(&self) -> Result<()> {
        let (endpoint, key, status) = {
            let i = self.lock()?;
            (
                i.active_endpoint()?,
                i.key.clone(),
                i.persisted.status.clone(),
            )
        };
        let client = self.control_client(&endpoint).await?;
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let tenant_id = status.tenant_id.ok_or(CloudError::State)?;
        let node_id = status.node_id.ok_or(CloudError::State)?;
        let epoch = status.auth_epoch.ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let expires = issued + TimeDuration::seconds(60);
        let nonce = URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes());
        let mut challenge = json!({"protocol_version":1,"tenant_id":tenant_id,"node_id":node_id,"audience":NODE_CHALLENGE_AUDIENCE,"operation":OPERATION,"auth_epoch":epoch,"issued_at":fmt_time(issued)?,"expires_at":fmt_time(expires)?,"nonce":nonce,"signature":""});
        challenge["signature"] = Value::String(sign_value(&key, &without_signature(&challenge))?);
        let issued_challenge = self
            .control_post_json(
                &endpoint,
                &client,
                endpoint.join("v1/node-auth/challenges")?,
                &challenge,
            )
            .await?;
        let issued_challenge =
            parse_challenge_response(issued_challenge, OPERATION, self.clock.now())?;
        let server_challenge = issued_challenge.challenge;
        let issued = self.clock.now();
        let mut proof = json!({"protocol_version":1,"tenant_id":tenant_id,"node_id":node_id,"audience":NODE_AUDIENCE,"auth_epoch":epoch,"issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,"challenge":server_challenge,"signature":""});
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        let response = self
            .control_post_json(
                &endpoint,
                &client,
                endpoint.join(&format!("v1/nodes/{node_id}/metadata-check-ins"))?,
                &proof,
            )
            .await?;
        if response["node_id"] != node_id
            || response["auth_epoch"] != epoch
            || matches!(
                response["status"].as_str(),
                Some("revoked" | "disabled" | "stale")
            )
            || response["revoked"].as_bool() == Some(true)
        {
            return Err(CloudError::Network("invalid check-in response".into()));
        }
        let mut inner = self.lock()?;
        let checked_in = response["checked_in_at"]
            .as_str()
            .ok_or_else(|| CloudError::Network("missing check-in timestamp".into()))?;
        let checked_in = canonical_cloud_time(checked_in, None)
            .map_err(|_| CloudError::Network("invalid check-in timestamp".into()))?;
        let mut candidate = inner.persisted.clone();
        candidate.status.last_check_in = Some(checked_in);
        persist_state(&inner.dir, &candidate)?;
        inner.persisted = candidate;
        inner.last_contact = Some(std::time::Instant::now());
        Ok(())
    }

    /// Pull obligations over the same signed, challenge-bound node channel.
    pub async fn pull_publish_directives(
        &self,
    ) -> Result<nw_cloud_protocol::media_session::PublishDirectiveBatch> {
        let outcome = self.request_publish_directives().await;
        self.note(&outcome);
        outcome
    }

    async fn request_publish_directives(
        &self,
    ) -> Result<nw_cloud_protocol::media_session::PublishDirectiveBatch> {
        let (endpoint, key, status, acknowledgements) = {
            let inner = self.lock()?;
            (
                inner.active_endpoint()?,
                inner.key.clone(),
                inner.persisted.status.clone(),
                inner.persisted.pending_directive_acknowledgements.clone(),
            )
        };
        if status.phase != LinkPhase::Linked {
            return Err(CloudError::State);
        }
        let client = self.control_client(&endpoint).await?;
        let challenge = self
            .node_challenge(
                &client,
                &endpoint,
                &key,
                &status,
                PUBLISH_DIRECTIVES_OPERATION,
            )
            .await?;
        let tenant_id = status.tenant_id.ok_or(CloudError::State)?;
        let node_id = status.node_id.ok_or(CloudError::State)?;
        let auth_epoch = status.auth_epoch.ok_or(CloudError::State)?;
        let issued = self.clock.now();
        let mut proof = json!({
            "protocol_version": 1,
            "tenant_id": tenant_id,
            "node_id": node_id,
            "audience": NODE_AUDIENCE,
            "auth_epoch": auth_epoch,
            "issued_at": fmt_time(issued)?,
            "expires_at": fmt_time(issued + TimeDuration::seconds(60))?,
            "challenge": challenge,
            "acknowledged_directive_ids": acknowledgements,
            "signature": ""
        });
        proof["signature"] = Value::String(sign_value(&key, &without_signature(&proof))?);
        let response = self
            .control_post_json(
                &endpoint,
                &client,
                endpoint.join(&format!("v1/nodes/{node_id}/publish-directives"))?,
                &proof,
            )
            .await?;
        let batch: nw_cloud_protocol::media_session::PublishDirectiveBatch =
            serde_json::from_value(response)
                .map_err(|_| CloudError::Network("invalid publish directive batch".into()))?;
        self.mark_contact();
        let policy = if self.lock()?.policy.allow_loopback_http {
            nw_cloud_protocol::media_session::EndpointPolicy::DEVELOPMENT_LOOPBACK
        } else {
            nw_cloud_protocol::media_session::EndpointPolicy::PUBLIC
        };
        batch
            .validate_with(policy)
            .map_err(|_| CloudError::Network("invalid publish directive batch".into()))?;
        if !acknowledgements.is_empty() {
            let mut inner = self.lock()?;
            let mut candidate = inner.persisted.clone();
            candidate
                .pending_directive_acknowledgements
                .retain(|id| !acknowledgements.contains(id));
            persist_state(&inner.dir, &candidate)?;
            inner.persisted = candidate;
        }
        Ok(batch)
    }

    fn acknowledge_consumed_directives(
        &self,
        batch: &nw_cloud_protocol::media_session::PublishDirectiveBatch,
    ) -> Result<()> {
        let mut inner = self.lock()?;
        let mut candidate = inner.persisted.clone();
        for directive in &batch.directives {
            if !candidate
                .pending_directive_acknowledgements
                .contains(&directive.directive_id)
            {
                candidate
                    .pending_directive_acknowledgements
                    .push(directive.directive_id.clone());
            }
        }
        persist_state(&inner.dir, &candidate)?;
        inner.persisted = candidate;
        Ok(())
    }
    #[cfg(test)]
    fn install_linked_for_test(
        &mut self,
        endpoint: &str,
        tenant: &str,
        node: &str,
        epoch: u64,
    ) -> Result<()> {
        self.websocket = None;
        let mut i = self.lock()?;
        i.endpoint = Some(CloudEndpoint::loopback_test(endpoint)?);
        i.persisted.status = CloudStatus {
            phase: LinkPhase::Linked,
            endpoint: Some(endpoint.into()),
            tenant_id: Some(tenant.into()),
            node_id: Some(node.into()),
            auth_epoch: Some(epoch),
            last_check_in: None,
        };
        let candidate = i.persisted.clone();
        persist_state(&i.dir, &candidate)
    }
}

/// The signed `config-results` body. The result travels as JSON text so the
/// signature covers exactly the bytes sent, whatever the cloud's JSON library.
fn config_result_body(
    key: &SigningKey,
    status: &CloudStatus,
    challenge: &str,
    request_id: &str,
    outcome: &std::result::Result<Value, String>,
    issued: OffsetDateTime,
) -> Result<Value> {
    let (ok, result_json, error) = match outcome {
        Ok(value) => {
            let text =
                serde_json::to_string(value).map_err(|e| CloudError::Invalid(e.to_string()))?;
            if text.len() > MAX_CONFIG_RESULT_BYTES {
                (
                    false,
                    None,
                    Some("the answer is too large to send".to_owned()),
                )
            } else {
                (true, Some(text), None)
            }
        }
        Err(message) => (
            false,
            None,
            Some(message.chars().take(MAX_CONFIG_ERROR_CHARS).collect()),
        ),
    };
    let mut body = json!({
        "protocol_version":1,"tenant_id":status.tenant_id,"node_id":status.node_id,
        "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
        "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
        "challenge":challenge,"request_id":request_id,"ok":ok,
        "result_json":result_json,"error":error,"signature":""
    });
    body["signature"] = Value::String(sign_value(key, &without_signature(&body))?);
    Ok(body)
}

/// The signed `diagnostics-reports` body. The snapshot travels as JSON text
/// so the signature covers exactly the bytes sent; `null` while unwatched.
fn diagnostics_report_body(
    key: &SigningKey,
    status: &CloudStatus,
    challenge: &str,
    snapshot_json: Option<String>,
    issued: OffsetDateTime,
) -> Result<Value> {
    let mut body = json!({
        "protocol_version":1,"tenant_id":status.tenant_id,"node_id":status.node_id,
        "audience":NODE_AUDIENCE,"auth_epoch":status.auth_epoch,
        "issued_at":fmt_time(issued)?,"expires_at":fmt_time(issued+TimeDuration::seconds(60))?,
        "challenge":challenge,"snapshot_json":snapshot_json,"signature":""
    });
    body["signature"] = Value::String(sign_value(key, &without_signature(&body))?);
    Ok(body)
}

/// Whether two cloud URLs name the same cloud. Only a trailing slash is
/// forgiven; anything else is a different cloud and is rejected.
fn same_endpoint(configured: &str, requested: &str) -> bool {
    configured.trim_end_matches('/') == requested.trim().trim_end_matches('/')
}

fn parse_persisted_endpoint(raw: &str, policy: EndpointPolicy) -> Result<CloudEndpoint> {
    #[cfg(test)]
    if raw.starts_with("http://127.0.0.1:") {
        return CloudEndpoint::loopback_test(raw);
    }
    CloudEndpoint::parse_with(raw, policy)
}

fn validate_enrollment(i: &EnrollmentInput) -> Result<()> {
    let enrollment_id = uuid::Uuid::parse_str(&i.enrollment_id)
        .map_err(|_| CloudError::Invalid("invalid enrollment id".into()))?;
    let tenant_id = uuid::Uuid::parse_str(&i.tenant_id)
        .map_err(|_| CloudError::Invalid("invalid tenant id".into()))?;
    if enrollment_id.to_string() != i.enrollment_id || tenant_id.to_string() != i.tenant_id {
        return Err(CloudError::Invalid("UUIDs must use canonical form".into()));
    }
    if i.challenge.len() < 32
        || i.transfer_proof.len() < 8
        || i.display_label.is_empty()
        || i.display_label.len() > 64
    {
        return Err(CloudError::Invalid("invalid enrollment fields".into()));
    }
    let label = i.display_label.to_ascii_lowercase();
    if !i
        .display_label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " ._()#&'-".contains(c))
        || [
            "://",
            "camera",
            "rtsp",
            "credential",
            "password",
            "secret",
            "private key",
            " lan",
            " ipc",
            "storage",
            "bucket",
            "endpoint",
        ]
        .iter()
        .any(|s| label.contains(s))
    {
        return Err(CloudError::Invalid("invalid display label".into()));
    }
    validate_cloud_data(&serde_json::to_value(i).map_err(|e| CloudError::Invalid(e.to_string()))?)
}
fn without_signature(v: &Value) -> Value {
    let mut v = v.clone();
    v.as_object_mut().unwrap().remove("signature");
    v
}
fn fmt_time(t: OffsetDateTime) -> Result<String> {
    // Whole seconds: the cloud re-serializes signed times at microsecond
    // precision, so nanosecond digits would break signature verification.
    let t = t
        .replace_nanosecond(0)
        .map_err(|e| CloudError::Invalid(e.to_string()))?;
    canonical_cloud_time(
        &t.format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| CloudError::Invalid(e.to_string()))?,
        None,
    )
}
/// What a node is told when an enrollment code's expiry has passed by the
/// cloud's clock. The UI recognises "enrollment code has expired".
pub const ENROLLMENT_CODE_EXPIRED: &str = "the enrollment code has expired (codes last five \
    minutes): create a new one in the web portal and link again. If it was just created, check \
    this computer's date, time and time zone";
/// `raw` as the cloud's canonical UTC rendering; with `must_follow`, it must
/// also be later than that time (the cloud's clock, see [`CloudClock`]).
fn canonical_cloud_time(raw: &str, must_follow: Option<OffsetDateTime>) -> Result<String> {
    let parsed = OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map_err(|_| CloudError::Invalid("invalid cloud timestamp".into()))?;
    if parsed.offset() != time::UtcOffset::UTC {
        return Err(CloudError::Invalid("cloud timestamp must be UTC".into()));
    }
    if must_follow.is_some_and(|now| parsed <= now) {
        return Err(CloudError::Invalid(ENROLLMENT_CODE_EXPIRED.into()));
    }
    let formatted = parsed
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| CloudError::Invalid(e.to_string()))?;
    Ok(formatted)
}
/// 401/403 mean the cloud refuses this node; anything else is a transport or
/// server problem that may pass.
fn http_error(status: reqwest::StatusCode) -> CloudError {
    match status.as_u16() {
        code @ (401 | 403) => CloudError::Refused(code),
        _ => CloudError::Network(format!("HTTP {status}")),
    }
}

/// Send one request, learning the cloud's clock from the answer's `Date`
/// header (refusals included: a refusal for a wrong clock teaches the offset
/// the next attempt signs with).
async fn send_observed(
    clock: &CloudClock,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response> {
    let sent = clock.system_now();
    let response = request
        .send()
        .await
        .map_err(|e| CloudError::Network(e.to_string()))?;
    clock.observe_headers(response.headers(), sent, clock.system_now());
    Ok(response)
}

async fn post_json(
    clock: &CloudClock,
    client: &reqwest::Client,
    url: reqwest::Url,
    body: &Value,
) -> Result<Value> {
    let r = send_observed(clock, client.post(url).json(body)).await?;
    if !r.status().is_success() {
        return Err(http_error(r.status()));
    }
    r.json()
        .await
        .map_err(|e| CloudError::Network(e.to_string()))
}
async fn post_typed<T: serde::de::DeserializeOwned>(
    clock: &CloudClock,
    client: &reqwest::Client,
    url: reqwest::Url,
    body: &impl Serialize,
) -> Result<T> {
    let r = send_observed(clock, client.post(url).json(body)).await?;
    if !r.status().is_success() {
        return Err(http_error(r.status()));
    }
    r.json()
        .await
        .map_err(|e| CloudError::Network(e.to_string()))
}
async fn post_no_content(
    clock: &CloudClock,
    client: &reqwest::Client,
    url: reqwest::Url,
    body: &Value,
) -> Result<()> {
    let response = send_observed(clock, client.post(url).json(body)).await?;
    if response.status() != reqwest::StatusCode::NO_CONTENT {
        return Err(http_error(response.status()));
    }
    Ok(())
}
fn persist_state(dir: &Path, persisted: &Persisted) -> Result<()> {
    let path = dir.join("cloud-state.json");
    let tmp = dir.join(format!(".cloud-state-{}.tmp", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec(persisted).map_err(|e| CloudError::Store(e.to_string()))?;
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let result = (|| {
        let mut file = o.open(&tmp).map_err(store_err)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(store_err)?;
        validate_private_file(&file, &tmp)?;
        // Windows: the handle must be closed before the file is renamed.
        drop(file);
        fs::rename(&tmp, &path).map_err(store_err)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub struct CloudScheduler {
    node: CloudNode,
    interval: Duration,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
    inventory: Option<InventorySource>,
    coverage: Option<CoverageSource>,
    directives: Option<DirectiveSink>,
    preview: Option<PreviewSource>,
    remote_config: Option<RemoteConfigHandler>,
    diagnostics: Option<DiagnosticsSource>,
    exports: Option<ExportSource>,
}
impl CloudScheduler {
    pub fn new(
        node: CloudNode,
        interval: Duration,
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            node,
            interval,
            attempts,
            inventory: None,
            coverage: None,
            directives: None,
            preview: None,
            remote_config: None,
            diagnostics: None,
            exports: None,
        }
    }

    /// Cut clips the web portal asks for and upload them (see `exports`).
    pub fn with_exports(mut self, source: ExportSource) -> Self {
        self.exports = Some(source);
        self
    }

    /// Send live diagnostics while a portal page watches this node.
    pub fn with_diagnostics(mut self, source: DiagnosticsSource) -> Self {
        self.diagnostics = Some(source);
        self
    }

    /// Take the portal's requests (settings, events, PTZ; camera details
    /// sealed), desktop or headless alike.
    pub fn with_remote_config(mut self, handler: RemoteConfigHandler) -> Self {
        self.remote_config = Some(handler);
        self
    }

    /// Also report when each camera has recordings, after each inventory.
    pub fn with_coverage(mut self, source: CoverageSource) -> Self {
        self.coverage = Some(source);
        self
    }

    pub fn with_preview(mut self, source: PreviewSource) -> Self {
        self.preview = Some(source);
        self
    }

    pub fn with_directives(mut self, sink: DirectiveSink) -> Self {
        self.directives = Some(sink);
        self
    }

    /// Also report the camera inventory after each successful check-in.
    pub fn with_inventory(mut self, source: InventorySource) -> Self {
        self.inventory = Some(source);
        self
    }

    /// The scheduler clock. The first tick fires at once, so a node reports
    /// right after start; a cycle that overran its period then resumes a full
    /// period later instead of firing every missed tick back to back.
    pub fn ticker(period: Duration) -> tokio::time::Interval {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick
    }

    /// Bounded steps in one cycle: retrying a pending unlink, the check-in,
    /// collecting and reporting the camera inventory, and collecting and
    /// reporting recording coverage.
    pub const CYCLE_STEPS: u32 = 6;

    /// Time allowed for one step of a cycle. Every step is bounded — including
    /// collecting the inventory, which reads shared backend state — and all of
    /// them together finish inside a period, so neither a slow cloud nor a busy
    /// backend can push every later cycle late.
    pub fn request_budget(period: Duration) -> Duration {
        period / (Self::CYCLE_STEPS + 1)
    }

    pub fn spawn(self) -> SchedulerHandle {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        // Snapshots run in their own loop: the check-in cycle (check-in,
        // inventory, recording coverage) and directive pulls must never hold
        // back a snapshot that a viewer is waiting for.
        let preview_loop = self.preview.clone().map(|source| {
            let node = self.node.shared();
            let mut stop = rx.clone();
            tokio::spawn(async move {
                let mut schedule = ThumbnailSchedule::default();
                let mut known: Vec<String> = Vec::new();
                let signals = Arc::clone(&node.signals);
                loop {
                    let started = tokio::time::Instant::now();
                    // With events, ask at once and then wait for one instead of long-polling.
                    let pushed = node.events_pushed();
                    if node.status().phase == LinkPhase::Linked {
                        let wait = if pushed {
                            Duration::ZERO
                        } else {
                            schedule
                                .next_due_in(&known, std::time::Instant::now())
                                .map_or(THUMBNAIL_DEMAND_WAIT, |due| due.min(THUMBNAIL_DEMAND_WAIT))
                        };
                        let pull = tokio::time::timeout(
                            THUMBNAIL_DEMAND_WAIT + Duration::from_secs(5),
                            node.pull_thumbnail_demands(wait, &known),
                        );
                        let pulled = tokio::select! {
                            pulled = pull => pulled,
                            changed = stop.changed() => {
                                if changed.is_err() || *stop.borrow() { break; }
                                continue;
                            }
                        };
                        match pulled {
                            Ok(Ok(demand)) => {
                                let due = schedule.due(&demand, std::time::Instant::now());
                                known = demand;
                                if !due.is_empty() {
                                    let images = source(due.clone()).await;
                                    let mut sent = Vec::new();
                                    for batch in images.chunks(16) {
                                        match node.push_thumbnail_batch(batch.to_vec()).await {
                                            Ok(_) => sent.extend(batch.iter().map(|image| image.camera_id.clone())),
                                            Err(error) => {
                                                tracing::warn!(error = %error, "Cloud thumbnail upload failed");
                                                break;
                                            }
                                        }
                                    }
                                    let now = std::time::Instant::now();
                                    schedule.uploaded(&sent, now);
                                    let missed: Vec<String> =
                                        due.into_iter().filter(|id| !sent.contains(id)).collect();
                                    schedule.missed(&missed, now);
                                }
                            }
                            Ok(Err(error)) => tracing::warn!(error = %error, "Cloud thumbnail demand poll failed"),
                            Err(_) => tracing::warn!("Cloud thumbnail demand poll timed out"),
                        }
                    }
                    // An answer that came back at once (demand changed, or a
                    // cloud that does not wait) is not immediately repeated.
                    let earliest = started + THUMBNAIL_MIN_CYCLE;
                    let deadline = if pushed {
                        let due = schedule
                            .next_due_in(&known, std::time::Instant::now())
                            .map_or(EVENT_FALLBACK, |due| due.min(EVENT_FALLBACK));
                        tokio::time::Instant::now() + due
                    } else {
                        earliest
                    };
                    if !rest(earliest, deadline, pushed.then_some(&signals.thumbnails), &mut stop).await {
                        break;
                    }
                }
            })
        });
        let export_loop = self
            .exports
            .clone()
            .map(|source| tokio::spawn(exports::run_loop(self.node.shared(), source, rx.clone())));
        let config_loop = self.remote_config.clone().map(|handler| {
            let node = self.node.shared();
            let mut stop = rx.clone();
            tokio::spawn(async move {
                let mut unsupported_logged = false;
                let signals = Arc::clone(&node.signals);
                loop {
                    let started = tokio::time::Instant::now();
                    let mut pause = CONFIG_PULL_MIN_CYCLE;
                    let pushed = node.events_pushed();
                    let mut wake = pushed;
                    if node.status().phase == LinkPhase::Linked {
                        let wait = if pushed {
                            Duration::ZERO
                        } else {
                            CONFIG_PULL_WAIT
                        };
                        let pull = tokio::time::timeout(
                            CONFIG_PULL_WAIT + Duration::from_secs(5),
                            node.pull_portal_requests(wait, false),
                        );
                        let pulled = tokio::select! {
                            pulled = pull => pulled,
                            changed = stop.changed() => {
                                if changed.is_err() || *stop.borrow() { break; }
                                continue;
                            }
                        };
                        match pulled {
                            Ok(Ok(requests)) => {
                                unsupported_logged = false;
                                if !requests.is_empty() {
                                    pause = CONFIG_PULL_BUSY_CYCLE;
                                }
                                for request in requests {
                                    // Searches and device lookups can take seconds; they run
                                    // alongside, while changes stay in the order they were made.
                                    if request.method.starts_with("camera.discovery.") {
                                        tokio::spawn(run_remote_request(
                                            node.shared(),
                                            Arc::clone(&handler),
                                            request,
                                        ));
                                    } else {
                                        run_remote_request(
                                            node.shared(),
                                            Arc::clone(&handler),
                                            request,
                                        )
                                        .await;
                                    }
                                }
                            }
                            Ok(Err(CloudError::Network(message)))
                                if message == "HTTP 404 Not Found" =>
                            {
                                if !unsupported_logged {
                                    tracing::info!(
                                        "This cloud does not offer remote configuration"
                                    );
                                    unsupported_logged = true;
                                }
                                pause = CONFIG_UNSUPPORTED_RETRY;
                                wake = false;
                            }
                            Ok(Err(error)) => {
                                tracing::warn!(error = %error, "Cloud configuration poll failed")
                            }
                            Err(_) => tracing::warn!("Cloud configuration poll timed out"),
                        }
                    }
                    let earliest = started + pause;
                    let deadline = if wake {
                        tokio::time::Instant::now() + EVENT_FALLBACK
                    } else {
                        earliest
                    };
                    if !rest(
                        earliest,
                        deadline,
                        wake.then_some(&signals.config),
                        &mut stop,
                    )
                    .await
                    {
                        break;
                    }
                }
            })
        });
        // Diagnostics run in their own loop too: idle, one long-poll asks the
        // cloud whether a page watches; watched, a snapshot every few seconds.
        let diagnostics_loop = self.diagnostics.clone().map(|source| {
            let node = self.node.shared();
            let mut stop = rx.clone();
            tokio::spawn(async move {
                let mut wanted = false;
                let mut unsupported_logged = false;
                let signals = Arc::clone(&node.signals);
                loop {
                    let started = tokio::time::Instant::now();
                    let mut pause = DIAGNOSTICS_MIN_CYCLE;
                    // Idle with events, wait for one (a page opening) instead of long-polling.
                    let pushed = node.events_pushed();
                    let mut wake = false;
                    if node.status().phase == LinkPhase::Linked {
                        let snapshot = if wanted {
                            tokio::time::timeout(DIAGNOSTICS_COLLECT_BUDGET, source())
                                .await
                                .ok()
                        } else {
                            None
                        };
                        let wait = if wanted || pushed {
                            Duration::ZERO
                        } else {
                            DIAGNOSTICS_WAIT
                        };
                        let report = tokio::time::timeout(
                            wait + Duration::from_secs(5),
                            node.push_diagnostics(snapshot.as_ref(), wait),
                        );
                        let reported = tokio::select! {
                            reported = report => reported,
                            changed = stop.changed() => {
                                if changed.is_err() || *stop.borrow() { break; }
                                continue;
                            }
                        };
                        let was_wanted = wanted;
                        wanted = false;
                        match reported {
                            Ok(Ok(now_wanted)) => {
                                unsupported_logged = false;
                                wanted = now_wanted;
                                // A page just opened: its first snapshot goes at once.
                                pause = match (was_wanted, now_wanted) {
                                    (false, true) => Duration::ZERO,
                                    (true, true) => DIAGNOSTICS_INTERVAL,
                                    _ => DIAGNOSTICS_MIN_CYCLE,
                                };
                                wake = pushed && !now_wanted;
                            }
                            Ok(Err(CloudError::Network(message)))
                                if message == "HTTP 404 Not Found" =>
                            {
                                if !unsupported_logged {
                                    tracing::info!("This cloud does not offer live diagnostics");
                                    unsupported_logged = true;
                                }
                                pause = DIAGNOSTICS_UNSUPPORTED_RETRY;
                            }
                            Ok(Err(error)) => {
                                tracing::warn!(error = %error, "Cloud diagnostics report failed")
                            }
                            Err(_) => tracing::warn!("Cloud diagnostics report timed out"),
                        }
                    }
                    let earliest = started + pause;
                    let deadline = if wake {
                        tokio::time::Instant::now() + EVENT_FALLBACK
                    } else {
                        earliest
                    };
                    if !rest(
                        earliest,
                        deadline,
                        wake.then_some(&signals.diagnostics),
                        &mut stop,
                    )
                    .await
                    {
                        break;
                    }
                    // Woken before the fallback: a page just opened. Its first
                    // snapshot goes with the next report rather than after one
                    // that only learns it is wanted. (If no page is watching
                    // after all, the cloud drops it.)
                    if wake && tokio::time::Instant::now() < deadline {
                        wanted = true;
                    }
                }
            })
        });
        let task = tokio::spawn(async move {
            let mut tick = Self::ticker(self.interval);
            let directive_period = self.interval.min(Duration::from_secs(5));
            let signals = Arc::clone(&self.node.signals);
            let mut last_directive_pull: Option<tokio::time::Instant> = None;
            // An event asked for a pull, made once the minimum gap allows.
            let mut directive_wanted = false;
            let mut change_tick = Self::ticker(INVENTORY_CHANGE_CHECK);
            let mut coverage_tick = Self::ticker(COVERAGE_CHANGE_CHECK);
            let mut last_coverage: Option<(Vec<CameraCoverage>, std::time::Instant)> = None;
            let mut last_inventory: Option<(Vec<CameraSnapshot>, std::time::Instant)> = None;
            let budget = Self::request_budget(self.interval);
            loop {
                // Without events, pull every `directive_period`; with them the
                // timer is only a fallback, and an event pulls at once.
                let directive_at = match last_directive_pull {
                    None => tokio::time::Instant::now(),
                    Some(at) if directive_wanted => at + DIRECTIVE_MIN_GAP,
                    Some(at) if self.node.events_pushed() => at + DIRECTIVE_EVENT_FALLBACK,
                    Some(at) => at + directive_period,
                };
                tokio::select! {
                    _ = tick.tick() => {
                        // An unlink tombstone belongs to an earlier link (the cloud
                        // may refuse it forever, e.g. if that node was revoked there
                        // first); it is retried but must not starve the current link.
                        if self.node.has_pending_unlink() {
                            self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let _ = tokio::time::timeout(budget, self.node.retry_pending_unlink()).await;
                        }
                        if self.node.status().phase == LinkPhase::Linked {
                            self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let checked_in = tokio::time::timeout(budget, self.node.check_in_once()).await;
                            if let (Ok(Ok(())), Some(source)) = (&checked_in, &self.inventory) {
                                if let Some(cameras) = collect_inventory(source, budget).await {
                                    let reported = tokio::time::timeout(budget, self.node.push_inventory(cameras.clone())).await;
                                    if let Some(reason) = inventory_failure_reason(&reported) {
                                        tracing::warn!(error = %reason, "Cloud inventory report failed");
                                    } else {
                                        last_inventory = Some((cameras, std::time::Instant::now()));
                                    }
                                }
                            }
                            if let (Ok(Ok(())), Some(source)) = (&checked_in, &self.coverage) {
                                match tokio::time::timeout(budget, source()).await {
                                    Ok(cameras) => {
                                        let reported = tokio::time::timeout(budget, self.node.push_coverage(cameras.clone())).await;
                                        if let Some(reason) = inventory_failure_reason(&reported) {
                                            tracing::warn!(error = %reason, "Cloud recording coverage report failed");
                                        } else {
                                            last_coverage = Some((cameras, std::time::Instant::now()));
                                        }
                                    }
                                    Err(_) => tracing::warn!("Cloud recording coverage report skipped: collecting coverage timed out"),
                                }
                            }
                        }
                    }
                    _ = coverage_tick.tick(), if self.coverage.is_some() => {
                        // Only once a full report went out, and only when a recording finished since.
                        if let (LinkPhase::Linked, Some(source), Some((reported, _))) =
                            (self.node.status().phase, &self.coverage, &last_coverage)
                        {
                            if let Ok(cameras) = tokio::time::timeout(budget, source()).await {
                                if *reported != cameras {
                                    let pushed = tokio::time::timeout(budget, self.node.push_coverage(cameras.clone())).await;
                                    if let Some(reason) = inventory_failure_reason(&pushed) {
                                        tracing::warn!(error = %reason, "Cloud recording coverage update failed");
                                    }
                                    // Either way, wait for the next check before trying again.
                                    last_coverage = Some((cameras, std::time::Instant::now()));
                                }
                            }
                        }
                    }
                    _ = change_tick.tick(), if self.inventory.is_some() => {
                        if self.node.status().phase == LinkPhase::Linked {
                            let source = self.inventory.as_ref().expect("guarded inventory source");
                            if let Some(cameras) = collect_inventory(source, budget).await {
                                if inventory_change_due(last_inventory.as_ref(), &cameras, std::time::Instant::now()) {
                                    let reported = tokio::time::timeout(budget, self.node.push_inventory(cameras.clone())).await;
                                    if let Some(reason) = inventory_failure_reason(&reported) {
                                        tracing::warn!(error = %reason, "Cloud camera status update failed");
                                    }
                                    // Either way, wait the minimum gap before trying again.
                                    last_inventory = Some((cameras, std::time::Instant::now()));
                                }
                            }
                        }
                    }
                    _ = signals.directives.notified(), if self.directives.is_some() && !directive_wanted => {
                        directive_wanted = true;
                    }
                    _ = tokio::time::sleep_until(directive_at), if self.directives.is_some() => {
                        last_directive_pull = Some(tokio::time::Instant::now());
                        directive_wanted = false;
                        if self.node.status().phase == LinkPhase::Linked {
                            let sink = self.directives.as_ref().expect("guarded directive sink");
                            match tokio::time::timeout(budget, self.node.pull_publish_directives()).await {
                                Ok(Ok(batch)) => {
                                    sink(batch.clone()).await;
                                    if let Err(error) = self.node.acknowledge_consumed_directives(&batch) {
                                        tracing::warn!(error = %error, "Could not persist directive acknowledgements");
                                    }
                                },
                                Ok(Err(error)) => {
                                    tracing::warn!(error = %error, "Cloud publish-directive pull failed; stopping active publications");
                                    sink(empty_publish_directive_batch()).await;
                                }
                                Err(_) => {
                                    tracing::warn!("Cloud publish-directive pull timed out; stopping active publications");
                                    sink(empty_publish_directive_batch()).await;
                                }
                            }
                        }
                    }
                    changed = rx.changed() => {
                        if changed.is_err() || *rx.borrow() { break; }
                    }
                }
            }
            if let Some(task) = preview_loop {
                task.abort();
            }
            if let Some(task) = config_loop {
                task.abort();
            }
            if let Some(task) = diagnostics_loop {
                task.abort();
            }
            if let Some(task) = export_loop {
                task.abort();
            }
        });
        SchedulerHandle { tx, task }
    }
}

/// Run one web-portal request and send back its result. A sealed request is
/// opened here and its answer sealed for the browser, so the handler never
/// sees an envelope and the cloud never sees what was asked or answered.
async fn run_remote_request(
    node: CloudNode,
    handler: RemoteConfigHandler,
    request: RemoteConfigRequest,
) {
    let method = request.method.clone();
    let (outcome, opened) = match request.params.get("sealed") {
        Some(envelope) => match node.open_sealed(&method, envelope) {
            Ok(opened) => {
                let outcome = handler(method.clone(), opened.params.clone(), true).await;
                (outcome, Some(opened))
            }
            Err(error) => (Err(error.to_string()), None),
        },
        None => (handler(method.clone(), request.params, false).await, None),
    };
    match &outcome {
        Err(error) => {
            tracing::info!(method = %method, error = %error, "Web portal configuration request refused")
        }
        Ok(_) => tracing::info!(method = %method, "Web portal configuration request applied"),
    }
    let outcome = match opened {
        Some(opened) => node
            .seal_reply(&opened, &outcome)
            .map(|envelope| json!({ "sealed": envelope }))
            .map_err(|e| e.to_string()),
        None => outcome,
    };
    if let Err(error) = node.push_config_result(&request.request_id, &outcome).await {
        tracing::warn!(error = %error, "Could not send a configuration result to the cloud");
    }
}

fn empty_publish_directive_batch() -> nw_cloud_protocol::media_session::PublishDirectiveBatch {
    nw_cloud_protocol::media_session::PublishDirectiveBatch {
        protocol_version: nw_cloud_protocol::media_session::MEDIA_SESSION_PROTOCOL_VERSION,
        directives: Vec::new(),
        active_session_ids: Vec::new(),
    }
}

/// Collect the camera inventory under the cycle's step budget. The source
/// reads shared backend state, so a busy backend must not be able to stretch a
/// cycle past its period; `None` means the snapshot was abandoned and this
/// cycle reports no inventory rather than running long.
async fn collect_inventory(
    source: &InventorySource,
    budget: Duration,
) -> Option<Vec<CameraSnapshot>> {
    match tokio::time::timeout(budget, source()).await {
        Ok(cameras) => Some(cameras),
        Err(_) => {
            tracing::warn!(error = "timed out", "Cloud inventory collection failed");
            None
        }
    }
}

/// Why an inventory report did not land, as one log line. A report that hit
/// the request budget used to be silent, which read like a successful cycle;
/// `None` means the cloud stored it or deliberately kept a newer snapshot.
fn inventory_failure_reason(
    reported: &std::result::Result<Result<bool>, tokio::time::error::Elapsed>,
) -> Option<String> {
    match reported {
        Err(_) => Some("timed out".into()),
        Ok(Err(error)) => Some(error.to_string()),
        Ok(Ok(_)) => None,
    }
}

pub struct SchedulerHandle {
    tx: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl SchedulerHandle {
    pub async fn shutdown(self) {
        let _ = self.tx.send(true);
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod websocket_tests;

#[cfg(test)]
mod config_result_contract {
    use super::*;

    const FIXTURE: &str = "src/cloud/fixtures/config-result-v1.json";

    /// A deterministic signed result. The web portal verifies this exact
    /// fixture (`tests/test_node_config_contract_fixture.py`), so both sides
    /// agree on the signed bytes of awkward results: non-ASCII text, quotes,
    /// control characters and floats.
    fn signed_bodies() -> Value {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let status = CloudStatus {
            phase: LinkPhase::Linked,
            endpoint: None,
            tenant_id: Some("6ba7b811-9dad-11d1-80b4-00c04fd430c8".into()),
            node_id: Some("6ba7b810-9dad-11d1-80b4-00c04fd430c8".into()),
            auth_epoch: Some(3),
            last_check_in: None,
        };
        let issued = OffsetDateTime::from_unix_timestamp(1_790_000_000).expect("valid time");
        let result = json!([{
            "id": "6f1c2a9e-0d5b-4e2f-9c1a-1b2c3d4e5f60",
            "name": "Café \"Nord\" – Tür\u{1}",
            "url": "rtsp://10.0.0.5:554/stream1",
            "has_password": true,
            "motion": {"cooldown_sec": 10.0, "min_duration_sec": 0.5},
            "recording": {"local": {"enabled": false, "storage_path": null, "quota_gb": null}}
        }]);
        let ok = config_result_body(
            &key,
            &status,
            "challenge-1",
            &"a".repeat(32),
            &Ok(result),
            issued,
        )
        .expect("signed result");
        let refused = config_result_body(
            &key,
            &status,
            "challenge-2",
            &"b".repeat(32),
            &Err("Camera not found".into()),
            issued,
        )
        .expect("signed refusal");
        json!({
            "public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
            "ok": ok,
            "refused": refused,
        })
    }

    #[test]
    fn signed_config_results_match_the_shared_fixture() {
        let bodies = signed_bodies();
        let public = bodies["public_key"].as_str().unwrap_or_default();
        for name in ["ok", "refused"] {
            let body = &bodies[name];
            let signature = body["signature"].as_str().unwrap_or_default();
            assert!(
                verify_signed(public, signature, &without_signature(body)),
                "{name}"
            );
        }
        let fixture: Value = serde_json::from_str(include_str!("fixtures/config-result-v1.json"))
            .expect("fixture is JSON");
        assert_eq!(
            bodies, fixture,
            "regenerate with: cargo test -- --ignored write_config_result_fixture"
        );
    }

    #[test]
    #[ignore = "writes the shared fixture"]
    fn write_config_result_fixture() {
        let text = serde_json::to_string_pretty(&signed_bodies()).expect("serialize") + "\n";
        std::fs::write(FIXTURE, text).expect("write fixture");
    }
}

#[cfg(test)]
mod diagnostics_report_contract {
    use super::*;

    const FIXTURE: &str = "src/cloud/fixtures/diagnostics-report-v1.json";

    /// Deterministic signed reports. The web portal verifies this exact
    /// fixture (`tests/test_diagnostics_contract_fixture.py`), so both sides
    /// agree on the signed bytes of a snapshot and of an unwatched report.
    fn signed_bodies() -> Value {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let status = CloudStatus {
            phase: LinkPhase::Linked,
            endpoint: None,
            tenant_id: Some("6ba7b811-9dad-11d1-80b4-00c04fd430c8".into()),
            node_id: Some("6ba7b810-9dad-11d1-80b4-00c04fd430c8".into()),
            auth_epoch: Some(3),
            last_check_in: None,
        };
        let issued = OffsetDateTime::from_unix_timestamp(1_790_000_000).expect("valid time");
        let snapshot = json!({
            "captured_at_utc": "2026-09-21T13:46:40Z",
            "app_version": "1.0.0",
            "process": {"cpu_percent": 12.5, "rss_bytes": 210_000_000_u64, "memory_percent": 2.6,
                        "system_cpu_percent": 31.0, "system_memory_percent": null,
                        "siblings": [{"label": "nw-backend", "pid": 4242, "cpu_percent": 12.5, "rss_bytes": 210_000_000_u64}]},
            "system": {"cpu_cores": 8, "memory_total_bytes": 16_000_000_000_u64,
                       "memory_available_bytes": 8_300_000_000_u64, "uptime_seconds": 86_400,
                       "load_average": [1.2, 0.9, 0.7]},
            "gpu": {"status": "unavailable", "reason": "Intel GPU — no counters (\"VA-API\")"},
            "video_acceleration": {"kind": "vaapi", "label": "VA-API", "hardware": true},
            "ipc": {"requests_total": 10, "messages_total": 10, "bytes_in_total": 900,
                    "bytes_out_total": 4000, "avg_latency_ms": 1.5, "p95_latency_ms": 3.0,
                    "drops_total": 0, "backpressure_events_total": 0, "inflight": 0},
            "cameras": [{"camera_id": "cam-1", "renderer_fps": null, "decoder_fps": 14.9,
                         "renderer_drops": 0, "decoder_drops": 2, "end_to_end_latency_ms": null,
                         "state": "streaming"}],
            "bottleneck": {"kind": "none", "severity": "ok", "explanation": "No resource is under pressure."},
            "drives": [{"mount_point": "/media/Données", "filesystem": "ext4",
                        "total_bytes": 500_000_000_000_u64, "available_bytes": 200_000_000_000_u64,
                        "recording": true}]
        });
        let watched = diagnostics_report_body(
            &key,
            &status,
            "challenge-1",
            Some(serde_json::to_string(&snapshot).expect("snapshot text")),
            issued,
        )
        .expect("signed report");
        let idle = diagnostics_report_body(&key, &status, "challenge-2", None, issued)
            .expect("signed idle report");
        json!({
            "public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
            "watched": watched,
            "idle": idle,
        })
    }

    #[test]
    fn signed_diagnostics_reports_match_the_shared_fixture() {
        let bodies = signed_bodies();
        let public = bodies["public_key"].as_str().unwrap_or_default();
        for name in ["watched", "idle"] {
            let body = &bodies[name];
            let signature = body["signature"].as_str().unwrap_or_default();
            assert!(
                verify_signed(public, signature, &without_signature(body)),
                "{name}"
            );
        }
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/diagnostics-report-v1.json"))
                .expect("fixture is JSON");
        assert_eq!(
            bodies, fixture,
            "regenerate with: cargo test -- --ignored write_diagnostics_report_fixture"
        );
    }

    #[test]
    #[ignore = "writes the shared fixture"]
    fn write_diagnostics_report_fixture() {
        let text = serde_json::to_string_pretty(&signed_bodies()).expect("serialize") + "\n";
        std::fs::write(FIXTURE, text).expect("write fixture");
    }
}
