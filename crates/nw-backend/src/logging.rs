//! The backend's logs.
//!
//! * **Service log**: standard output, which the service manager writes to
//!   `<state>/logs/service.log` (or the systemd journal). `RUST_LOG`
//!   replaces its filter ([`default_log_filter`]); `NW_LOG_JSON` makes it
//!   JSON.
//! * **Debug log**, while extended debug logging is on (`debug.extended`,
//!   Settings → Debug): `<state>/logs/backend-debug.log` ([`debug_log_path`]),
//!   at most [`DEBUG_LOG_FILES`] files of [`DEBUG_LOG_MAX_BYTES`] each. It
//!   holds the backend's own debug lines, with thread names and ids;
//!   GStreamer's messages down to `debug.gst_log` ([`gst_threshold`]; they
//!   reach the service log too, warnings and errors only); and the reports of
//!   [`crate::debug_monitor`] (resources, stalls). [`apply`] switches it on
//!   and off at once, without a restart.
//!
//! A `GST_DEBUG` set in the environment leaves GStreamer's logging to it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{fmt, reload, EnvFilter, Layer as _, Registry};

use crate::config::DebugConfig;

/// Largest debug log file before it is rotated.
pub const DEBUG_LOG_MAX_BYTES: u64 = 25 * 1024 * 1024;
/// Debug log files kept: the current one and its rotated predecessors.
pub const DEBUG_LOG_FILES: usize = 4;

static EXTENDED: AtomicBool = AtomicBool::new(false);
static CONTROL: OnceLock<Control> = OnceLock::new();

struct Control {
    debug_filter: reload::Handle<EnvFilter, Registry>,
    debug_file: Arc<Mutex<RotatingFile>>,
    /// The settings last applied, so a repeat does nothing.
    applied: Mutex<Option<DebugConfig>>,
    /// GStreamer's log reaches ours (once GStreamer is initialized, unless
    /// `GST_DEBUG` is set).
    gst_bridge: OnceLock<bool>,
}

/// Extended debug logging is on.
pub fn extended() -> bool {
    EXTENDED.load(Ordering::Relaxed)
}

/// Set up the service log and the (still closed) debug log, then apply
/// `debug`. Call once, first thing; GStreamer's part follows in [`apply`]
/// once GStreamer is initialized.
pub fn init(debug: &DebugConfig) {
    let service_filter = std::env::var("RUST_LOG")
        .ok()
        .map(|value| normalize_env_filter(&value))
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new(default_log_filter()));
    let service = if std::env::var("NW_LOG_JSON").is_ok() {
        fmt::layer().json().with_filter(service_filter).boxed()
    } else {
        fmt::layer().pretty().with_filter(service_filter).boxed()
    };
    let debug_file = Arc::new(Mutex::new(RotatingFile::new(
        debug_log_path(),
        DEBUG_LOG_MAX_BYTES,
        DEBUG_LOG_FILES,
    )));
    let (debug_filter, handle) = reload::Layer::new(EnvFilter::new("off"));
    let debug_layer = fmt::layer()
        .with_ansi(false)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_writer(DebugWriter(Arc::clone(&debug_file)))
        .with_filter(debug_filter)
        .boxed();
    if tracing_subscriber::registry()
        .with(vec![service, debug_layer])
        .try_init()
        .is_err()
    {
        return; // already set up (tests)
    }
    let _ = CONTROL.set(Control {
        debug_filter: handle,
        debug_file,
        applied: Mutex::new(None),
        gst_bridge: OnceLock::new(),
    });
    apply(debug);
}

/// Switch extended debug logging to `debug` (Settings → Debug, `config.set`).
pub fn apply(settings: &DebugConfig) {
    let Some(control) = CONTROL.get() else {
        return;
    };
    let gst_ready = gstreamer::INITIALIZED.load(Ordering::SeqCst);
    {
        let Ok(mut applied) = control.applied.lock() else {
            return;
        };
        // GStreamer's part waits for GStreamer: apply again once it is ready.
        let unchanged = applied.as_ref() == Some(settings)
            && (!settings.extended || control.gst_bridge.get().is_some() || !gst_ready);
        if unchanged {
            return;
        }
        *applied = Some(settings.clone());
    }
    let was = EXTENDED.load(Ordering::SeqCst);
    if settings.extended {
        if let Ok(mut file) = control.debug_file.lock() {
            if let Err(error) = file.open() {
                tracing::warn!(%error, path = %file.path.display(), "Could not open the debug log");
            }
        }
        let filter = EnvFilter::try_new(extended_filter(&settings.gst_log))
            .unwrap_or_else(|_| EnvFilter::new(extended_filter("warn")));
        let _ = control.debug_filter.reload(filter);
        EXTENDED.store(true, Ordering::SeqCst);
        if gst_ready {
            configure_gstreamer(control, Some(gst_threshold(&settings.gst_log)));
        }
        if !was {
            tracing::info!(
                path = %debug_log_path().display(),
                gst_log = %settings.gst_log,
                version = env!("CARGO_PKG_VERSION"),
                pid = std::process::id(),
                os = std::env::consts::OS,
                arch = std::env::consts::ARCH,
                supervised = crate::supervise::supervised(),
                "Extended debug logging is on"
            );
        }
    } else {
        if was {
            tracing::info!("Extended debug logging is off");
        }
        EXTENDED.store(false, Ordering::SeqCst);
        if gst_ready {
            configure_gstreamer(control, None);
        }
        let _ = control.debug_filter.reload(EnvFilter::new("off"));
        if let Ok(mut file) = control.debug_file.lock() {
            file.close();
        }
    }
}

