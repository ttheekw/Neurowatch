//! Clip export: a camera's recordings over a time range, saved as one MP4.
//!
//! The engine is independent of who asks, so every client shares it:
//!
//! - the desktop UI, over IPC (`export.start` / `export.status` /
//!   `export.cancel` / `export.list`, [`crate::ipc::export_handler`]);
//! - the web portal later, through the same [`ExportService`] (one per
//!   backend, in [`crate::app_state::AppState::exports`]). The cloud media
//!   contract already names the action (`MediaAction::Export`); a portal
//!   request leaves `output` unset, so the clip lands in
//!   [`default_export_dir`] and never at a path the portal chose.
//!
//! Pieces:
//!
//! - [`ExportRequest`] + [`ExportService::start`]: checks the range and the
//!   destination (and its free space), then runs the job on its own thread.
//! - [`ExportService::status`] / [`ExportService::list`]: progress and
//!   outcome, for polling; [`ExportService::cancel`] stops a job.
//! - [`remux`]: the worker. Video is copied, never re-encoded (see there for
//!   what that means for where a clip starts).
//!
//! A job writes `<name>.part` next to its destination and renames it once the
//! MP4 is complete, so a failed or cancelled export never leaves a broken
//! file under the chosen name.

pub mod remux;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::playback::session::PlaybackSegment;
use remux::RemuxSummary;

/// Longest range one export covers.
pub const MAX_RANGE_MS: i64 = 24 * 60 * 60 * 1000;
/// Exports running at once (each reads and writes a whole clip).
const MAX_RUNNING: usize = 2;
/// Finished jobs whose status is kept for polling.
const KEPT_FINISHED: usize = 32;
/// Free space kept beyond the estimate.
const SPACE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
/// Progress is stored in parts per this.
const PROGRESS_SCALE: f64 = 10_000.0;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The request itself is wrong (range, destination, space).
    #[error("{0}")]
    Invalid(String),
    #[error("No recordings of this camera in the chosen time range")]
    NoRecordings,
    #[error("The recordings in the chosen time range could not be read")]
    Unreadable,
    #[error("These recordings are {0}; only H.264 and H.265 video can be exported to MP4")]
    Unsupported(String),
    #[error("Export cancelled")]
    Cancelled,
    #[error("Writing the MP4 failed: {0}")]
    Output(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl ExportError {
    /// A problem with the request (as opposed to with the recordings or the
    /// writing): the caller should change what it asked for.
    pub fn is_request_error(&self) -> bool {
        matches!(self, Self::Invalid(_) | Self::NoRecordings)
    }
}

/// What to export.
#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub camera_id: String,
    /// For the default file name.
    pub camera_name: String,
    /// Range, in ms since the epoch.
    pub start_ms: i64,
    pub end_ms: i64,
    /// Where to save the MP4 (absolute; `.mp4` is added when missing).
    /// `None`: a new file in [`default_export_dir`].
    pub output: Option<PathBuf>,
    /// Replace a file already at `output` (the user confirmed it).
    pub overwrite: bool,
}

/// Check an export range.
pub fn validate_range(start_ms: i64, end_ms: i64) -> Result<(), ExportError> {
    if start_ms < 0 || end_ms <= start_ms {
        return Err(ExportError::Invalid(
            "The end of the clip must be later than its start".into(),
        ));
    }
    if end_ms - start_ms > MAX_RANGE_MS {
        return Err(ExportError::Invalid(
            "A clip can be at most 24 hours long".into(),
        ));
    }
    Ok(())
}

/// Where clips go when the request names no file:
/// `<local data dir>/neurowatch/exports`.
pub fn default_export_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("neurowatch")
        .join("exports")
}

