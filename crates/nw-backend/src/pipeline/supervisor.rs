//! Per-camera desktop view supervisor: decodes a camera for the desktop wall.
//!
//! Runs only while a desktop view shows the camera (see
//! `AppState::reconcile_views`); recording and camera status belong to the
//! encoded ingest ([`super::ingest`]), which never decodes. Manages the
//! decode pipeline's lifecycle with exponential backoff reconnection.
//!
//! State machine:
//!   Connecting → Streaming → Error → BackoffWait → Reconnecting → Stopped
//!
//! Published status (what the UI and cloud see) deliberately lags the
//! internal state so an unreachable camera does not flicker:
//! - `streaming` only once the pipeline has delivered a decoded frame;
//! - `connecting` / `reconnecting` during the first failed attempts;
//! - `offline` from [`OFFLINE_AFTER_FAILURES`] consecutive failures on, and
//!   it stays `offline` through later retries until frames arrive again.
//!
//! Retries first probe the camera's TCP port so an unreachable camera does
//! not cost a full decode/recording pipeline build on every attempt.

use std::hash::{BuildHasher, Hasher};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;
use gstreamer_video::VideoFrameExt;
use nw_core::camera::{CameraConfig, CameraId};
use nw_core::control_plane::CameraAssignment;
use nw_core::event::NeuroEvent;
use nw_core::traits::EventBus;
use tokio::sync::watch;

use super::reachability::Reachability;
use super::{builder, hwaccel::HwAccelKind};
use crate::assignment::{ActiveAssignmentLease, AssignmentRegistry};
use crate::shm::ring_buffer::ShmRingBufferWriter;
use crate::stats::HealthCounters;
use crate::taps::FrameTap;

const MAX_BACKOFF_SECS: u64 = 30;
/// Longest single sleep while waiting out a backoff, so stop and assignment
/// revocation are still observed promptly on the blocking supervisor thread.
const BACKOFF_POLL_SLICE: Duration = Duration::from_millis(100);
/// Consecutive failures after which a camera is reported `offline` rather
/// than `reconnecting`. Short blips stay amber; a dead camera goes offline
/// after roughly 1 + 2 seconds of backoff and stays there without flicker.
pub(super) const OFFLINE_AFTER_FAILURES: u32 = 3;
/// A running pipeline that delivers no decoded frame for this long (at start
/// or mid-stream) is treated as failed, so a source that accepts the
/// connection but sends no video does not sit in `connecting` forever.
pub(super) const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound for the cheap reachability probe before a pipeline build.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Cap on elements visited while walking upstream from the live sink.
const MAX_UPSTREAM_ELEMENTS: usize = 64;

/// Best-effort description of a camera's live-view input stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// `H264`, `H265`, `MJPEG`, … or `CODEC?` until negotiated.
    pub codec: String,
    /// Decoded picture size (width, height), once negotiated.
    pub resolution: Option<(u32, u32)>,
    /// The camera's audio track (`AAC`, `G.711 A-law`, …), from its stream
    /// description; `None` without one (or before it was described).
    pub audio: Option<String>,
}

impl Default for StreamInfo {
    fn default() -> Self {
        Self {
            codec: "CODEC?".to_owned(),
            resolution: None,
            audio: None,
        }
    }
}

impl StreamInfo {
    fn is_complete(&self) -> bool {
        self.codec != "CODEC?" && self.resolution.is_some()
    }
}

#[derive(Debug, Clone)]
enum State {
    Connecting,
    Streaming,
    Error {
        reason: String,
    },
    /// `answers`: the camera's [`Reachability`] mark when the wait began.
    BackoffWait {
        until: Instant,
        attempt: u32,
        answers: u64,
    },
    Reconnecting {
        attempt: u32,
    },
    Stopped,
}

pub struct PipelineSupervisor {
    cam_id: CameraId,
    config: CameraConfig,
    hwaccel: HwAccelKind,
    state: State,
    pipeline: Option<gstreamer::Pipeline>,
    shm_writer: Option<Arc<StdMutex<ShmRingBufferWriter>>>,
    /// Decoded frames are counted, so `streaming` waits for real frames and a
    /// silent camera is detected.
    frames_counted: bool,
    /// No desktop UI: skip the SHM ring and convert frames only on demand.
    headless: bool,
    event_bus: Arc<dyn EventBus>,
    stop_rx: watch::Receiver<bool>,
    /// Publishes the current pipeline state string so AppState can serve it
    /// via the `camera.status_all` IPC method without acquiring any internal lock.
    state_tx: watch::Sender<String>,
    /// Publishes the best-effort detected input codec and resolution.
    stream_info_tx: watch::Sender<StreamInfo>,
    stream_info_complete: bool,
    assignment: Option<CameraAssignment>,
    assignment_registry: Option<Arc<dyn AssignmentRegistry>>,
    /// Held from before first activation until the pipeline and its callbacks
    /// have stopped. A replacement waits for this lease to be released.
    active_assignment: Option<Box<dyn ActiveAssignmentLease>>,
    /// Optional per-camera health counters. Bumped on decoded frame + state
    /// transition so the stats aggregator can turn counts into rates.
    health: Option<Arc<HealthCounters>>,
    /// Optional consumer of decoded frames (see [`crate::taps`]).
    frame_tap: Option<Arc<dyn FrameTap>>,
    /// Shared with the camera's ingest: whoever gets data first ends the
    /// other's backoff.
    reachability: Option<Arc<Reachability>>,
    /// Consecutive failed connections. Only reset once a frame is decoded:
    /// `set_state(Playing)` succeeds asynchronously for RTSP, so reaching
    /// `Streaming` does not prove the camera is reachable.
    reconnect_attempt: u32,
    /// Decoded input frames, before optional preview pacing.
    frame_counter: Arc<AtomicU64>,
}

