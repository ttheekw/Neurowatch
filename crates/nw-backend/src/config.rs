//! Application configuration — JSON persistence with atomic writes.

use anyhow::Context as _;
use nw_core::camera::{CameraConfig, RuntimeMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    #[serde(default, skip_serializing_if = "RuntimeMode::is_local_standalone")]
    pub runtime_mode: RuntimeMode,
    pub site_name: String,
    pub wall: WallConfig,
    pub cameras: Vec<CameraConfig>,
    /// Cameras beyond this build's camera limit, kept as they were but not
    /// run (`crate::edition`). Empty in an unlimited build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cameras_over_limit: Vec<CameraConfig>,
    pub ui: UiConfig,
    #[serde(default)]
    pub debug: DebugConfig,
    #[serde(default)]
    pub recording: RecordingGlobalConfig,
    #[serde(default)]
    pub motion: MotionGlobalConfig,
    #[serde(default)]
    pub events: EventsConfig,
    /// Alert notifications: channels and rules (`crate::alerts`).
    #[serde(default, skip_serializing_if = "is_default_alerts")]
    pub alerts: crate::alerts::AlertsConfig,
    /// Local accounts and the sign-in requirement (`crate::accounts`).
    #[serde(default, skip_serializing_if = "is_default_accounts")]
    pub accounts: crate::accounts::AccountsConfig,
    #[serde(default)]
    pub monitors: MonitorsConfig,
    /// The desktop UI's extra windows, reopened when the app starts again.
    #[serde(default)]
    pub desktop: DesktopConfig,
}

#[cfg(test)]
mod recording_policy_tests {
    use super::*;