/// The service log's filter when `RUST_LOG` is unset: this crate's own
/// target at info (it logs as `nw_backend::…`), `neurowatch` kept for
/// compatibility, the cloud's WebSocket control plane (`cloud_ws`), and
/// GStreamer's warnings while extended debug logging forwards them.
pub fn default_log_filter() -> String {
    "nw_backend=info,neurowatch=info,cloud_ws=info,gstreamer=warn".into()
}

/// The debug log's filter: other libraries' warnings, everything of ours,
/// GStreamer's messages down to `gst_level`.
pub fn extended_filter(gst_level: &str) -> String {
    format!(
        "warn,nw_backend=debug,neurowatch=debug,cloud_ws=debug,livekit=info,gstreamer={}",
        normalize_tracing_level(gst_level)
    )
}

/// `<state>/logs/backend-debug.log`, next to the service log: the state
/// directory is `$XDG_STATE_HOME/neurowatch` (`~/.local/state/neurowatch`)
/// on Linux, `%LOCALAPPDATA%\Neurowatch` on Windows, like the service
/// manager's (`ui/neurowatch/service/paths.py`).
pub fn debug_log_path() -> PathBuf {
    state_dir().join("logs").join("backend-debug.log")
}

fn state_dir() -> PathBuf {
    let non_empty = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        non_empty("LOCALAPPDATA")
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(std::env::temp_dir)
            .join("Neurowatch")
    } else {
        non_empty("XDG_STATE_HOME")
            .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")))
            .unwrap_or_else(std::env::temp_dir)
            .join("neurowatch")
    }
}

/// GStreamer's threshold for a `debug.gst_log` level.
pub fn gst_threshold(level: &str) -> gstreamer::DebugLevel {
    use gstreamer::DebugLevel;
    match normalize_tracing_level(level) {
        "error" => DebugLevel::Error,
        "info" => DebugLevel::Info,
        "debug" => DebugLevel::Debug,
        "trace" => DebugLevel::Log,
        _ => DebugLevel::Warning,
    }
}

/// Forward GStreamer's log to ours at `threshold`, or stop (`None`).
fn configure_gstreamer(control: &Control, threshold: Option<gstreamer::DebugLevel>) {
    let bridged = *control.gst_bridge.get_or_init(|| {
        if std::env::var_os("GST_DEBUG").is_some() {
            tracing::info!("GST_DEBUG is set: GStreamer's log stays as it asks");
            return false;
        }
        // Its default handler prints to standard error; ours goes through
        // the log's filters instead.
        gstreamer::log::remove_default_log_function();
        gstreamer::log::add_log_function(forward_gstreamer);
        true
    });
    if !bridged {
        return;
    }
    match threshold {
        Some(level) => {
            gstreamer::log::set_default_threshold(level);
            gstreamer::log::set_active(true);
        }
        None => {
            gstreamer::log::set_default_threshold(gstreamer::DebugLevel::None);
            gstreamer::log::set_active(false);
        }
    }
}

/// One GStreamer message, as a `gstreamer` log line. Runs on GStreamer's
/// threads, streaming ones included: it only formats and writes.
fn forward_gstreamer(
    category: gstreamer::DebugCategory,
    level: gstreamer::DebugLevel,
    file: &gstreamer::glib::GStr,
    function: &gstreamer::glib::GStr,
    line: u32,
    object: Option<&gstreamer::LoggedObject>,
    message: &gstreamer::DebugMessage,
) {
    use gstreamer::DebugLevel;
    if !extended() {
        return;
    }
    let Some(text) = message.get() else {
        return;
    };
    let category = category.name();
    let object = object.map(|object| object.to_string()).unwrap_or_default();
    let at = format_args!("{file}:{line} {function}");
    match level {
        DebugLevel::Error => {
            tracing::error!(target: "gstreamer", category, object = %object, at = %at, "{text}")
        }
        DebugLevel::Warning | DebugLevel::Fixme => {
            tracing::warn!(target: "gstreamer", category, object = %object, at = %at, "{text}")
        }
        DebugLevel::Info => {
            tracing::info!(target: "gstreamer", category, object = %object, at = %at, "{text}")
        }
        DebugLevel::Debug => {
            tracing::debug!(target: "gstreamer", category, object = %object, at = %at, "{text}")
        }
        _ => tracing::trace!(target: "gstreamer", category, object = %object, at = %at, "{text}"),
    }
}

fn normalize_tracing_level(level: &str) -> &'static str {
    match level.trim().to_ascii_lowercase().as_str() {
        "warning" | "warn" => "warn",
        "err" | "error" => "error",
        "off" => "off",
        "info" => "info",
        "debug" => "debug",
        "trace" | "log" => "trace",
        "0" => "0",
        "1" => "1",
        "2" => "2",
        "3" => "3",
        "4" => "4",
        "5" => "5",
        _ => "warn",
    }
}