impl PipelineSupervisor {
    pub fn new(
        config: CameraConfig,
        hwaccel: HwAccelKind,
        event_bus: Arc<dyn EventBus>,
        stop_rx: watch::Receiver<bool>,
        state_tx: watch::Sender<String>,
        stream_info_tx: watch::Sender<StreamInfo>,
    ) -> Self {
        let cam_id = config.id.clone();
        Self {
            cam_id,
            config,
            hwaccel,
            state: State::Connecting,
            pipeline: None,
            shm_writer: None,
            frames_counted: false,
            headless: false,
            event_bus,
            stop_rx,
            state_tx,
            stream_info_tx,
            stream_info_complete: false,
            assignment: None,
            assignment_registry: None,
            active_assignment: None,
            health: None,
            frame_tap: None,
            reachability: None,
            reconnect_attempt: 0,
            frame_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Attach health counters that will be incremented as frames flow.
    pub fn with_health_counters(mut self, counters: Arc<HealthCounters>) -> Self {
        self.health = Some(counters);
        self
    }

    /// Fence this supervisor to an assignment before it is run.
    pub fn with_assignment(
        mut self,
        assignment: CameraAssignment,
        assignment_registry: Arc<dyn AssignmentRegistry>,
    ) -> Self {
        self.assignment = Some(assignment);
        self.assignment_registry = Some(assignment_registry);
        self
    }

    /// Entry point — run on a dedicated blocking thread.
    pub fn run(mut self) {
        tracing::info!(cam_id = %self.cam_id, "Supervisor started");
        self.publish_state("connecting");
        if !self.acquire_assignment() {
            self.state = State::Stopped;
        }
        loop {
            if *self.stop_rx.borrow() {
                self.state = State::Stopped;
            }
            if !self.assignment_is_current() {
                self.state = State::Stopped;
            }
            match self.state.clone() {
                State::Connecting | State::Reconnecting { .. } => {
                    self.do_connect();
                }
                State::Streaming => {
                    self.do_stream();
                }
                State::Error { reason } => {
                    let attempt = self.reconnect_attempt;
                    let safe_reason = super::sanitize_credentials(&reason);
                    tracing::warn!(
                        cam_id = %self.cam_id,
                        state = "error",
                        attempt,
                        reason = %safe_reason,
                        "Supervisor state changed"
                    );
                    if !self.destroy_pipeline() {
                        // Do not reconnect or release the lease after an
                        // unverified teardown; the Stopped path fails closed.
                        self.state = State::Stopped;
                        continue;
                    }
                    self.reconnect_attempt = attempt.saturating_add(1);
                    let status = failure_status(self.reconnect_attempt);
                    self.publish_state(status);
                    self.notify_state(status, Some(&safe_reason));
                    let wait = jittered(backoff_duration(attempt), &self.cam_id, attempt);
                    tracing::info!(
                        cam_id = %self.cam_id,
                        attempt = self.reconnect_attempt,
                        wait_ms = wait.as_millis(),
                        "Waiting before reconnect"
                    );
                    self.state = State::BackoffWait {
                        until: Instant::now() + wait,
                        attempt: self.reconnect_attempt,
                        answers: self.reachability.as_ref().map_or(0, |r| r.mark()),
                    };
                }
                State::BackoffWait {
                    until,
                    attempt,
                    answers,
                } => {
                    // The camera's ingest got data: retry now, not after the backoff.
                    if self
                        .reachability
                        .as_ref()
                        .is_some_and(|r| r.answered_since(answers))
                    {
                        tracing::info!(cam_id = %self.cam_id, "Camera answered its ingest; reconnecting now");
                        self.state = State::Reconnecting { attempt };
                        continue;
                    }
                    // Supervisors run on a blocking thread, so wait out the
                    // backoff in bounded slices; the loop head re-checks
                    // stop/revocation between slices.
                    match backoff_slice(until, Instant::now()) {
                        Some(slice) => std::thread::sleep(slice),
                        None => self.state = State::Reconnecting { attempt },
                    }
                }
                State::Stopped => {
                    if self.destroy_pipeline() {
                        // A replacement is allowed to proceed only after the
                        // pipeline is NULL and appsink callbacks are gone.
                        if let Some(lease) = self.active_assignment.take() {
                            lease.release();
                        }
                        self.publish_state("offline");
                        tracing::info!(cam_id = %self.cam_id, "Supervisor stopped");
                    } else {
                        // Teardown was not verified. All owned resources
                        // (pipeline, SHM writer, assignment lease) remain in
                        // `self` and are dropped normally when the supervisor
                        // exits. The assignment lease does NOT have a Drop-based
                        // release — its explicit `release()` is the only path
                        // that clears the registry entry — so the assignment
                        // fence remains in place deterministically.
                        self.publish_state("teardown_failed");
                        tracing::error!(cam_id = %self.cam_id, "Pipeline teardown was not verified; assignment remains fenced");
                    }
                    break;
                }
            }
        }
    }

    fn do_connect(&mut self) {
        if !self.assignment_is_current() {
            self.state = State::Stopped;
            return;
        }
        let attempt = match &self.state {
            State::Reconnecting { attempt } => *attempt,
            _ => 0,
        };
        if attempt > 0 {
            tracing::warn!(cam_id = %self.cam_id, attempt, "Reconnect attempt");
        }
        if let Err(reason) = probe_source(&self.config.url) {
            self.state = State::Error { reason };
            return;
        }
        if self.reconnect_attempt < OFFLINE_AFTER_FAILURES {
            // Once offline, stay offline until frames prove otherwise.
            let status = if attempt > 0 {
                "reconnecting"
            } else {
                "connecting"
            };
            self.publish_state(status);
        }
        tracing::info!(cam_id = %self.cam_id, state = "connecting", "Supervisor state changed");
        self.stream_info_complete = false;
        self.publish_stream_info(StreamInfo::default());
        let pipeline_str = builder::select_pipeline(&self.config, self.hwaccel);
        tracing::info!(
            cam_id = %self.cam_id,
            pipeline_len = pipeline_str.len(),
            hwaccel = ?self.hwaccel,
            "Building pipeline"
        );
        let pipeline = match builder::build(&pipeline_str) {
            Ok(p) => p,
            Err(e) => {
                let reason = super::sanitize_credentials(&e.to_string());
                tracing::error!(cam_id = %self.cam_id, error = %reason, "Pipeline build failed");
                self.state = State::Error { reason };
                return;
            }
        };

        // Wire appsink → SHM ring buffer before going to PLAYING
        let (shm_writer, frames_counted) = self.wire_appsink(&pipeline);

        if !self.assignment_is_current() {
            self.state = State::Stopped;
            return;
        }
        if pipeline.set_state(gstreamer::State::Playing).is_err() {
            tracing::error!(cam_id = %self.cam_id, "Failed to set pipeline to PLAYING");
            self.state = State::Error {
                reason: "Failed to set pipeline to PLAYING".into(),
            };
            return;
        }

        // RTSP negotiates after PLAYING returns; the bus loop retries until
        // both codec and resolution are known.
        self.try_publish_stream_info(&pipeline);
        self.pipeline = Some(pipeline);
        self.shm_writer = shm_writer;
        self.frames_counted = frames_counted;
        self.state = State::Streaming;
        if !self.frames_counted {
            // Without a frame count there is nothing to wait for.
            self.mark_streaming();
        }
        tracing::info!(cam_id = %self.cam_id, "Pipeline started");
    }

    /// Get the appsink, create a SHM writer, and wire callbacks.
    /// Returns the writer Arc so the supervisor retains a reference for lifecycle management.
    /// Run without a desktop UI (`./neurowatch.sh --headless`).
    pub fn with_headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    pub fn with_frame_tap(mut self, tap: Arc<dyn FrameTap>) -> Self {
        self.frame_tap = Some(tap);
        self
    }

    /// Share "the camera answered" with the camera's ingest.
    pub fn with_reachability(mut self, reachability: Arc<Reachability>) -> Self {
        self.reachability = Some(reachability);
        self
    }

    /// Whether a [`FrameTap`] wants this camera's converted frames right now.
    fn frame_demand(&self) -> super::preview_rate::FrameDemand {
        let tap = self.frame_tap.clone();
        let cam_id = self.cam_id.clone();
        Arc::new(move || tap.as_ref().is_some_and(|tap| tap.wants(&cam_id)))
    }

    /// Headless: no SHM ring, and the RGB conversion runs only for frames a
    /// consumer demands. Returns whether decoded frames are counted.
    fn wire_headless_appsink(&self, appsink: &gstreamer_app::AppSink) -> bool {
        self.frame_counter.store(0, AtomicOrdering::Relaxed);
        let counted = super::preview_rate::install_demand_gate(
            appsink,
            Arc::clone(&self.frame_counter),
            self.health.clone(),
            self.frame_demand(),
        );
        let frame_tap = self.frame_tap.clone();
        let cam_id = self.cam_id.clone();
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink
                        .pull_sample()
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    if let Some(tap) = &frame_tap {
                        tap.submit_sample(&cam_id, &sample);
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );
        tracing::debug!(cam_id = %self.cam_id, counted, "Headless appsink: frames converted on demand");
        counted
    }

    /// Returns the SHM writer (none when headless) and whether decoded frames
    /// are counted.
    fn wire_appsink(
        &self,
        pipeline: &gstreamer::Pipeline,
    ) -> (Option<Arc<StdMutex<ShmRingBufferWriter>>>, bool) {
        let Some(appsink) = pipeline
            .by_name("frame_sink")
            .and_then(|el| el.downcast::<gstreamer_app::AppSink>().ok())
        else {
            return (None, false);
        };
        if self.headless {
            return (None, self.wire_headless_appsink(&appsink));
        }

        let writer = match ShmRingBufferWriter::create(&self.cam_id) {
            Ok(w) => Arc::new(StdMutex::new(w)),
            Err(e) => {
                tracing::warn!(cam_id = %self.cam_id, error = %e, "SHM create failed — frames will not be shared");
                return (None, false);
            }
        };

        let writer_clone = Arc::clone(&writer);
        self.frame_counter.store(0, AtomicOrdering::Relaxed);
        super::preview_format::install(&appsink, Arc::clone(&writer));
        let preview_gate = super::preview_rate::install(
            &appsink,
            Arc::clone(&writer),
            Arc::clone(&self.frame_counter),
            self.health.clone(),
            false,
        );
        let frame_counter_cb = Arc::clone(&self.frame_counter);
        let cam_id_cb = self.cam_id.clone();
        let health_cb = self.health.clone();
        let frame_tap = self.frame_tap.clone();
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink
                        .pull_sample()
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gstreamer::FlowError::Error)?;
                    let info = gstreamer_video::VideoInfo::from_caps(caps)
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    if !matches!(
                        info.format(),
                        gstreamer_video::VideoFormat::Rgb | gstreamer_video::VideoFormat::Nv12
                    ) {
                        return Err(gstreamer::FlowError::Error);
                    }
                    if let Some(tap) = &frame_tap {
                        tap.submit_sample(&cam_id_cb, &sample);
                    }
                    let width = info.width();
                    let height = info.height();
                    let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
                    let frame =
                        gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                            .map_err(|_| gstreamer::FlowError::Error)?;
                    let stride = usize::try_from(frame.plane_stride()[0])
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    let pixels = frame
                        .plane_data(0)
                        .map_err(|_| gstreamer::FlowError::Error)?;

                    if let Ok(mut w) = writer_clone.lock() {
                        let written = if info.format() == gstreamer_video::VideoFormat::Nv12 {
                            let uv_stride = usize::try_from(frame.plane_stride()[1])
                                .map_err(|_| gstreamer::FlowError::Error)?;
                            let uv = frame
                                .plane_data(1)
                                .map_err(|_| gstreamer::FlowError::Error)?;
                            super::preview_format::descriptor(&info).is_some_and(|color| {
                                w.write_nv12_frame(
                                    width,
                                    height,
                                    [stride, uv_stride],
                                    [pixels, uv],
                                    color,
                                )
                            })
                        } else {
                            w.write_frame(width, height, stride, pixels)
                        };
                        if let Some(h) = &health_cb {
                            // SHM writes are decode-side; rendered frames
                            // are reported by the UI reader, not here.
                            if !written {
                                h.record_decoder_drop();
                            } else if !preview_gate {
                                h.record_decoded();
                            }
                        }
                    } else if let Some(h) = &health_cb {
                        h.record_decoder_drop();
                    }

                    // Rate-limited frame debug log: every 300 frames
                    let count = if preview_gate {
                        frame_counter_cb.load(AtomicOrdering::Relaxed)
                    } else {
                        frame_counter_cb.fetch_add(1, AtomicOrdering::Relaxed) + 1
                    };
                    if count % 300 == 0 {
                        tracing::debug!(
                            cam_id = %cam_id_cb,
                            frame = count,
                            width,
                            height,
                            "Frame write rate checkpoint"
                        );
                    }

                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );

        tracing::debug!(cam_id = %self.cam_id, "Appsink wired to SHM ring buffer");
        (Some(writer), true)
    }