    #[test]
    fn v4_migration_keeps_continuous_defaults_fullscreen_choice_and_legacy_string() {
        let path =
            std::env::temp_dir().join(format!("nw-policy-config-{}.json", uuid::Uuid::new_v4()));
        let mut config = AppConfig {
            version: 3,
            ..AppConfig::default()
        };
        config.ui.fullscreen = false;
        let mut camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://example.invalid/1");
        camera.recording.schedule = Some("0 8 * * 1-5".into());
        config.cameras.push(camera);
        config.save(&path).unwrap();
        let loaded = AppConfig::load_or_default(&path).unwrap();
        assert_eq!(loaded.version, 4);
        assert!(!loaded.ui.fullscreen);
        assert_eq!(
            loaded.cameras[0].recording.mode,
            nw_core::camera::RecordingMode::Continuous
        );
        assert_eq!(
            loaded.cameras[0].recording.schedule.as_deref(),
            Some("0 8 * * 1-5")
        );
        assert!(loaded.cameras[0].recording.weekly_rules.is_empty());
        std::fs::remove_file(path).unwrap();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WallConfig {
    pub rows: u32,
    pub cols: u32,
    pub auto_fit: bool,
    pub page_interval_sec: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,
    pub fullscreen: bool,
    /// In fullscreen with the panels hidden, show the status readings over
    /// the video (half opaque) while the title strip is hidden.
    #[serde(default = "default_true")]
    pub fullscreen_status: bool,
    /// Opacity of that overlay's background, in percent (0–100).
    #[serde(default = "default_status_opacity")]
    pub fullscreen_status_opacity: u8,
    /// Desktop live-preview frame-rate limits.
    #[serde(default)]
    pub preview_fps: PreviewFpsConfig,
}

/// Frames per second the desktop wall paints, by what is on screen. Only the
/// desktop preview: the camera, recording and browser viewing are never
/// limited by these. Missing fields take the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewFpsConfig {
    /// One camera fills the wall (single view).
    pub single: u32,
    /// Each tile of a page showing up to 16 cameras.
    pub wall: u32,
    /// Each tile of a page showing more than 16 cameras.
    pub dense: u32,
    /// Cameras on pages not shown, kept warm for an instant page change.
    pub hidden: u32,
}

/// Upper bounds of [`PreviewFpsConfig`] (the lower bound is 1).
pub const MAX_PREVIEW_FPS: u32 = 60;
pub const MAX_HIDDEN_PREVIEW_FPS: u32 = 30;

impl Default for PreviewFpsConfig {
    fn default() -> Self {
        Self {
            single: 25,
            wall: 15,
            dense: 4,
            hidden: 2,
        }
    }
}

impl PreviewFpsConfig {
    /// `self` with the fields `value` sets, each clamped to its range.
    pub fn updated(mut self, value: &serde_json::Value) -> Self {
        let field = |name: &str, current: u32, top: u32| {
            value[name]
                .as_u64()
                .map_or(current, |v| v.clamp(1, u64::from(top)) as u32)
        };
        self.single = field("single", self.single, MAX_PREVIEW_FPS);
        self.wall = field("wall", self.wall, MAX_PREVIEW_FPS);
        self.dense = field("dense", self.dense, MAX_PREVIEW_FPS);
        self.hidden = field("hidden", self.hidden, MAX_HIDDEN_PREVIEW_FPS);
        self
    }
}

pub const DEFAULT_STATUS_OPACITY: u8 = 50;

fn default_status_opacity() -> u8 {
    DEFAULT_STATUS_OPACITY
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugConfig {
    pub gst_log: String,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            gst_log: "warn".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingGlobalConfig {
    pub storage_path: String,
    pub segment_minutes: u32,
    pub pre_buffer_sec: u32,
    #[serde(default)]
    pub local_all_cameras: bool,
    /// Delete the oldest recordings once the recording drive is fuller than
    /// this percentage of its size. `0` turns it off.
    #[serde(default = "default_disk_threshold_percent")]
    pub disk_threshold_percent: u8,
    /// Each recording location's identity (expanded path → the id in its
    /// `.neurowatch-storage` marker, `crate::storage_guard`): recordings go
    /// there only while that marker is present, so an unmounted drive or
    /// share never fills the empty mount point on the local disk.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub storage_ids: std::collections::BTreeMap<String, String>,
    /// Move older recordings to a second location (`crate::recording::archive`).
    #[serde(default, skip_serializing_if = "ArchiveConfig::is_default")]
    pub archive: ArchiveConfig,
}

/// The archive tier: finished recordings older than `after_days` move from
/// the recording location to `path` (typically a NAS), where playback and
/// export still find them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub path: String,
    /// Recordings older than this many days are moved.
    #[serde(default = "default_archive_after_days")]
    pub after_days: u32,
    /// Most the archive keeps, in GB; the oldest archived recordings (those
    /// without events first) are deleted beyond it. `None`: no limit.
    #[serde(default)]
    pub quota_gb: Option<u32>,
    /// Local hours ("HH:MM") archiving may run in; `None`: any time. An end
    /// before the start runs overnight.
    #[serde(default)]
    pub hours: Option<ArchiveHours>,
    /// Copy speed limit in megabits per second; `None`: no limit.
    #[serde(default)]
    pub max_mbps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveHours {
    pub start: String,
    pub end: String,
}

pub const DEFAULT_ARCHIVE_AFTER_DAYS: u32 = 7;
pub const MAX_ARCHIVE_AFTER_DAYS: u32 = 3650;

fn default_archive_after_days() -> u32 {
    DEFAULT_ARCHIVE_AFTER_DAYS
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: String::new(),
            after_days: DEFAULT_ARCHIVE_AFTER_DAYS,
            quota_gb: None,
            hours: None,
            max_mbps: None,
        }
    }
}

impl ArchiveConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Minutes after midnight of an "HH:MM" time.
    pub fn minute_of(time: &str) -> Option<u32> {
        let (hours, minutes) = time.split_once(':')?;
        let (hours, minutes) = (hours.parse::<u32>().ok()?, minutes.parse::<u32>().ok()?);
        (hours < 24 && minutes < 60 && time.len() == 5).then_some(hours * 60 + minutes)
    }