/// `<camera>_<YYYY-MM-DD_HH-MM-SS>.mp4`, the start in local time; the camera
/// name reduced to characters every file system takes.
pub fn default_file_name(camera_name: &str, start_ms: i64) -> String {
    let name: String = camera_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim_matches('_');
    let name = if name.is_empty() { "camera" } else { name };
    let at = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(start_ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let at = local_offset(at)
        .map(|offset| at.to_offset(offset))
        .unwrap_or(at);
    format!(
        "{name}_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.mp4",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// The local time zone's offset at `at`. Not `UtcOffset::local_offset_at`
/// on Unix: it refuses to answer in a process with more than one thread.
fn local_offset(at: time::OffsetDateTime) -> Option<time::UtcOffset> {
    #[cfg(unix)]
    {
        let seconds = at.unix_timestamp() as libc::time_t;
        let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: `localtime_r` is the thread-safe variant; it fills `tm`
        // completely when it returns non-null, and only then is it read.
        let tm = unsafe {
            if libc::localtime_r(&seconds, tm.as_mut_ptr()).is_null() {
                return None;
            }
            tm.assume_init()
        };
        time::UtcOffset::from_whole_seconds(i32::try_from(tm.tm_gmtoff).ok()?).ok()
    }
    #[cfg(not(unix))]
    {
        time::UtcOffset::local_offset_at(at).ok()
    }
}

/// The file a request writes, checked: absolute, `.mp4`, in an existing
/// folder, and not an existing file unless `overwrite`.
pub fn resolve_output(request: &ExportRequest) -> Result<PathBuf, ExportError> {
    let Some(asked) = &request.output else {
        let dir = default_export_dir();
        std::fs::create_dir_all(&dir)?;
        return Ok(unused_name(
            &dir,
            &default_file_name(&request.camera_name, request.start_ms),
        ));
    };
    if !asked.is_absolute() {
        return Err(ExportError::Invalid(
            "The export destination must be a full path".into(),
        ));
    }
    let mut path = asked.clone();
    let is_mp4 = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"));
    if !is_mp4 {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".mp4");
        path.set_file_name(name);
    }
    if path.file_name().is_none() {
        return Err(ExportError::Invalid(
            "Choose a file name for the clip".into(),
        ));
    }
    match path.parent() {
        Some(dir) if dir.is_dir() => {}
        _ => {
            return Err(ExportError::Invalid(
                "The folder to export to does not exist".into(),
            ))
        }
    }
    if path.is_dir() {
        return Err(ExportError::Invalid(
            "The export destination is a folder; choose a file name".into(),
        ));
    }
    if path.exists() && !request.overwrite {
        return Err(ExportError::Invalid(format!(
            "{} already exists",
            path.display()
        )));
    }
    Ok(path)
}

/// `name` in `dir`, or `name (2)`, `name (3)`… when taken.
fn unused_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "mp4"));
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}).{ext}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

/// The file written while the export runs.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// About how many bytes the clip takes: each file's share of the range.
fn estimated_bytes(segments: &[PlaybackSegment], start_ms: i64, end_ms: i64) -> u64 {
    segments
        .iter()
        .map(|segment| {
            let size = std::fs::metadata(&segment.path).map_or(0, |m| m.len());
            let span = (segment.end_ts - segment.start_ts).max(1);
            let overlap = (segment.end_ts.min(end_ms) - segment.start_ts.max(start_ms)).max(0);
            (size as f64 * (overlap as f64 / span as f64).min(1.0)) as u64
        })
        .sum()
}

