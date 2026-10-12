//! Per-camera ingest: the camera's status and its recording, never decoding.
//!
//! One [`IngestSupervisor`] runs for every configured camera, independent of
//! the desktop view ([`super::supervisor`], which decodes only while a desktop
//! view shows the camera) and of browser live view:
//!
//! - **Recording**: the camera's main stream is received, depayloaded and
//!   parsed, and written as it is to segmented Matroska files
//!   ([`super::builder::encoded_recording_pipeline`]) on a connection of its
//!   own. Access units counted at `ingest_tap` are the camera's liveness, and
//!   its caps give codec and size. An RTSP camera is described first: its
//!   audio track, when Matroska stores it as it is, is recorded next to the
//!   video. If a recording with audio fails before any segment opened (a
//!   camera that describes audio but never sends it stalls the muxer), the
//!   next attempts record video only.
//! - **Not recording**: no media is received. Every
//!   [`METADATA_INTERVAL`] a metadata check asks the camera to describe its
//!   stream (RTSP DESCRIBE and SETUP, no PLAY), or checks that a non-RTSP
//!   source's port, device or file is there.
//!
//! Published status keeps the camera status contract: `connecting`,
//! `streaming`, `recording_ready` (a recording segment is open),
//! `reconnecting`, and `offline` from the third consecutive failure on.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;
use nw_core::camera::{CameraConfig, CameraId, Transport};
use nw_core::event::NeuroEvent;
use nw_core::traits::EventBus;
use tokio::sync::watch;

use super::builder;
use super::supervisor::{
    backoff_duration, backoff_slice, codec_from_structure, failure_status, jittered, probe_source,
    resolution_from_structure, StreamInfo, NO_FRAME_TIMEOUT, OFFLINE_AFTER_FAILURES,
};
use crate::stats::HealthCounters;

/// How often a camera that nobody records is asked to describe its stream.
pub const METADATA_INTERVAL: Duration = Duration::from_secs(30);
/// Longest wait for a camera to describe its stream.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
/// After the video track, how long to wait for an audio track to be described.
const AUDIO_DESCRIBE_GRACE: Duration = Duration::from_millis(1500);
/// Bound on finishing the open recording segment when stopping.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest single sleep, so a stop is observed promptly.
const POLL_SLICE: Duration = Duration::from_millis(100);

enum Attempt {
    Stopped,
    /// The camera answered; check again after the metadata interval.
    Healthy,
    Failed {
        reason: String,
        delivered: bool,
    },
}

pub struct IngestSupervisor {
    cam_id: CameraId,
    /// The camera with its main stream URL: what is recorded and checked.
    source: CameraConfig,
    recording: Option<(String, u32)>,
    event_bus: Arc<dyn EventBus>,
    stop_rx: watch::Receiver<bool>,
    state_tx: watch::Sender<String>,
    stream_info_tx: watch::Sender<StreamInfo>,
    health: Option<Arc<HealthCounters>>,
    /// Optional consumer of the recorded keyframes (see [`crate::taps`]).
    keyframes: Option<Arc<dyn crate::taps::KeyframeSink>>,
    /// Shared with the camera's desktop view: whoever gets data first ends
    /// the other's backoff.
    reachability: Option<Arc<super::reachability::Reachability>>,
    metadata_interval: Duration,
    failures: u32,
    /// The camera's audio track label, once described (`Some(None)`: none).
    audio: Option<Option<String>>,
    /// Recording with audio failed before a segment opened: record video only.
    audio_failed: bool,
    /// Whether the current recording attempt opened a segment.
    segment_opened: bool,
}

impl IngestSupervisor {
    pub fn new(
        source: CameraConfig,
        event_bus: Arc<dyn EventBus>,
        stop_rx: watch::Receiver<bool>,
        state_tx: watch::Sender<String>,
        stream_info_tx: watch::Sender<StreamInfo>,
    ) -> Self {
        Self {
            cam_id: source.id.clone(),
            source,
            recording: None,
            event_bus,
            stop_rx,
            state_tx,
            stream_info_tx,
            health: None,
            keyframes: None,
            reachability: None,
            metadata_interval: METADATA_INTERVAL,
            failures: 0,
            audio: None,
            audio_failed: false,
            segment_opened: false,
        }
    }