    /// `current` with the fields of `changes` applied (a `config.set`
    /// `recording.archive` object), or why they cannot be. Nothing changes on
    /// error.
    pub fn changed(current: &Self, changes: &serde_json::Value) -> anyhow::Result<Self> {
        let mut next = current.clone();
        if let Some(value) = changes.get("enabled") {
            next.enabled = value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("archive 'enabled' is true or false"))?;
        }
        if let Some(value) = changes.get("path") {
            next.path = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("archive 'path' is a folder"))?
                .trim()
                .to_owned();
        }
        if let Some(value) = changes.get("after_days") {
            next.after_days = value
                .as_u64()
                .filter(|days| (1..=u64::from(MAX_ARCHIVE_AFTER_DAYS)).contains(days))
                .ok_or_else(|| {
                    anyhow::anyhow!("recordings move to the archive after 1 to 3650 days")
                })? as u32;
        }
        if let Some(value) = changes.get("quota_gb") {
            next.quota_gb = match value {
                serde_json::Value::Null => None,
                value => Some(
                    value
                        .as_u64()
                        .filter(|gb| (1..=1_000_000).contains(gb))
                        .ok_or_else(|| {
                            anyhow::anyhow!("archive size limit is 1 to 1000000 GB, or none")
                        })? as u32,
                ),
            };
        }
        if let Some(value) = changes.get("max_mbps") {
            next.max_mbps = match value {
                serde_json::Value::Null => None,
                value => Some(
                    value
                        .as_u64()
                        .filter(|mbps| (1..=100_000).contains(mbps))
                        .ok_or_else(|| {
                            anyhow::anyhow!("archive speed limit is 1 to 100000 Mbit/s, or none")
                        })? as u32,
                ),
            };
        }
        if let Some(value) = changes.get("hours") {
            next.hours = match value {
                serde_json::Value::Null => None,
                value => {
                    let time = |field: &str| {
                        value[field]
                            .as_str()
                            .filter(|time| Self::minute_of(time).is_some())
                            .map(str::to_owned)
                            .ok_or_else(|| anyhow::anyhow!("archive hours are \"HH:MM\" times"))
                    };
                    Some(ArchiveHours {
                        start: time("start")?,
                        end: time("end")?,
                    })
                }
            };
        }
        if next.enabled && next.path.is_empty() {
            anyhow::bail!("choose a folder for the archive");
        }
        Ok(next)
    }

    /// Whether archiving may run at `minute` (local minutes after midnight).
    pub fn allows(&self, minute: u32) -> bool {
        let Some(hours) = &self.hours else {
            return true;
        };
        let (Some(start), Some(end)) = (Self::minute_of(&hours.start), Self::minute_of(&hours.end))
        else {
            return true;
        };
        match start.cmp(&end) {
            std::cmp::Ordering::Less => (start..end).contains(&minute),
            std::cmp::Ordering::Greater => minute >= start || minute < end,
            std::cmp::Ordering::Equal => true,
        }
    }
}

/// Recording drives are kept at most this full unless configured otherwise.
pub const DEFAULT_DISK_THRESHOLD_PERCENT: u8 = 90;
/// Highest disk threshold; `0` turns the threshold off.
pub const MAX_DISK_THRESHOLD_PERCENT: u8 = 99;

fn default_disk_threshold_percent() -> u8 {
    DEFAULT_DISK_THRESHOLD_PERCENT
}

/// `~/Videos/neurowatch`. Unit tests record (and mark recording locations)
/// under a temporary folder instead, never in the developer's home.
fn default_storage_root() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join("nw-test-recordings");
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join("Videos")
        .join("neurowatch")
}