/// Refuse an export that would not fit: the clip, and as much again while
/// the MP4's index is moved to the front (`faststart`, for streaming).
fn check_space(dir: &Path, needed: u64) -> Result<(), ExportError> {
    let Some((_, available)) = crate::storage_drives::space(&dir.to_string_lossy()) else {
        return Ok(());
    };
    let needed = needed.saturating_mul(2).saturating_add(SPACE_MARGIN_BYTES);
    if available < needed {
        return Err(ExportError::Invalid(format!(
            "Not enough free space: the clip needs about {} MB, {} MB are free",
            needed / 1_000_000,
            available / 1_000_000
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum JobState {
    Running,
    Done {
        summary: RemuxSummary,
        size_bytes: u64,
    },
    Failed(String),
    Cancelled,
}

struct Job {
    id: String,
    camera_id: String,
    start_ms: i64,
    end_ms: i64,
    path: PathBuf,
    created_ms: i64,
    cancel: AtomicBool,
    progress: AtomicU32,
    state: Mutex<JobState>,
}

impl Job {
    fn running(&self) -> bool {
        self.state
            .lock()
            .map(|state| matches!(*state, JobState::Running))
            .unwrap_or(false)
    }

    fn finish(&self, outcome: JobState) {
        if let Ok(mut state) = self.state.lock() {
            *state = outcome;
        }
    }

    fn status(&self) -> ExportStatus {
        let state = self
            .state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| JobState::Failed("job state unavailable".into()));
        let mut status = ExportStatus {
            job_id: self.id.clone(),
            camera_id: self.camera_id.clone(),
            state: "running",
            progress: f64::from(self.progress.load(Ordering::Relaxed)) / PROGRESS_SCALE,
            path: self.path.to_string_lossy().into_owned(),
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            created_ms: self.created_ms,
            first_ms: None,
            last_ms: None,
            duration_ms: None,
            size_bytes: None,
            frames: None,
            audio: None,
            audio_copied: None,
            gaps: None,
            skipped_files: None,
            truncated: None,
            error: None,
        };
        match state {
            JobState::Running => {}
            JobState::Done {
                summary,
                size_bytes,
            } => {
                status.state = "done";
                status.progress = 1.0;
                status.first_ms = Some(summary.first_ms);
                status.last_ms = Some(summary.last_ms);
                status.duration_ms = Some(summary.duration_ms);
                status.size_bytes = Some(size_bytes);
                status.frames = Some(summary.frames);
                status.audio = Some(summary.audio);
                status.audio_copied = Some(summary.sound_copied);
                status.gaps = Some(summary.gaps);
                status.skipped_files = Some(summary.skipped_files);
                status.truncated = Some(summary.truncated);
            }
            JobState::Failed(error) => {
                status.state = "failed";
                status.error = Some(error);
            }
            JobState::Cancelled => status.state = "cancelled",
        }
        status
    }
}

/// A job's state as clients see it (`export.status`).
#[derive(Debug, Clone, Serialize)]
pub struct ExportStatus {
    pub job_id: String,
    pub camera_id: String,
    /// `running`, `done`, `failed` or `cancelled`.
    pub state: &'static str,
    /// 0.0 – 1.0.
    pub progress: f64,
    pub path: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub created_ms: i64,
    /// Done: recording time of the first picture (the keyframe at or before
    /// `start_ms`) and of the last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<i64>,
    /// Done: the MP4's length (recording gaps left out).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    /// Done: the sound was copied as recorded (AAC, Opus) rather than
    /// decoded and encoded to AAC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_copied: Option<bool>,
    /// Done: gaps in the recording left out of the clip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gaps: Option<u32>,
    /// Done: recording files that could not be read and were skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_files: Option<u32>,
    /// Done: the clip ends early, where the recording changed format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Default)]
struct Jobs {
    by_id: HashMap<String, Arc<Job>>,
    /// Oldest first.
    order: VecDeque<String>,
}

/// Runs export jobs and keeps their status. One per backend.
#[derive(Default)]
pub struct ExportService {
    jobs: Mutex<Jobs>,
}

impl ExportService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start exporting `segments` (the camera's recordings overlapping the
    /// range, oldest first) as `request` asks. Returns the new job's status.
    pub fn start(
        &self,
        request: &ExportRequest,
        segments: Vec<PlaybackSegment>,
    ) -> Result<ExportStatus, ExportError> {
        validate_range(request.start_ms, request.end_ms)?;
        if segments.is_empty() {
            return Err(ExportError::NoRecordings);
        }
        let path = resolve_output(request)?;
        if let Some(dir) = path.parent() {
            check_space(
                dir,
                estimated_bytes(&segments, request.start_ms, request.end_ms),
            )?;
        }

        let job = {
            let mut jobs = self
                .jobs
                .lock()
                .map_err(|_| ExportError::Output("export jobs unavailable".into()))?;
            let running: Vec<&Arc<Job>> = jobs.by_id.values().filter(|j| j.running()).collect();
            if running.iter().any(|j| j.path == path) {
                return Err(ExportError::Invalid(format!(
                    "{} is already being exported",
                    path.display()
                )));
            }
            if running.len() >= MAX_RUNNING {
                return Err(ExportError::Invalid(
                    "Other exports are still running; try again when they finish".into(),
                ));
            }
            let job = Arc::new(Job {
                id: uuid::Uuid::new_v4().to_string(),
                camera_id: request.camera_id.clone(),
                start_ms: request.start_ms,
                end_ms: request.end_ms,
                path,
                created_ms: now_ms(),
                cancel: AtomicBool::new(false),
                progress: AtomicU32::new(0),
                state: Mutex::new(JobState::Running),
            });
            jobs.by_id.insert(job.id.clone(), Arc::clone(&job));
            jobs.order.push_back(job.id.clone());
            forget_old(&mut jobs);
            job
        };

        let running = Arc::clone(&job);
        std::thread::Builder::new()
            .name("nw-export".into())
            .spawn(move || run(&running, &segments))
            .map_err(|error| {
                job.finish(JobState::Failed(error.to_string()));
                ExportError::Io(error)
            })?;
        tracing::info!(
            job_id = %job.id,
            camera_id = %job.camera_id,
            start_ms = job.start_ms,
            end_ms = job.end_ms,
            path = %job.path.display(),
            "Export started"
        );
        Ok(job.status())
    }

    pub fn status(&self, job_id: &str) -> Option<ExportStatus> {
        let jobs = self.jobs.lock().ok()?;
        jobs.by_id.get(job_id).map(|job| job.status())
    }

    /// Every kept job, oldest first.
    pub fn list(&self) -> Vec<ExportStatus> {
        let Ok(jobs) = self.jobs.lock() else {
            return Vec::new();
        };
        jobs.order
            .iter()
            .filter_map(|id| jobs.by_id.get(id))
            .map(|job| job.status())
            .collect()
    }

    /// Ask a job to stop; `false` when there is no such job or it already
    /// ended. The job ends as `cancelled` shortly after.
    pub fn cancel(&self, job_id: &str) -> bool {
        let Ok(jobs) = self.jobs.lock() else {
            return false;
        };
        match jobs.by_id.get(job_id) {
            Some(job) if job.running() => {
                job.cancel.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Stop every running job (backend shutdown): their partial files are
    /// removed.
    pub fn cancel_all(&self) {
        if let Ok(jobs) = self.jobs.lock() {
            for job in jobs.by_id.values() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Drop the oldest finished jobs beyond [`KEPT_FINISHED`].
fn forget_old(jobs: &mut Jobs) {
    let finished = jobs
        .order
        .iter()
        .filter(|id| jobs.by_id.get(*id).is_some_and(|job| !job.running()))
        .count();
    let mut excess = finished.saturating_sub(KEPT_FINISHED);
    let mut kept = VecDeque::with_capacity(jobs.order.len());
    for id in jobs.order.drain(..) {
        let finished = jobs.by_id.get(&id).is_some_and(|job| !job.running());
        if finished && excess > 0 {
            excess -= 1;
            jobs.by_id.remove(&id);
        } else {
            kept.push_back(id);
        }
    }
    jobs.order = kept;
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// The job's thread: write the `.part` file, then move it into place.
fn run(job: &Job, segments: &[PlaybackSegment]) {
    let part = part_path(&job.path);
    let mut report = |fraction: f64| {
        let scaled = (fraction.clamp(0.0, 0.999) * PROGRESS_SCALE) as u32;
        job.progress.fetch_max(scaled, Ordering::Relaxed);
    };
    let outcome = remux::remux(
        segments,
        job.start_ms,
        job.end_ms,
        &part,
        &job.cancel,
        &mut report,
    )
    .and_then(|summary| {
        std::fs::rename(&part, &job.path)?;
        let size_bytes = std::fs::metadata(&job.path).map_or(0, |m| m.len());
        Ok(JobState::Done {
            summary,
            size_bytes,
        })
    });
    let state = match outcome {
        Ok(done) => {
            tracing::info!(job_id = %job.id, path = %job.path.display(), "Export finished");
            done
        }
        Err(ExportError::Cancelled) => {
            tracing::info!(job_id = %job.id, "Export cancelled");
            JobState::Cancelled
        }
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "Export failed");
            JobState::Failed(error.to_string())
        }
    };
    if !matches!(state, JobState::Done { .. }) {
        let _ = std::fs::remove_file(&part);
    }
    job.finish(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(output: Option<PathBuf>, overwrite: bool) -> ExportRequest {
        ExportRequest {
            camera_id: "cam".into(),
            camera_name: "Front Door".into(),
            start_ms: 1_000,
            end_ms: 2_000,
            output,
            overwrite,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nw-export-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_range_must_be_forward_and_at_most_a_day() {
        assert!(validate_range(1_000, 2_000).is_ok());
        assert!(validate_range(2_000, 2_000).is_err());
        assert!(validate_range(3_000, 2_000).is_err());
        assert!(validate_range(-5, 2_000).is_err());
        assert!(validate_range(0, MAX_RANGE_MS).is_ok());
        assert!(validate_range(0, MAX_RANGE_MS + 1).is_err());
    }

    #[test]
    fn default_names_keep_only_safe_characters() {
        let name = default_file_name("Front door / porch", 0);
        assert!(name.starts_with("Front_door___porch_"), "{name}");
        assert!(name.ends_with(".mp4"));
        assert!(default_file_name("///", 0).starts_with("camera_"));
    }

    #[test]
    fn an_output_gets_mp4_and_must_be_new_unless_overwritten() {
        let dir = temp_dir("resolve");
        let plain = resolve_output(&request(Some(dir.join("clip")), false)).unwrap();
        assert_eq!(plain, dir.join("clip.mp4"));
        let upper = resolve_output(&request(Some(dir.join("clip.MP4")), false)).unwrap();
        assert_eq!(upper, dir.join("clip.MP4"));

        std::fs::write(dir.join("clip.mp4"), b"x").unwrap();
        assert!(matches!(
            resolve_output(&request(Some(dir.join("clip.mp4")), false)),
            Err(ExportError::Invalid(_))
        ));
        assert!(resolve_output(&request(Some(dir.join("clip.mp4")), true)).is_ok());

        assert!(resolve_output(&request(Some(PathBuf::from("relative.mp4")), true)).is_err());
        assert!(resolve_output(&request(Some(dir.join("missing").join("a.mp4")), true)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn taken_default_names_are_numbered() {
        let dir = temp_dir("unused");
        assert_eq!(unused_name(&dir, "a.mp4"), dir.join("a.mp4"));
        std::fs::write(dir.join("a.mp4"), b"x").unwrap();
        std::fs::write(dir.join("a (2).mp4"), b"x").unwrap();
        assert_eq!(unused_name(&dir, "a.mp4"), dir.join("a (3).mp4"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_estimate_counts_each_files_share_of_the_range() {
        let dir = temp_dir("estimate");
        let path = dir.join("seg.mkv");
        std::fs::write(&path, vec![0_u8; 1_000]).unwrap();
        let segment = PlaybackSegment {
            path: path.to_string_lossy().into_owned(),
            start_ts: 0,
            end_ts: 10_000,
        };
        assert_eq!(
            estimated_bytes(std::slice::from_ref(&segment), 0, 10_000),
            1_000
        );
        assert_eq!(
            estimated_bytes(std::slice::from_ref(&segment), 5_000, 20_000),
            500
        );
        assert_eq!(estimated_bytes(&[segment], 20_000, 30_000), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_job_without_recordings_is_refused() {
        let service = ExportService::new();
        assert!(matches!(
            service.start(&request(None, false), Vec::new()),
            Err(ExportError::NoRecordings)
        ));
        assert!(service.list().is_empty());
        assert!(!service.cancel("nope"));
        assert!(service.status("nope").is_none());
    }

    fn wait_until_finished(service: &ExportService, job_id: &str) -> ExportStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let status = service.status(job_id).expect("job kept");
            if status.state != "running" || std::time::Instant::now() > deadline {
                return status;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn a_job_writes_the_clip_under_its_name_and_reports_it() {
        let source = remux::tests::recording(2, false);
        let dir = temp_dir("job");
        let segments = vec![PlaybackSegment {
            path: source.clone(),
            start_ts: 1_000,
            end_ts: 3_000,
        }];
        let service = ExportService::new();
        let started = service
            .start(&request(Some(dir.join("clip")), false), segments)
            .unwrap();
        assert_eq!(started.path, dir.join("clip.mp4").to_string_lossy());
        let done = wait_until_finished(&service, &started.job_id);
        assert_eq!(done.state, "done", "{:?}", done.error);
        assert_eq!(done.progress, 1.0);
        assert_eq!(done.first_ms, Some(1_000));
        assert_eq!(done.frames, Some(25), "1 s of the 2 s file");
        assert!(done.size_bytes.unwrap_or(0) > 0);
        assert!(dir.join("clip.mp4").is_file());
        assert!(!dir.join("clip.mp4.part").exists());
        assert!(!dir.join("clip.mp4.part.moov").exists());
        assert!(
            !service.cancel(&started.job_id),
            "a finished job cannot be cancelled"
        );
        assert_eq!(service.list().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_file(source).unwrap();
    }

    #[test]
    fn a_job_that_fails_leaves_no_file() {
        let dir = temp_dir("failed");
        let segments = vec![PlaybackSegment {
            path: dir.join("missing.mkv").to_string_lossy().into_owned(),
            start_ts: 1_000,
            end_ts: 3_000,
        }];
        let service = ExportService::new();
        let started = service
            .start(&request(Some(dir.join("clip.mp4")), false), segments)
            .unwrap();
        let failed = wait_until_finished(&service, &started.job_id);
        assert_eq!(failed.state, "failed");
        assert!(failed.error.is_some());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "nothing left behind"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_finished_jobs_are_forgotten() {
        let mut jobs = Jobs::default();
        for n in 0..(KEPT_FINISHED + 3) {
            let job = Arc::new(Job {
                id: n.to_string(),
                camera_id: "cam".into(),
                start_ms: 0,
                end_ms: 1,
                path: PathBuf::from(format!("/tmp/{n}.mp4")),
                created_ms: 0,
                cancel: AtomicBool::new(false),
                progress: AtomicU32::new(0),
                state: Mutex::new(if n == 0 {
                    JobState::Running
                } else {
                    JobState::Cancelled
                }),
            });
            jobs.by_id.insert(job.id.clone(), job);
            jobs.order.push_back(n.to_string());
        }
        forget_old(&mut jobs);
        assert_eq!(jobs.order.len(), KEPT_FINISHED + 1);
        assert_eq!(
            jobs.order.front().map(String::as_str),
            Some("0"),
            "running kept"
        );
        assert!(!jobs.by_id.contains_key("1"));
    }
}