    /// Record the main stream into `storage_path` in `segment_minutes` segments.
    pub fn with_recording(mut self, storage_path: String, segment_minutes: u32) -> Self {
        self.recording = Some((storage_path, segment_minutes));
        self
    }

    pub fn with_health_counters(mut self, counters: Arc<HealthCounters>) -> Self {
        self.health = Some(counters);
        self
    }

    /// Keep each recorded keyframe (a reference, not a copy) in `store`, so a
    /// portal thumbnail decodes one picture instead of running a decoder.
    /// Share "the camera answered" with the camera's desktop view.
    pub fn with_reachability(
        mut self,
        reachability: Arc<super::reachability::Reachability>,
    ) -> Self {
        self.reachability = Some(reachability);
        self
    }

    pub fn with_keyframes(mut self, store: Arc<dyn crate::taps::KeyframeSink>) -> Self {
        self.keyframes = Some(store);
        self
    }

    #[cfg(test)]
    pub fn with_metadata_interval(mut self, interval: Duration) -> Self {
        self.metadata_interval = interval;
        self
    }

    /// Entry point; runs on a dedicated blocking thread until stopped.
    pub fn run(mut self) {
        tracing::info!(cam_id = %self.cam_id, recording = self.recording.is_some(), "Ingest started");
        self.publish_state("connecting");
        loop {
            if self.stopped() {
                break;
            }
            let attempt = if self.recording.is_some() {
                self.record()
            } else {
                self.check()
            };
            let (wait, after_failure) = match attempt {
                Attempt::Stopped => break,
                Attempt::Healthy => {
                    self.failures = 0;
                    (self.metadata_interval, false)
                }
                Attempt::Failed { reason, delivered } => {
                    if delivered {
                        self.failures = 0;
                    }
                    let attempt = self.failures;
                    self.failures = self.failures.saturating_add(1);
                    let status = failure_status(self.failures);
                    tracing::warn!(cam_id = %self.cam_id, attempt = self.failures, reason = %super::sanitize_credentials(&reason), "Ingest attempt failed");
                    self.publish_state(status);
                    self.notify_state(status, Some(&super::sanitize_credentials(&reason)));
                    (
                        jittered(backoff_duration(attempt), &self.cam_id, attempt),
                        true,
                    )
                }
            };
            if !self.rest(wait, after_failure) {
                break;
            }
        }
        self.publish_state("offline");
        tracing::info!(cam_id = %self.cam_id, "Ingest stopped");
    }

    fn stopped(&self) -> bool {
        *self.stop_rx.borrow() || self.stop_rx.has_changed().is_err()
    }

    /// Sleep for `wait` in slices; `false` once stopped. A backoff after a
    /// failure (`after_failure`) ends early when the camera's desktop view
    /// gets data: the camera is back, so retry now.
    fn rest(&self, wait: Duration, after_failure: bool) -> bool {
        let until = Instant::now() + wait;
        let answers = self
            .reachability
            .as_ref()
            .filter(|_| after_failure)
            .map(|r| (r, r.mark()));
        while let Some(slice) = backoff_slice(until, Instant::now()) {
            if self.stopped() {
                return false;
            }
            if answers.is_some_and(|(r, mark)| r.answered_since(mark)) {
                tracing::info!(cam_id = %self.cam_id, "Camera answered its desktop view; retrying now");
                break;
            }
            std::thread::sleep(slice);
        }
        !self.stopped()
    }

    fn publish_state(&self, state: &str) {
        let changed = self.state_tx.send_if_modified(|current| {
            if current == state {
                return false;
            }
            *current = state.to_owned();
            true
        });
        if changed && matches!(state, "streaming" | "recording_ready") {
            if let Some(reachability) = &self.reachability {
                reachability.answered();
            }
        }
        if let Some(health) = &self.health {
            health.set_state(state);
        }
    }

    fn publish_stream_info(&self, info: StreamInfo) {
        self.stream_info_tx.send_if_modified(|current| {
            if *current == info {
                return false;
            }
            *current = info;
            true
        });
    }