    fn do_stream(&mut self) {
        let Some(pipeline_ref) = self.pipeline.as_ref() else {
            self.state = State::Error {
                reason: "Pipeline missing in Streaming state".into(),
            };
            return;
        };
        let pipeline = pipeline_ref.clone();

        let bus: gstreamer::Bus = match pipeline.bus() {
            Some(b) => b,
            None => {
                self.state = State::Error {
                    reason: "Cannot get pipeline bus".into(),
                };
                return;
            }
        };

        let mut last_frames = 0;
        let mut last_progress = Instant::now();
        loop {
            if *self.stop_rx.borrow() {
                self.state = State::Stopped;
                return;
            }
            if !self.assignment_is_current() {
                self.state = State::Stopped;
                return;
            }
            if self.frames_counted {
                let frames = self.frame_counter.load(AtomicOrdering::Relaxed);
                if frames != last_frames {
                    if last_frames == 0 {
                        self.mark_streaming();
                    }
                    last_frames = frames;
                    last_progress = Instant::now();
                } else if last_progress.elapsed() >= NO_FRAME_TIMEOUT {
                    let reason = if frames == 0 {
                        "No video received from camera"
                    } else {
                        "Video stopped arriving from camera"
                    };
                    self.state = State::Error {
                        reason: reason.into(),
                    };
                    return;
                }
            }

            let Some(msg) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(500)) else {
                if !self.stream_info_complete {
                    self.try_publish_stream_info(&pipeline);
                }
                continue;
            };

            if !self.stream_info_complete {
                self.try_publish_stream_info(&pipeline);
            }

            use gstreamer::MessageView;
            match msg.view() {
                MessageView::Error(e) => {
                    let reason = super::sanitize_credentials(&format!(
                        "{}: {}",
                        e.error(),
                        e.debug().unwrap_or_default()
                    ));
                    tracing::error!(cam_id = %self.cam_id, %reason, "Pipeline error");
                    self.state = State::Error { reason };
                    return;
                }
                MessageView::Eos(_) => {
                    if builder::is_local_file_url(&self.config.url) {
                        let _ = pipeline.seek_simple(
                            gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::KEY_UNIT,
                            gstreamer::ClockTime::ZERO,
                        );
                        continue;
                    }
                    tracing::warn!(cam_id = %self.cam_id, "Stream ended (EOS)");
                    self.state = State::Error {
                        reason: "Stream ended (EOS)".into(),
                    };
                    return;
                }
                _ => {}
            }
        }
    }

    /// Return true only after GStreamer reports that the pipeline is NULL.
    fn destroy_pipeline(&mut self) -> bool {
        if let Some(pipeline) = self.pipeline.take() {
            if let Err(error) = pipeline.set_state(gstreamer::State::Null) {
                tracing::error!(cam_id = %self.cam_id, %error, "Failed to set pipeline to NULL");
                self.pipeline = Some(pipeline);
                return false;
            }
            // GStreamer state changes are asynchronous. Wait for NULL so
            // in-flight appsink callbacks finish before dropping the writer.
            let (transition, current, _) =
                pipeline.state(Some(gstreamer::ClockTime::from_seconds(5)));
            let reached_null = transition.is_ok() && current == gstreamer::State::Null;
            if !reached_null {
                tracing::error!(cam_id = %self.cam_id, "Pipeline did not reach NULL before teardown deadline");
                self.pipeline = Some(pipeline);
                return false;
            }
            tracing::debug!(cam_id = %self.cam_id, "Pipeline destroyed");
        }
        // Drop after pipeline so callback is unregistered first.
        self.shm_writer = None;
        self.frames_counted = false;
        true
    }

    /// First decoded frame of this pipeline: the camera is really online.
    fn mark_streaming(&mut self) {
        self.reconnect_attempt = 0;
        if let Some(reachability) = &self.reachability {
            reachability.answered();
        }
        self.publish_state("streaming");
        self.notify_state("streaming", None);
        tracing::info!(cam_id = %self.cam_id, state = "streaming", "Supervisor state changed");
    }

    /// The view's own state; the camera's status (and its health state) is
    /// the ingest's.
    fn publish_state(&self, state: &str) {
        self.state_tx.send(state.to_owned()).ok();
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

    fn assignment_is_current(&self) -> bool {
        self.active_assignment
            .as_ref()
            .is_some_and(|lease| lease.permits(time::OffsetDateTime::now_utc()))
    }

    fn acquire_assignment(&mut self) -> bool {
        match (&self.assignment, &self.assignment_registry) {
            (Some(assignment), Some(registry)) => {
                match registry.acquire(assignment, time::OffsetDateTime::now_utc()) {
                    Ok(lease) => {
                        self.active_assignment = Some(lease);
                        true
                    }
                    Err(error) => {
                        tracing::warn!(cam_id = %self.cam_id, %error, "Supervisor assignment was not activated");
                        false
                    }
                }
            }
            // Supervisors must be explicitly fenced before they can start.
            _ => false,
        }
    }

    fn try_publish_stream_info(&mut self, pipeline: &gstreamer::Pipeline) {
        if self.stream_info_complete {
            return;
        }
        let info = detect_stream_info(pipeline);
        if info.is_complete() {
            let (width, height) = info.resolution.unwrap_or_default();
            tracing::info!(
                cam_id = %self.cam_id,
                codec = %info.codec,
                width,
                height,
                "Stream info detected"
            );
            self.stream_info_complete = true;
        }
        self.publish_stream_info(info);
    }
}