impl Default for RecordingGlobalConfig {
    fn default() -> Self {
        let path = default_storage_root().to_string_lossy().into_owned();
        Self {
            storage_path: path,
            segment_minutes: 1,
            pre_buffer_sec: 10,
            local_all_cameras: false,
            disk_threshold_percent: DEFAULT_DISK_THRESHOLD_PERCENT,
            storage_ids: std::collections::BTreeMap::new(),
            archive: ArchiveConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionGlobalConfig {
    pub default_sensitivity: u8,
    pub default_cooldown_sec: f32,
    pub default_min_duration_sec: f32,
}

impl Default for MotionGlobalConfig {
    fn default() -> Self {
        Self {
            default_sensitivity: 5,
            default_cooldown_sec: 10.0,
            default_min_duration_sec: 0.5,
        }
    }
}

/// Stored events (`crate::events`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsConfig {
    /// Finished events are deleted after this many days; `0` keeps them.
    #[serde(default = "default_event_retention_days")]
    pub retention_days: u32,
    /// TCP port cameras send their events to (push); `0`: never listen, ask
    /// the cameras for their events instead (pull).
    #[serde(default = "default_push_port")]
    pub push_port: u16,
}

pub const DEFAULT_PUSH_PORT: u16 = 7681;
/// Longest event history that can be kept (ten years); `0` keeps it all.
pub const MAX_EVENT_RETENTION_DAYS: u32 = 3650;

impl EventsConfig {
    /// `current` with the fields of `changes` applied (a `config.set`
    /// `events` object), or why they cannot be. Nothing changes on error.
    pub fn changed(current: &Self, changes: &serde_json::Value) -> anyhow::Result<Self> {
        let mut next = current.clone();
        if let Some(value) = changes.get("retention_days") {
            next.retention_days = value
                .as_u64()
                .filter(|days| *days <= u64::from(MAX_EVENT_RETENTION_DAYS))
                .ok_or_else(|| {
                    anyhow::anyhow!("event history must be kept 0 (forever) to 3650 days")
                })? as u32;
        }
        if let Some(value) = changes.get("push_port") {
            next.push_port = value
                .as_u64()
                .filter(|port| *port == 0 || (1024..=65535).contains(port))
                .ok_or_else(|| {
                    anyhow::anyhow!("camera event port must be 0 (off) or 1024 to 65535")
                })? as u16;
        }
        Ok(next)
    }
}

fn is_default_accounts(accounts: &crate::accounts::AccountsConfig) -> bool {
    !accounts.enabled && accounts.users.is_empty()
}

fn is_default_alerts(alerts: &crate::alerts::AlertsConfig) -> bool {
    alerts.channels.is_empty() && alerts.rules.is_empty()
}

fn default_push_port() -> u16 {
    DEFAULT_PUSH_PORT
}

pub const DEFAULT_EVENT_RETENTION_DAYS: u32 = 30;

fn default_event_retention_days() -> u32 {
    DEFAULT_EVENT_RETENTION_DAYS
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_EVENT_RETENTION_DAYS,
            push_port: DEFAULT_PUSH_PORT,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorsConfig {
    pub detached: Vec<DetachedMonitor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetachedMonitor {
    pub camera_id: String,
    pub monitor_index: u32,
}

/// Most extra windows and tiles a saved desktop layout may hold.
pub const MAX_DESKTOP_WINDOWS: usize = 16;
pub const MAX_WINDOW_TILES: usize = 1024;

/// The desktop UI's session: extra windows and what they show. Only the
/// desktop reads it; the main wall's layout lives in each camera's
/// `wall_position`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DesktopConfig {
    #[serde(default)]
    pub windows: Vec<DesktopWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesktopWindow {
    /// The number in its title ("Window 2").
    pub number: u32,
    pub rows: u32,
    pub cols: u32,
    /// Its cameras and where each sits in this window (page, cell, size).
    pub tiles: Vec<WindowTile>,
    /// Page shown (0-based).
    #[serde(default)]
    pub page: u32,
    #[serde(default = "default_true")]
    pub cycling: bool,
    #[serde(default)]
    pub geometry: Option<WindowGeometry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowTile {
    pub camera_id: String,
    pub page: u32,
    pub row: u32,
    pub col: u32,
    #[serde(default = "default_span")]
    pub row_span: u32,
    #[serde(default = "default_span")]
    pub col_span: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowGeometry {
    /// The screen's name (e.g. "DP-1"); empty when unknown.
    #[serde(default)]
    pub screen: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// "normal", "maximized" or "fullscreen".
    #[serde(default = "default_window_state")]
    pub state: String,
}

fn default_true() -> bool {
    true
}

fn default_span() -> u32 {
    1
}

fn default_window_state() -> String {
    "normal".into()
}

impl DesktopConfig {
    /// Parse a `desktop` value from `config.set`, refusing what no desktop
    /// would send: too many windows or tiles, empty grids, unknown states.
    pub fn from_value(value: &serde_json::Value) -> anyhow::Result<Self> {
        let desktop: Self =
            serde_json::from_value(value.clone()).context("invalid desktop layout")?;
        anyhow::ensure!(
            desktop.windows.len() <= MAX_DESKTOP_WINDOWS,
            "at most {MAX_DESKTOP_WINDOWS} windows can be saved"
        );
        for window in &desktop.windows {
            anyhow::ensure!(
                (1..=16).contains(&window.rows) && (1..=16).contains(&window.cols),
                "a window grid must be 1 to 16 rows and columns"
            );
            anyhow::ensure!(
                window.tiles.len() <= MAX_WINDOW_TILES,
                "a window can hold at most {MAX_WINDOW_TILES} tiles"
            );
            if let Some(geometry) = &window.geometry {
                anyhow::ensure!(
                    matches!(
                        geometry.state.as_str(),
                        "normal" | "maximized" | "fullscreen"
                    ),
                    "unknown window state"
                );
                anyhow::ensure!(geometry.screen.len() <= 128, "screen name too long");
            }
            for tile in &window.tiles {
                anyhow::ensure!(
                    !tile.camera_id.is_empty() && tile.camera_id.len() <= 128,
                    "invalid camera id in a saved window"
                );
            }
        }
        Ok(desktop)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 4,
            runtime_mode: RuntimeMode::LocalStandalone,
            site_name: "My Site".into(),
            wall: WallConfig {
                rows: 3,
                cols: 3,
                auto_fit: true,
                page_interval_sec: 0,
            },
            cameras: Vec::new(),
            cameras_over_limit: Vec::new(),
            ui: UiConfig {
                theme: "dark".into(),
                fullscreen: true,
                fullscreen_status: true,
                fullscreen_status_opacity: DEFAULT_STATUS_OPACITY,
                preview_fps: PreviewFpsConfig::default(),
            },
            debug: DebugConfig::default(),
            recording: RecordingGlobalConfig::default(),
            motion: MotionGlobalConfig::default(),
            events: EventsConfig::default(),
            alerts: crate::alerts::AlertsConfig::default(),
            accounts: crate::accounts::AccountsConfig::default(),
            monitors: MonitorsConfig::default(),
            desktop: DesktopConfig::default(),
        }
    }
}

impl AppConfig {
    /// Load from disk, or return default config if the file does not exist yet.
    pub fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::info!(path = %path.display(), "Config not found — using defaults");
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read config: {}", path.display()))?;
        let mut config: Self = serde_json::from_str(&text)
            .with_context(|| format!("Invalid JSON in config: {}", path.display()))?;
        // Fullscreen was previously persisted but ignored by the UI. Since it
        // could not represent a real user choice, enable the new working
        // fullscreen startup once during the v3 migration.
        let old_version = config.version;
        if old_version < 3 {
            config.ui.fullscreen = true;
        }
        for camera in config.cameras.iter().chain(&config.cameras_over_limit) {
            crate::recording::policy::validate_settings(&camera.recording).map_err(|message| {
                anyhow::anyhow!(
                    "Invalid recording policy for camera {}: {message}",
                    camera.id
                )
            })?;
            if old_version < 4
                && camera
                    .recording
                    .schedule
                    .as_deref()
                    .is_some_and(|schedule| !schedule.is_empty() && schedule != "always")
            {
                tracing::warn!(camera_id=%camera.id,"Legacy cron schedule was never enforced; preserved for reference, use weekly_rules");
            }
        }
        let held_before = config.cameras_over_limit.len();
        crate::edition::apply(&mut config);
        if config.cameras_over_limit.len() > held_before {
            tracing::warn!(
                running = config.cameras.len(),
                held = config.cameras_over_limit.len(),
                limit = ?crate::edition::CAMERA_LIMIT,
                downloads = crate::edition::DOWNLOADS_URL,
                "Camera limit of this free version reached; the other cameras are kept but not run"
            );
        }
        if old_version < 4 {
            config.version = 4;
            if let Err(e) = config.save(path) {
                tracing::warn!(error = %e, "Failed to write migrated v4 config");
            } else {
                tracing::info!("Config migrated to v4");
            }
        }
        Ok(config)
    }

    /// Atomic write: write to .tmp then rename (crash-safe).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create config directory: {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("Failed to serialize config")?;
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        std::fs::write(&tmp, &text)
            .with_context(|| format!("Cannot write temp config: {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!(
                "Cannot rename config: {} → {}",
                tmp.display(),
                path.display()
            )
        })?;
        tracing::debug!(path = %path.display(), "Config saved");
        Ok(())
    }
}