    fn notify_state(&self, state: &str, detail: Option<&str>) {
        self.event_bus.publish(NeuroEvent::PipelineState {
            cam_id: self.cam_id.clone(),
            state: state.into(),
            detail: detail.map(String::from),
        });
    }

    fn announce_attempt(&self) {
        if self.failures < OFFLINE_AFTER_FAILURES {
            // Once offline, stay offline until the camera answers again.
            self.publish_state(if self.failures > 0 {
                "reconnecting"
            } else {
                "connecting"
            });
        }
    }

    // -- recording ---------------------------------------------------------------

    fn record(&mut self) -> Attempt {
        let failed = |reason: String| Attempt::Failed {
            reason,
            delivered: false,
        };
        if let Err(reason) = probe_source(&self.source.url) {
            return failed(reason);
        }
        let Some((storage_path, segment_minutes)) = self.recording.clone() else {
            return failed("not recording".into());
        };
        let with_audio = self.recordable_audio();
        let Some(description) = builder::encoded_recording_pipeline(
            &self.source,
            &self.cam_id,
            &storage_path,
            segment_minutes,
            with_audio,
        ) else {
            return failed("this source has no compressed video to record".into());
        };
        self.announce_attempt();
        self.segment_opened = false;
        let attempt = match builder::build(&description) {
            Ok(pipeline) => self.run_recording(&pipeline),
            Err(error) => failed(error.to_string()),
        };
        if with_audio && !self.segment_opened && matches!(attempt, Attempt::Failed { .. }) {
            self.audio_failed = true;
            tracing::warn!(cam_id = %self.cam_id, "Recording with audio did not start; recording video only");
        }
        attempt
    }

    /// Whether to record the camera's audio: an RTSP camera that describes
    /// an audio track Matroska stores as it is, unless that failed before.
    fn recordable_audio(&mut self) -> bool {
        let url = &self.source.url;
        if self.audio_failed || !(url.starts_with("rtsp://") || url.starts_with("rtsps://")) {
            return false;
        }
        if self.audio.is_none() {
            // A camera that cannot describe itself is still recorded (video).
            match describe(&self.source) {
                Ok(info) => {
                    if let Some(label) = &info.audio {
                        tracing::info!(cam_id = %self.cam_id, audio = %label, "Camera sends audio");
                    }
                    self.audio = Some(info.audio);
                }
                Err(reason) => {
                    tracing::debug!(cam_id = %self.cam_id, reason = %super::sanitize_credentials(&reason), "Could not describe the camera's audio");
                    return false;
                }
            }
        }
        self.audio
            .as_ref()
            .and_then(Option::as_deref)
            .is_some_and(super::audio::recordable)
    }