pub(super) fn backoff_duration(attempt: u32) -> Duration {
    let secs = (1u64 << attempt.min(5)).min(MAX_BACKOFF_SECS);
    Duration::from_secs(secs)
}

/// Spread a backoff by ±20% so cameras that failed together (a switch or
/// NVR-side outage) do not all retry in the same instant.
pub(super) fn jittered(base: Duration, cam_id: &CameraId, attempt: u32) -> Duration {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(cam_id.as_bytes());
    hasher.write_u32(attempt);
    // 0..=400 per mille, recentred to -200..=+200.
    let offset = (hasher.finish() % 401) as i64 - 200;
    let millis = base.as_millis() as i64;
    Duration::from_millis((millis + millis * offset / 1000).max(0) as u64)
}

/// Published status after `failures` consecutive failed connections.
pub(super) fn failure_status(failures: u32) -> &'static str {
    if failures >= OFFLINE_AFTER_FAILURES {
        "offline"
    } else {
        "reconnecting"
    }
}

/// Network endpoint (`host`, `port`) of a camera URL, or `None` for local
/// sources (files, V4L2 devices) that have nothing to probe.
fn source_endpoint(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "rtsp" => 554,
        "rtsps" => 322,
        "http" => 80,
        "https" => 443,
        "rtmp" => 1935,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        (host, tail.strip_prefix(':'))
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => default_port,
    };
    Some((host.to_owned(), port))
}