fn normalize_env_filter(filter: &str) -> String {
    filter
        .split(',')
        .map(|directive| {
            let Some((target, level)) = directive.rsplit_once('=') else {
                return directive.to_owned();
            };
            format!("{target}={}", normalize_tracing_level(level))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A log file that rotates by size: `name`, `name.1` (newest) … up to
/// `files` files in all. Closed, it drops what it is given.
struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    files: usize,
    file: Option<File>,
    written: u64,
}

impl RotatingFile {
    fn new(path: PathBuf, max_bytes: u64, files: usize) -> Self {
        Self {
            path,
            max_bytes,
            files: files.max(1),
            file: None,
            written: 0,
        }
    }

    fn open(&mut self) -> io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(file);
        Ok(())
    }

    fn close(&mut self) {
        self.file = None;
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(buf)?;
            self.written += buf.len() as u64;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        if self.files > 1 {
            for index in (1..self.files).rev() {
                let from = numbered(&self.path, index - 1);
                if from.exists() {
                    let to = numbered(&self.path, index);
                    // Windows cannot rename onto an existing file.
                    let _ = std::fs::remove_file(&to);
                    std::fs::rename(&from, &to)?;
                }
            }
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
        self.written = 0;
        self.open()
    }
}

/// `path` itself for 0, else `path.<n>`.
fn numbered(path: &Path, n: usize) -> PathBuf {
    if n == 0 {
        return path.to_owned();
    }
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

#[derive(Clone)]
struct DebugWriter(Arc<Mutex<RotatingFile>>);

impl Write for DebugWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut file) = self.0.lock() {
            // A log that cannot be written must not fail what is logged.
            let _ = file.write(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for DebugWriter {
    type Writer = DebugWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "neurowatch-logging-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn default_log_filter_enables_this_crates_own_log_target() {
        // Events are emitted under the crate target `nw_backend::…`; a filter
        // that only names `neurowatch` silently drops every backend log line.
        let filter = default_log_filter();
        assert!(
            filter.split(',').any(|d| d == "nw_backend=info"),
            "{filter}"
        );
        assert!(EnvFilter::try_new(&filter).is_ok());
    }

    #[test]
    fn the_extended_filter_logs_ours_at_debug_and_gstreamer_at_its_level() {
        let filter = extended_filter("warning");
        assert!(filter.contains("nw_backend=debug") && filter.contains("gstreamer=warn"));
        assert!(extended_filter("debug").contains("gstreamer=debug"));
        assert!(extended_filter("nonsense").contains("gstreamer=warn"));
        assert!(EnvFilter::try_new(extended_filter("info")).is_ok());
    }

    #[test]
    fn gstreamer_levels_follow_the_setting() {
        use gstreamer::DebugLevel;
        assert_eq!(gst_threshold("warning"), DebugLevel::Warning);
        assert_eq!(gst_threshold("error"), DebugLevel::Error);
        assert_eq!(gst_threshold("debug"), DebugLevel::Debug);
        assert_eq!(gst_threshold("?"), DebugLevel::Warning);
    }

    #[test]
    fn rust_log_levels_are_normalized() {
        assert_eq!(
            normalize_env_filter("nw_backend=warning,x=err"),
            "nw_backend=warn,x=error"
        );
    }

    #[test]
    fn the_debug_log_rotates_by_size_and_keeps_a_few_files() {
        let dir = temp_dir("rotate");
        let path = dir.join("backend-debug.log");
        let mut log = RotatingFile::new(path.clone(), 10, 3);
        log.write(b"dropped while closed\n").expect("write");
        assert!(!path.exists(), "closed: nothing written");

        log.open().expect("open");
        for line in ["aaaaaaaa\n", "bbbbbbbb\n", "cccccccc\n", "dddddddd\n"] {
            log.write(line.as_bytes()).expect("write");
        }
        let read = |p: &Path| std::fs::read_to_string(p).expect("read");
        assert_eq!(read(&path), "dddddddd\n");
        assert_eq!(read(&numbered(&path, 1)), "cccccccc\n");
        assert_eq!(read(&numbered(&path, 2)), "bbbbbbbb\n");
        assert!(!numbered(&path, 3).exists(), "at most three files");

        // Reopened, it continues the current file and its size.
        log.close();
        log.open().expect("reopen");
        log.write(b"e\n").expect("write");
        assert_eq!(read(&path), "e\n", "full: rotated first");
        assert_eq!(read(&numbered(&path, 1)), "dddddddd\n");
        assert_eq!(read(&numbered(&path, 2)), "cccccccc\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_debug_log_lives_with_the_service_log() {
        let path = debug_log_path();
        assert!(path.ends_with(Path::new("logs").join("backend-debug.log")));
        let state = path.parent().and_then(Path::parent).expect("state dir");
        let name = state.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name.eq_ignore_ascii_case("neurowatch"),
            "{}",
            path.display()
        );
    }
}