    /// Run a built recording pipeline until it fails or the ingest stops.
    fn run_recording(&mut self, pipeline: &gstreamer::Pipeline) -> Attempt {
        let units = Arc::new(AtomicU64::new(0));
        let Some(tap) = pipeline.by_name("ingest_tap") else {
            return Attempt::Failed {
                reason: "recording pipeline has no ingest tap".into(),
                delivered: false,
            };
        };
        if let Some(src) = tap.static_pad("src") {
            let (counter, health) = (Arc::clone(&units), self.health.clone());
            let (keyframes, cam_id) = (self.keyframes.clone(), self.cam_id.clone());
            src.add_probe(gstreamer::PadProbeType::BUFFER, move |pad, info| {
                counter.fetch_add(1, Ordering::Relaxed);
                if let Some(health) = &health {
                    health.record_ingested();
                }
                if let (Some(store), Some(buffer)) = (&keyframes, info.buffer()) {
                    if !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT) {
                        if let Some(caps) = pad.current_caps() {
                            store.record(&cam_id, caps, buffer.to_owned());
                        }
                    }
                }
                gstreamer::PadProbeReturn::Ok
            });
        }
        warn_when_the_writer_falls_behind(pipeline, &self.cam_id);
        let outcome = self.watch_recording(pipeline, &tap, &units);
        let _ = pipeline.set_state(gstreamer::State::Null);
        let _ = pipeline.state(Some(gstreamer::ClockTime::from_seconds(5)));
        outcome
    }

    fn watch_recording(
        &mut self,
        pipeline: &gstreamer::Pipeline,
        tap: &gstreamer::Element,
        units: &AtomicU64,
    ) -> Attempt {
        let failed = |reason: &str, units: &AtomicU64| Attempt::Failed {
            reason: reason.to_owned(),
            delivered: units.load(Ordering::Relaxed) > 0,
        };
        if pipeline.set_state(gstreamer::State::Playing).is_err() {
            return failed("recording pipeline would not start", units);
        }
        let Some(bus) = pipeline.bus() else {
            return failed("recording pipeline has no bus", units);
        };
        let mut segment_open = false;
        let mut info_complete = false;
        let mut last_units = 0;
        let mut last_progress = Instant::now();
        loop {
            if self.stopped() {
                finalize(pipeline, &self.cam_id);
                return Attempt::Stopped;
            }
            let delivered = units.load(Ordering::Relaxed);
            if delivered != last_units {
                if last_units == 0 {
                    self.failures = 0;
                    let status = if segment_open {
                        "recording_ready"
                    } else {
                        "streaming"
                    };
                    self.publish_state(status);
                    self.notify_state(status, None);
                }
                last_units = delivered;
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= NO_FRAME_TIMEOUT {
                finalize(pipeline, &self.cam_id);
                return failed(
                    if delivered == 0 {
                        "No video received from camera"
                    } else {
                        "Video stopped arriving from camera"
                    },
                    units,
                );
            }
            if !info_complete {
                if let Some(mut info) = tap_stream_info(tap) {
                    info_complete = info.resolution.is_some();
                    info.audio = self.audio.clone().flatten();
                    self.publish_stream_info(info);
                }
            }
            let Some(message) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(
                POLL_SLICE.as_millis() as u64 * 5,
            )) else {
                continue;
            };
            match message.view() {
                gstreamer::MessageView::Error(error) => {
                    return failed(
                        &format!("{}: {}", error.error(), error.debug().unwrap_or_default()),
                        units,
                    );
                }
                gstreamer::MessageView::Eos(_) => {
                    // A file source ended (it restarts at once) or a camera
                    // closed the stream (it reconnects with backoff).
                    return failed("Stream ended (EOS)", units);
                }
                gstreamer::MessageView::Element(element)
                    if !segment_open
                        && element
                            .structure()
                            .is_some_and(|s| s.name() == "splitmuxsink-fragment-opened") =>
                {
                    segment_open = true;
                    self.segment_opened = true;
                    if last_units > 0 {
                        self.publish_state("recording_ready");
                        self.notify_state("recording_ready", None);
                    }
                    tracing::info!(cam_id = %self.cam_id, "Recording segment opened");
                }
                _ => {}
            }
        }
    }

    // -- metadata check ------------------------------------------------------------

    fn check(&mut self) -> Attempt {
        let url = self.source.url.clone();
        let failed = |reason: String| Attempt::Failed {
            reason,
            delivered: false,
        };
        let outcome = if url.starts_with("rtsp://") || url.starts_with("rtsps://") {
            if let Err(reason) = probe_source(&url) {
                return failed(reason);
            }
            self.announce_attempt();
            describe(&self.source).map(Some)
        } else if url.starts_with("http://")
            || url.starts_with("https://")
            || url.starts_with("rtmp://")
        {
            probe_source(&url).map(|()| None)
        } else if std::path::Path::new(&url).exists() {
            Ok(None)
        } else {
            Err("Camera device or file not found".to_owned())
        };
        match outcome {
            Ok(info) => {
                if let Some(info) = info {
                    self.publish_stream_info(info);
                }
                if self.state_tx.borrow().as_str() != "streaming" {
                    self.notify_state("streaming", None);
                }
                self.publish_state("streaming");
                Attempt::Healthy
            }
            Err(reason) => failed(reason),
        }
    }
}

/// Codec and size from the caps reaching the recording's tap, once known.
fn tap_stream_info(tap: &gstreamer::Element) -> Option<StreamInfo> {
    let caps = tap.static_pad("sink")?.current_caps()?;
    let structure = caps.structure(0)?;
    Some(StreamInfo {
        codec: codec_from_structure(structure)?,
        resolution: resolution_from_structure(structure),
        audio: None,
    })
}