/// Cheap reachability check before building a full pipeline. A refused or
/// timed-out TCP connect fails the attempt without touching GStreamer.
pub(super) fn probe_source(url: &str) -> Result<(), String> {
    let Some((host, port)) = source_endpoint(url) else {
        return Ok(());
    };
    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| format!("Camera address did not resolve: {error}"))?;
    let mut last_error = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(match last_error {
        Some(error) => format!("Camera unreachable: {error}"),
        None => "Camera address did not resolve".to_owned(),
    })
}

/// Next sleep while waiting for a backoff deadline, or `None` once it passed.
pub(super) fn backoff_slice(until: Instant, now: Instant) -> Option<Duration> {
    let remaining = until.saturating_duration_since(now);
    (!remaining.is_zero()).then(|| remaining.min(BACKOFF_POLL_SLICE))
}

/// Codec and decoded resolution of the live-view input.
///
/// Walks upstream from `frame_sink`, so a dedicated recording connection
/// (possibly the main stream while live view uses the sub stream) never
/// labels the live view. Only negotiated caps count: queried caps list every
/// format a pad *could* accept, and the recording muxer's first entry
/// (`video/mpeg`) used to label H.264 cameras "MPEG".
fn detect_stream_info(pipeline: &gstreamer::Pipeline) -> StreamInfo {
    let mut info = StreamInfo::default();
    let Some(sink) = pipeline.by_name("frame_sink") else {
        return info;
    };
    let mut pending = vec![sink];
    let mut visited = 0;
    while let Some(element) = pending.pop() {
        visited += 1;
        if visited > MAX_UPSTREAM_ELEMENTS {
            break;
        }
        for pad in element.sink_pads() {
            let Some(caps) = pad.current_caps() else {
                continue;
            };
            let Some(s) = caps.structure(0) else {
                continue;
            };
            if s.has_name("video/x-raw") {
                // The most upstream raw caps are the decoder output, before
                // any conversion on the way to the sink.
                if let Some(size) = resolution_from_structure(s) {
                    info.resolution = Some(size);
                }
            } else if let Some(codec) = codec_from_structure(s) {
                info.codec = codec;
                if info.resolution.is_none() {
                    info.resolution = resolution_from_structure(s);
                }
                return info;
            }
            if let Some(upstream) = pad.peer().and_then(|peer| peer.parent_element()) {
                pending.push(upstream);
            }
        }
    }
    info
}

