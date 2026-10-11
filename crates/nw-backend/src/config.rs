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
    pub ui: UiConfig,
    #[serde(default)]
    pub debug: DebugConfig,
    #[serde(default)]
    pub recording: RecordingGlobalConfig,
    #[serde(default)]
    pub motion: MotionGlobalConfig,
    #[serde(default)]
    pub monitors: MonitorsConfig,
    /// The desktop UI's extra windows, reopened when the app starts again.
    #[serde(default)]
    pub desktop: DesktopConfig,
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
}

/// Recording drives are kept at most this full unless configured otherwise.
pub const DEFAULT_DISK_THRESHOLD_PERCENT: u8 = 90;
/// Highest disk threshold; `0` turns the threshold off.
pub const MAX_DISK_THRESHOLD_PERCENT: u8 = 99;

fn default_disk_threshold_percent() -> u8 {
    DEFAULT_DISK_THRESHOLD_PERCENT
}

impl Default for RecordingGlobalConfig {
    fn default() -> Self {
        let path = dirs::home_dir()
            .unwrap_or_default()
            .join("Videos")
            .join("neurowatch")
            .to_string_lossy()
            .into_owned();
        Self {
            storage_path: path,
            segment_minutes: 1,
            pre_buffer_sec: 10,
            local_all_cameras: false,
            disk_threshold_percent: DEFAULT_DISK_THRESHOLD_PERCENT,
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
            version: 3,
            runtime_mode: RuntimeMode::LocalStandalone,
            site_name: "My Site".into(),
            wall: WallConfig {
                rows: 3,
                cols: 3,
                auto_fit: true,
                page_interval_sec: 0,
            },
            cameras: Vec::new(),
            ui: UiConfig {
                theme: "dark".into(),
                fullscreen: true,
                fullscreen_status: true,
                fullscreen_status_opacity: DEFAULT_STATUS_OPACITY,
            },
            debug: DebugConfig::default(),
            recording: RecordingGlobalConfig::default(),
            motion: MotionGlobalConfig::default(),
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
        if config.version < 3 {
            config.version = 3;
            config.ui.fullscreen = true;
            if let Err(e) = config.save(path) {
                tracing::warn!(error = %e, "Failed to write migrated v3 config");
            } else {
                tracing::info!("Config migrated to v3");
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