/// Give `splitmuxsink` a bounded chance to finish the open segment, so the
/// last file keeps its index and duration.
fn finalize(pipeline: &gstreamer::Pipeline, cam_id: &str) {
    if !pipeline.send_event(gstreamer::event::Eos::new()) {
        return;
    }
    let Some(bus) = pipeline.bus() else {
        return;
    };
    let deadline = Instant::now() + FINALIZE_TIMEOUT;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Some(message) = bus.timed_pop(gstreamer::ClockTime::from_nseconds(
            u64::try_from(remaining.as_nanos()).unwrap_or(u64::MAX),
        )) else {
            break;
        };
        match message.view() {
            gstreamer::MessageView::Eos(_) | gstreamer::MessageView::Error(_) => return,
            _ => {}
        }
    }
    tracing::warn!(cam_id, "Timed out finishing the recording segment");
}

/// Ask an RTSP camera to describe and set up its video stream, without
/// playing it: no media is received and nothing is decoded. Returns the
/// stream's codec (its size is not part of the description) and its audio
/// track, if it has one.
pub(crate) fn describe(camera: &CameraConfig) -> Result<StreamInfo, String> {
    let location = builder::gst_quote(&builder::url_with_credentials_str(
        &camera.url,
        camera.username.as_deref(),
        camera.password_b64.as_deref(),
    ));
    let protocols = match camera.transport {
        Transport::Tcp => "tcp",
        Transport::Udp => "udp",
    };
    let pipeline = builder::build(&format!(
        "rtspsrc name=describe location=\"{location}\" protocols={protocols} latency=100 ! fakesink"
    ))
    .map_err(|error| error.to_string())?;
    let found: Arc<Mutex<Option<String>>> = Arc::default();
    let audio: Arc<Mutex<Option<String>>> = Arc::default();
    if let Some(source) = pipeline.by_name("describe") {
        let found = Arc::clone(&found);
        let audio = Arc::clone(&audio);
        // Emitted for each stream the camera describes, with its RTP caps;
        // pads only appear once media is played.
        source.connect("select-stream", false, move |values| {
            let caps = values
                .get(2)
                .and_then(|value| value.get::<gstreamer::Caps>().ok());
            if let Some(structure) = caps.as_ref().and_then(|caps| caps.structure(0)) {
                if let Some(label) = super::audio::audio_from_rtp(structure) {
                    if let Ok(mut audio) = audio.lock() {
                        audio.get_or_insert(label);
                    }
                }
                if structure.get::<String>("media").ok().as_deref() == Some("video") {
                    let codec = codec_from_structure(structure).unwrap_or_else(|| "CODEC?".into());
                    if let Ok(mut found) = found.lock() {
                        found.get_or_insert(codec);
                    }
                }
            }
            Some(true.to_value())
        });
    }
    // PAUSED connects and describes; only PLAYING would start media.
    let outcome = (|| {
        pipeline
            .set_state(gstreamer::State::Paused)
            .map_err(|_| "Camera did not accept the connection".to_owned())?;
        let bus = pipeline.bus().ok_or("no bus")?;
        let deadline = Instant::now() + DESCRIBE_TIMEOUT;
        let mut video_seen: Option<Instant> = None;
        while Instant::now() < deadline {
            if let Some(codec) = found.lock().ok().and_then(|found| found.clone()) {
                // Streams are selected one after another (each is set up in
                // between), so an audio track listed after the video comes a
                // moment later: wait for it briefly.
                let seen = *video_seen.get_or_insert_with(Instant::now);
                let audio = audio.lock().ok().and_then(|audio| audio.clone());
                if audio.is_some() || seen.elapsed() >= AUDIO_DESCRIBE_GRACE {
                    return Ok(StreamInfo {
                        codec,
                        resolution: None,
                        audio,
                    });
                }
            }
            if let Some(message) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(100)) {
                if let gstreamer::MessageView::Error(error) = message.view() {
                    return Err(error.error().to_string());
                }
            }
        }
        Err("Camera did not describe a video stream".to_owned())
    })();
    let _ = pipeline.set_state(gstreamer::State::Null);
    outcome
}