pub(super) fn resolution_from_structure(s: &gstreamer::StructureRef) -> Option<(u32, u32)> {
    let width = u32::try_from(s.get::<i32>("width").ok()?).ok()?;
    let height = u32::try_from(s.get::<i32>("height").ok()?).ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

pub(super) fn codec_from_structure(s: &gstreamer::StructureRef) -> Option<String> {
    let name = s.name().to_ascii_lowercase();
    if let Ok(encoding_name) = s.get::<String>("encoding-name") {
        let codec = normalize_codec(&encoding_name);
        if codec != "CODEC?" {
            return Some(codec);
        }
    }

    if name.contains("x-h264") {
        return Some("H264".to_owned());
    }
    if name.contains("x-h265") || name.contains("x-hvc1") || name.contains("x-hevc") {
        return Some("H265".to_owned());
    }
    if name.contains("jpeg") || name.contains("mjpg") {
        return Some("MJPEG".to_owned());
    }
    if name.contains("vp8") {
        return Some("VP8".to_owned());
    }
    if name.contains("vp9") {
        return Some("VP9".to_owned());
    }
    if name.contains("av1") {
        return Some("AV1".to_owned());
    }
    if s.has_name("video/mpeg") {
        let codec = match s.get::<i32>("mpegversion") {
            Ok(4) => "MPEG4",
            Ok(1 | 2) => "MPEG2",
            _ => "MPEG",
        };
        return Some(codec.to_owned());
    }
    None
}

fn normalize_codec(raw: &str) -> String {
    match raw.trim().to_ascii_uppercase().as_str() {
        "H264" | "AVC" | "X-H264" => "H264".to_owned(),
        "H265" | "HEVC" | "X-H265" => "H265".to_owned(),
        "JPEG" | "MJPEG" | "JPG" | "M-JPEG" => "MJPEG".to_owned(),
        "VP8" => "VP8".to_owned(),
        "VP9" => "VP9".to_owned(),
        "AV1" => "AV1".to_owned(),
        "MPEG4" | "MP4V-ES" => "MPEG4".to_owned(),
        "MPV" => "MPEG2".to_owned(),
        _ => "CODEC?".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use nw_core::camera::CameraId;

    use super::{
        backoff_duration, backoff_slice, failure_status, jittered, probe_source, source_endpoint,
        BACKOFF_POLL_SLICE, OFFLINE_AFTER_FAILURES,
    };

    #[test]
    fn camera_goes_offline_after_repeated_failures_and_stays_there() {
        assert_eq!(failure_status(1), "reconnecting");
        assert_eq!(failure_status(OFFLINE_AFTER_FAILURES - 1), "reconnecting");
        assert_eq!(failure_status(OFFLINE_AFTER_FAILURES), "offline");
        assert_eq!(failure_status(OFFLINE_AFTER_FAILURES + 10), "offline");
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        let cam: CameraId = "cam-a".into();
        for attempt in 0..50 {
            let wait = jittered(Duration::from_secs(10), &cam, attempt);
            assert!(wait >= Duration::from_secs(8) && wait <= Duration::from_secs(12));
        }
    }

    #[test]
    fn source_endpoint_parses_camera_urls() {
        let ep = |url: &str| source_endpoint(url);
        assert_eq!(
            ep("rtsp://10.0.0.5/stream1"),
            Some(("10.0.0.5".into(), 554))
        );
        assert_eq!(
            ep("rtsp://admin:p@ss:w0rd@cam.local:8554/cam01/main"),
            Some(("cam.local".into(), 8554))
        );
        assert_eq!(
            ep("rtsps://[fe80::1]:7441/x"),
            Some(("fe80::1".into(), 7441))
        );
        assert_eq!(ep("http://cam/mjpeg?x=1"), Some(("cam".into(), 80)));
        assert_eq!(ep("rtmp://srv/live"), Some(("srv".into(), 1935)));
        assert_eq!(ep("/dev/video0"), None);
        assert_eq!(ep("/home/me/clip.mp4"), None);
        assert_eq!(ep("rtsp://host:notaport/x"), None);
    }

    #[test]
    fn probe_fails_fast_for_a_closed_port_and_passes_for_local_sources() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(probe_source(&format!("rtsp://127.0.0.1:{port}/x")).is_ok());
        drop(listener);
        assert!(probe_source(&format!("rtsp://127.0.0.1:{port}/x")).is_err());
        assert!(probe_source("/dev/video0").is_ok());
    }

    #[test]
    fn backoff_wait_is_not_cut_short_by_the_poll_slice() {
        let now = Instant::now();
        let until = now + backoff_duration(0);
        assert_eq!(backoff_slice(until, now), Some(BACKOFF_POLL_SLICE));
        assert_eq!(
            backoff_slice(until, until - Duration::from_millis(30)),
            Some(Duration::from_millis(30))
        );
        assert_eq!(backoff_slice(until, until), None);
        assert_eq!(backoff_slice(until, until + Duration::from_secs(1)), None);
    }

    #[test]
    fn backoff_escalates_to_the_cap() {
        let secs: Vec<u64> = (0..8).map(|a| backoff_duration(a).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    /// The SHM appsink callback is the point where post-decode frames are
    /// copied into the shared-memory ring. It must NOT call
    /// `record_rendered()` — that fabricates a "renderer FPS" from
    /// decode-side writes. Rendering is what the UI actually paints; that
    /// counter belongs on the reader side.
    #[test]
    fn appsink_callback_does_not_fabricate_renderer_fps() {
        let source = include_str!("supervisor.rs");
        let prod_end = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod = &source[..prod_end];
        let sink_offset = prod
            .find("appsink.set_callbacks(")
            .expect("appsink.set_callbacks block should exist in production code");
        let after = &prod[sink_offset..];
        // Rough bound: stop at the next top-level "    fn " (4-space indent)
        // which is the next method definition on the impl block.
        let block_end = after.find("\n    fn ").unwrap_or(after.len());
        let callback_block = &after[..block_end];
        assert!(
            !callback_block.contains("record_rendered"),
            "appsink callback must not call record_rendered — decode-side SHM writes are not renderer FPS"
        );
    }

    /// Verify that production teardown code does not use std::mem's forget
    /// function, which would prevent destructors from running and leak pipeline,
    /// SHM, and assignment lease objects unrecoverably.
    ///
    /// The forbidden pattern is assembled at runtime so this test's own source
    /// text does not itself contain the literal substring (which would cause the
    /// source scan to always report a false positive).
    #[test]
    fn teardown_path_does_not_orphan_owned_resources() {
        let source = include_str!("supervisor.rs");
        // Only scan production code — skip everything from the test module marker.
        let prod_end = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..prod_end];
        let forbidden: String = ["mem", "::", "forget"].concat();
        assert!(
            !prod_code.contains(forbidden.as_str()),
            "teardown must keep owned pipeline, SHM, and lease objects recoverable"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn headless_camera_still_hands_frames_to_a_tap_on_demand() {
        use gstreamer::prelude::*;
        use std::sync::Arc;

        use crate::taps::FrameTap as _;

        /// Wants one frame, then no more.
        #[derive(Default)]
        struct OneFrame(std::sync::Mutex<Option<gstreamer::Sample>>);
        impl crate::taps::FrameTap for OneFrame {
            fn wants(&self, _camera_id: &str) -> bool {
                self.0.lock().unwrap().is_none()
            }
            fn submit_sample(&self, _camera_id: &str, sample: &gstreamer::Sample) {
                self.0.lock().unwrap().get_or_insert_with(|| sample.clone());
            }
        }

        gstreamer::init().unwrap();
        let config = nw_core::camera::CameraConfig::new("Door", "videotestsrc");
        let cam_id = config.id.clone();
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (state_tx, _state_rx) = tokio::sync::watch::channel(String::new());
        let (info_tx, _info_rx) = tokio::sync::watch::channel(super::StreamInfo::default());
        let tap = Arc::new(OneFrame::default());
        let supervisor = super::PipelineSupervisor::new(
            config,
            crate::pipeline::hwaccel::HwAccelKind::Software,
            Arc::new(nw_core::traits::LogEventBus),
            stop_rx,
            state_tx,
            info_tx,
        )
        .with_frame_tap(tap.clone())
        .with_headless(true);
        let pipeline = gstreamer::parse::launch(
            "videotestsrc is-live=true ! video/x-raw,width=320,height=240,framerate=15/1 \
             ! videoconvert ! video/x-raw,format=RGB \
             ! appsink name=frame_sink max-buffers=2 drop=true sync=false emit-signals=true",
        )
        .unwrap()
        .downcast::<gstreamer::Pipeline>()
        .unwrap();

        let (writer, counted) = supervisor.wire_appsink(&pipeline);
        // No SHM ring without a desktop UI, but frames are still counted.
        assert!(writer.is_none());
        assert!(counted);
        pipeline.set_state(gstreamer::State::Playing).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while tap.wants(&cam_id) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let frame = tap.0.lock().unwrap().clone();
        let counted_frames = supervisor
            .frame_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        pipeline.set_state(gstreamer::State::Null).unwrap();

        assert!(frame.is_some_and(|sample| sample.buffer().is_some_and(|b| b.size() > 0)));
        assert!(counted_frames > 0);
    }

    #[test]
    fn stream_info_reports_negotiated_h264_not_the_recording_muxer_templates() {
        use gstreamer::prelude::*;

        gstreamer::init().unwrap();
        for factory in ["x264enc", "h264parse", "matroskamux"] {
            if gstreamer::ElementFactory::find(factory).is_none() {
                eprintln!("skipping: GStreamer element {factory} is not installed");
                return;
            }
        }
        // The second branch mirrors the dedicated recording connection: its
        // Matroska muxer's pad templates start with video/mpeg, which once
        // labelled H.264 "MPEG", and it runs at a different resolution.
        let pipeline = gstreamer::parse::launch(
            "videotestsrc num-buffers=30 ! video/x-raw,width=320,height=240 \
             ! x264enc tune=zerolatency ! h264parse ! queue ! decodebin ! videoconvert \
             ! video/x-raw,format=RGB ! appsink name=frame_sink sync=false \
             videotestsrc num-buffers=30 ! video/x-raw,width=640,height=480 \
             ! x264enc tune=zerolatency ! h264parse ! queue ! matroskamux ! fakesink sync=false",
        )
        .unwrap()
        .downcast::<gstreamer::Pipeline>()
        .unwrap();

        // Nothing is negotiated yet: unknown, never a template guess.
        assert_eq!(
            super::detect_stream_info(&pipeline),
            super::StreamInfo::default()
        );

        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let sink = pipeline
            .by_name("frame_sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        assert!(sink
            .try_pull_sample(gstreamer::ClockTime::from_seconds(10))
            .is_some());
        let info = super::detect_stream_info(&pipeline);
        pipeline.set_state(gstreamer::State::Null).unwrap();

        assert_eq!(info.codec, "H264");
        assert_eq!(info.resolution, Some((320, 240)));
    }
}