/// Log when the segment writer stalled long enough to fill the recording
/// queue (`builder::RECORDING_STALL_NS`): from then on the camera connection
/// waits and video is lost. At most once a minute per recording.
fn warn_when_the_writer_falls_behind(pipeline: &gstreamer::Pipeline, cam_id: &str) {
    let Some(queue) = pipeline.by_name(super::builder::RECORDING_QUEUE) else {
        return;
    };
    let cam_id = cam_id.to_owned();
    let last_warned = std::sync::Mutex::new(None::<std::time::Instant>);
    queue.connect("overrun", false, move |_| {
        let now = std::time::Instant::now();
        if let Ok(mut last) = last_warned.lock() {
            let due = match *last {
                Some(at) => now.duration_since(at) >= std::time::Duration::from_secs(60),
                None => true,
            };
            if due {
                *last = Some(now);
                tracing::warn!(
                    cam_id = %cam_id,
                    stall_s = super::builder::RECORDING_STALL_NS / 1_000_000_000,
                    "Recording fell behind writing to disk; video may be missing. \
                     A slow or busy drive, or an antivirus scanning recordings, can cause this"
                );
            }
        }
        None
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::traits::LogEventBus;

    /// The latest keyframe handed on, and whose it was.
    #[derive(Default)]
    struct LatestKeyframe(std::sync::Mutex<Option<(String, gstreamer::Caps, gstreamer::Buffer)>>);

    impl crate::taps::KeyframeSink for LatestKeyframe {
        fn record(&self, camera_id: &str, caps: gstreamer::Caps, buffer: gstreamer::Buffer) {
            *self.0.lock().unwrap() = Some((camera_id.to_owned(), caps, buffer));
        }
    }

    struct Harness {
        stop: watch::Sender<bool>,
        states: watch::Receiver<String>,
        infos: watch::Receiver<StreamInfo>,
        task: std::thread::JoinHandle<()>,
    }

    fn start(
        supervisor: impl FnOnce(IngestSupervisor) -> IngestSupervisor,
        camera: CameraConfig,
    ) -> Harness {
        let (stop, stop_rx) = watch::channel(false);
        let (state_tx, states) = watch::channel(String::new());
        let (info_tx, infos) = watch::channel(StreamInfo::default());
        let ingest = supervisor(IngestSupervisor::new(
            camera,
            Arc::new(LogEventBus),
            stop_rx,
            state_tx,
            info_tx,
        ));
        Harness {
            stop,
            states,
            infos,
            task: std::thread::spawn(move || ingest.run()),
        }
    }

    #[test]
    fn a_backoff_ends_when_the_camera_answers_its_desktop_view() {
        let reach = Arc::new(crate::pipeline::reachability::Reachability::default());
        let (_stop, stop_rx) = watch::channel(false);
        let (state_tx, _states) = watch::channel(String::new());
        let (info_tx, _infos) = watch::channel(StreamInfo::default());
        let ingest = IngestSupervisor::new(
            CameraConfig::new("Door", "rtsp://127.0.0.1:9/unreachable"),
            Arc::new(LogEventBus),
            stop_rx,
            state_tx,
            info_tx,
        )
        .with_reachability(Arc::clone(&reach));

        // A backoff after a failure ends as soon as the view gets data.
        let waker = {
            let reach = Arc::clone(&reach);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                reach.answered();
            })
        };
        let started = Instant::now();
        assert!(ingest.rest(Duration::from_secs(30), true));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        waker.join().unwrap();

        // A healthy ingest's wait between checks is not cut short.
        let started = Instant::now();
        reach.answered();
        assert!(ingest.rest(Duration::from_millis(400), false));
        assert!(started.elapsed() >= Duration::from_millis(400));

        // Going live tells the view.
        let mark = reach.mark();
        ingest.publish_state("streaming");
        assert!(reach.answered_since(mark));
        let mark = reach.mark();
        ingest.publish_state("streaming");
        assert!(!reach.answered_since(mark), "only a change to live counts");
    }

    fn wait_for(states: &watch::Receiver<String>, wanted: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if states.borrow().as_str() == wanted {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_missing_local_source_goes_offline_after_three_failures() {
        let camera = CameraConfig::new("Cam", "/nonexistent/neurowatch-camera.mp4");
        let harness = start(|s| s, camera);
        // 1 s + 2 s of backoff (±20%) between the three failed checks.
        assert!(wait_for(
            &harness.states,
            "reconnecting",
            Duration::from_secs(2)
        ));
        assert!(wait_for(&harness.states, "offline", Duration::from_secs(6)));
        harness.stop.send(true).unwrap();
        harness.task.join().unwrap();
    }

    #[test]
    fn a_present_device_is_reported_streaming_without_receiving_media() {
        let file = std::env::temp_dir().join(format!("nw-ingest-{}.mp4", uuid::Uuid::new_v4()));
        std::fs::write(&file, b"not opened").unwrap();
        let camera = CameraConfig::new("Cam", file.to_str().unwrap());
        let harness = start(
            |s| s.with_metadata_interval(Duration::from_millis(50)),
            camera,
        );
        assert!(wait_for(
            &harness.states,
            "streaming",
            Duration::from_secs(2)
        ));
        harness.stop.send(true).unwrap();
        harness.task.join().unwrap();
        assert_eq!(
            harness.states.borrow().as_str(),
            "offline",
            "stopped ingest reads offline"
        );
        std::fs::remove_file(file).unwrap();
    }

    /// Against a real RTSP server (`NW_RTSP_TEST_URL`, e.g. MediaMTX fed by
    /// ffmpeg): records without decoding, and checks status by DESCRIBE only.
    #[test]
    #[ignore = "requires an RTSP H.264 stream at NW_RTSP_TEST_URL"]
    fn rtsp_recording_and_metadata_checks_against_a_real_server() {
        gstreamer::init().unwrap();
        let url = std::env::var("NW_RTSP_TEST_URL").expect("NW_RTSP_TEST_URL");
        let camera = CameraConfig::new("Cam", &url);

        let info = describe(&camera).expect("the camera describes its stream");
        assert_eq!(info.codec, "H264");
        let missing = CameraConfig::new("Cam", format!("{}-missing", url.trim_end_matches('/')));
        assert!(describe(&missing).is_err());

        let monitor = start(|s| s, camera.clone());
        assert!(wait_for(
            &monitor.states,
            "streaming",
            Duration::from_secs(10)
        ));
        assert_eq!(monitor.infos.borrow().codec, "H264");
        monitor.stop.send(true).unwrap();
        monitor.task.join().unwrap();

        let dir =
            std::env::temp_dir().join(format!("nw-ingest-rec-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let health = Arc::new(HealthCounters::default());
        let counters = Arc::clone(&health);
        let keyframes = Arc::new(LatestKeyframe::default());
        let store: Arc<dyn crate::taps::KeyframeSink> = keyframes.clone();
        let cam_id = camera.id.clone();
        let recorder = start(
            |s| {
                s.with_recording(dir.to_string_lossy().into(), 1)
                    .with_health_counters(counters)
                    .with_keyframes(store)
            },
            camera,
        );
        assert!(wait_for(
            &recorder.states,
            "recording_ready",
            Duration::from_secs(15)
        ));
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            recorder.infos.borrow().resolution.is_some(),
            "size from the parser's caps"
        );
        let (camera_seen, caps, keyframe) = keyframes
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("the recording hands its keyframes on");
        assert_eq!(camera_seen, cam_id);
        assert!(
            caps.structure(0)
                .is_some_and(|s| s.name().starts_with("video/")),
            "{caps}"
        );
        assert!(
            !keyframe
                .flags()
                .contains(gstreamer::BufferFlags::DELTA_UNIT)
                && keyframe.size() > 0,
            "only keyframes are handed on"
        );
        recorder.stop.send(true).unwrap();
        recorder.task.join().unwrap();
        assert!(health.ingested_units.load(Ordering::Relaxed) > 30);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 1);
        assert!(
            files[0].metadata().unwrap().len() > 10_000,
            "a finalized segment"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
