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
pub(crate) const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(20);
/// A decoder teardown taking longer is logged: a replacement view (a stream
/// switch restarts the view in place) waits for it.
const SLOW_TEARDOWN: Duration = Duration::from_secs(1);
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
    /// Shared camera connections: an RTSP/RTMP view decodes its stream's
    /// hub instead of connecting itself (`crate::streams`).
    hubs: Option<Arc<crate::streams::StreamHubs>>,
    /// Decode any shareable source through its hub, whatever
    /// `NW_STREAM_HUB` says (tests: files).
    shared_any: bool,
    /// Kept across decoder rebuilds and backoffs: a decoder failure does not
    /// reconnect the camera.
    subscription: Option<crate::streams::Subscription>,
    feed: Option<crate::streams::feed::VideoFeed>,
    frame_ids: Arc<StdMutex<crate::streams::frames::DecoderFrames>>,
    /// The unit at which the stream's format changed: the rebuilt decoder
    /// starts with it.
    pending_unit: Option<crate::streams::SharedUnit>,
    hub_seen: u64,
    /// The hub lost the camera since the last decoded frame.
    hub_lost: bool,
    /// The recorder decodes this view's stream all the time: show those
    /// pictures instead of decoding it again (`with_borrowed_pictures`).
    borrow: bool,
    /// They stopped arriving while the camera streamed: decode it here for
    /// the rest of this view.
    borrow_failed: bool,
    /// This run shows borrowed pictures (its subscription has that role).
    borrowing: bool,
    raw_input: Option<gstreamer_app::AppSrc>,
    raw_caps: Option<gstreamer::Caps>,
    borrowed: Option<crate::streams::decoded::Receiver>,
    /// When a borrowed picture last arrived (or the run began).
    borrow_seen: Instant,
    /// When this view's hardware decoder is given up for the CPU
    /// (`decoder_health`): later runs decode on it (`hwaccel::software_only`).
    decoder_health: super::decoder_health::DecoderHealth,
    /// The decoder this run's pictures come from, named once it is plugged.
    decoder_label: Arc<crate::streams::decoded::DecoderLabel>,
    decoder_label_tried: Instant,
    /// A stall was counted against the decoder since its last picture.
    stall_counted: bool,
    /// When a unit last went into this run's decoder.
    last_pushed: Option<Instant>,
    /// The run's time origin for borrowed pictures' timestamps.
    raw_start: Instant,
}

/// No borrowed picture this long while the camera streams: the always-on
/// decoder is not delivering, so the view decodes the camera itself. Long
/// enough for a just-started decoder to reach its first keyframe.
const BORROW_TIMEOUT: Duration = Duration::from_secs(8);

/// Most bytes waiting in a shared view's `appsrc` before units are left in
/// its subscription (whose bound then skips to the next keyframe).
const VIEW_BACKLOG_BYTES: u64 = 8 * 1024 * 1024;

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
            decoder_health: super::decoder_health::DecoderHealth::default(),
            decoder_label: Arc::default(),
            decoder_label_tried: Instant::now(),
            stall_counted: false,
            last_pushed: None,
            hubs: None,
            shared_any: false,
            subscription: None,
            feed: None,
            frame_ids: Arc::default(),
            pending_unit: None,
            hub_seen: u64::MAX,
            hub_lost: false,
            borrow: false,
            borrow_failed: false,
            borrowing: false,
            raw_input: None,
            raw_caps: None,
            borrowed: None,
            borrow_seen: Instant::now(),
            raw_start: Instant::now(),
        }
    }

    /// The recorder decodes this view's stream all the time
    /// (`CameraConfig::always_decode`): show those pictures, decode nothing.
    pub fn with_borrowed_pictures(mut self, borrow: bool) -> Self {
        self.borrow = borrow;
        self
    }

    /// Decode RTSP/RTMP cameras from their shared connection (`hubs`).
    pub fn with_stream_hubs(mut self, hubs: Arc<crate::streams::StreamHubs>) -> Self {
        self.hubs = Some(hubs);
        self
    }

    /// Whether this view decodes the camera's shared connection. Tests force
    /// it (`shared_any`) whatever `NW_STREAM_HUB` says.
    fn uses_hub(&self) -> bool {
        self.hubs.is_some()
            && ((self.shared_any && crate::streams::shareable(&self.config))
                || (crate::streams::enabled() && builder::shared_view_supported(&self.config.url)))
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
                        self.subscription = None;
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
        let shared = self.uses_hub();
        // A shared connection checks the camera itself.
        if !shared {
            if let Err(reason) = probe_source(&self.config.url) {
                self.state = State::Error { reason };
                return;
            }
        }
        let borrowing = shared && self.borrow && !self.borrow_failed;
        if self.subscription.is_some() && self.borrowing != borrowing {
            // Its role says whether it decodes: subscribe again.
            self.subscription = None;
        }
        self.borrowing = borrowing;
        if shared && self.subscription.is_none() {
            let Some(hubs) = &self.hubs else {
                return;
            };
            let policy = if borrowing {
                crate::streams::SubscriberPolicy::shared_view()
            } else {
                crate::streams::SubscriberPolicy::desktop_view()
            };
            match hubs.subscribe(&self.config, policy, self.reachability.clone()) {
                Ok(subscription) => {
                    self.subscription = Some(subscription);
                    self.hub_seen = u64::MAX;
                }
                Err(reason) => {
                    self.state = State::Error { reason };
                    return;
                }
            }
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
        let pipeline_str = if borrowing {
            builder::borrowed_view_pipeline_str(self.hwaccel)
        } else if shared {
            builder::shared_view_pipeline_str(self.hwaccel)
        } else {
            builder::select_pipeline(&self.config, self.hwaccel)
        };
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

        self.decoder_health.run_started();
        self.decoder_label = Arc::default();
        self.stall_counted = false;
        self.last_pushed = None;
        if shared && !borrowing && self.decoder_health.software() {
            if let Some(decodebin) = pipeline.by_name(builder::VIEW_DECODE) {
                super::hwaccel::software_only(&decodebin);
            }
        }
        self.frame_ids = Arc::default();
        // Wire appsink → SHM ring buffer before going to PLAYING
        let (shm_writer, frames_counted) = self.wire_appsink(&pipeline);
        if shared {
            let Some(input) = pipeline
                .by_name(builder::VIEW_INPUT)
                .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
            else {
                self.state = State::Error {
                    reason: "the view pipeline has no input".into(),
                };
                return;
            };
            if borrowing {
                tracing::info!(cam_id = %self.cam_id, "Desktop view shows the always-on decoder's pictures");
                self.borrowed = self
                    .subscription
                    .as_ref()
                    .map(|subscription| subscription.decoded().subscribe());
                self.raw_input = Some(input);
                self.raw_caps = None;
                self.raw_start = Instant::now();
                self.borrow_seen = Instant::now();
            } else {
                self.feed = Some(
                    crate::streams::feed::VideoFeed::new(input)
                        .with_frames(Arc::clone(&self.frame_ids)),
                );
            }
        }

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
    fn wire_headless_appsink(
        &self,
        appsink: &gstreamer_app::AppSink,
        decoded: Option<crate::streams::decoded::Publisher>,
    ) -> bool {
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
                    if let Some(decoded) = &decoded {
                        decoded.publish(&sample);
                    }
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
        // Borrowed pictures are not offered again: they are the plugins' own source.
        let decoded = self
            .subscription
            .as_ref()
            .filter(|_| !self.borrowing)
            .map(|subscription| {
                crate::streams::decoded::Publisher::with_frames(
                    subscription.decoded(),
                    pipeline,
                    Arc::clone(&self.frame_ids),
                )
                .with_decoder(Arc::clone(&self.decoder_label))
            });
        if self.headless {
            return (None, self.wire_headless_appsink(&appsink, decoded));
        }

        let writer = match ShmRingBufferWriter::create(&self.cam_id) {
            Ok(w) => Arc::new(StdMutex::new(w)),
            Err(e) => {
                tracing::warn!(cam_id = %self.cam_id, error = %e, "SHM create failed — frames will not be shared");
                return (None, false);
            }
        };

        let writer_clone = Arc::clone(&writer);
        let frame_ids = Arc::clone(&self.frame_ids);
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
                    if let Some(decoded) = &decoded {
                        decoded.publish(&sample);
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
                        let identity = buffer
                            .pts()
                            .and_then(|pts| frame_ids.try_lock().ok()?.get(pts.nseconds()));
                        w.set_identity(identity);
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
            let waiting_for_camera = self.follow_hub();
            self.name_decoder(&pipeline);
            if self.frames_counted {
                let frames = self.frame_counter.load(AtomicOrdering::Relaxed);
                if frames != last_frames {
                    self.stall_counted = false;
                    if frames > 0 {
                        self.decoder_health.picture();
                    }
                    // Frames decoded from what arrived before a loss do not
                    // mean the camera is back; the shared connection streaming
                    // again does.
                    let back = self.hub_lost && !waiting_for_camera;
                    if last_frames == 0 || back {
                        self.hub_lost = false;
                        self.mark_streaming();
                    }
                    last_frames = frames;
                    last_progress = Instant::now();
                } else if waiting_for_camera {
                    // The shared connection is reconnecting: no decoder fault.
                    last_progress = Instant::now();
                } else {
                    let idle = last_progress.elapsed();
                    // Units keep arriving from a streaming camera and no
                    // picture comes out: the decoder's fault, not the camera's
                    // (before the run's first picture, only once the run
                    // would be given up anyway).
                    if idle >= super::decoder_health::STALL
                        && !self.stall_counted
                        && (frames > 0 || idle >= NO_FRAME_TIMEOUT)
                    {
                        self.stall_counted = true;
                        if self.units_flowing()
                            && self.blame_decoder(&pipeline, None)
                                == super::decoder_health::Verdict::Software
                        {
                            self.state = State::Error {
                                reason: "The hardware decoder stalled; decoding on the CPU".into(),
                            };
                            return;
                        }
                    }
                    if idle >= NO_FRAME_TIMEOUT {
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
            }

            if self.feed.is_some() || self.raw_input.is_some() {
                let next = if self.raw_input.is_some() {
                    self.pump_borrowed()
                } else {
                    self.pump_shared()
                };
                if let Some(next) = next {
                    self.state = next;
                    return;
                }
                while let Some(msg) = bus.pop() {
                    if let Some(next) = self.on_message(&msg, &pipeline) {
                        self.state = next;
                        return;
                    }
                }
                if !self.stream_info_complete {
                    self.try_publish_stream_info(&pipeline);
                }
                continue;
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
            if let Some(next) = self.on_message(&msg, &pipeline) {
                self.state = next;
                return;
            }
        }
    }

    /// A bus message of the running pipeline: `Some` state to move to.
    fn on_message(
        &mut self,
        msg: &gstreamer::Message,
        pipeline: &gstreamer::Pipeline,
    ) -> Option<State> {
        {
            use gstreamer::MessageView;
            match msg.view() {
                MessageView::Error(e) => {
                    let reason = super::sanitize_credentials(&format!(
                        "{}: {}",
                        e.error(),
                        e.debug().unwrap_or_default()
                    ));
                    tracing::error!(cam_id = %self.cam_id, %reason, "Pipeline error");
                    self.blame_decoder(pipeline, Some(msg));
                    return Some(State::Error { reason });
                }
                MessageView::Eos(_) => {
                    if self.feed.is_none() && builder::is_local_file_url(&self.config.url) {
                        let _ = pipeline.seek_simple(
                            gstreamer::SeekFlags::FLUSH | gstreamer::SeekFlags::KEY_UNIT,
                            gstreamer::ClockTime::ZERO,
                        );
                        return None;
                    }
                    tracing::warn!(cam_id = %self.cam_id, "Stream ended (EOS)");
                    return Some(State::Error {
                        reason: "Stream ended (EOS)".into(),
                    });
                }
                _ => {}
            }
        }
        None
    }

    /// A run of a shared view failed (`msg`: the pipeline's error) or its
    /// decoder stalled (`None`): count it against a hardware decoder when it
    /// is the decoder's doing, and say whether to decode on the CPU from now on
    /// (`decoder_health`). Before the run's first picture any pipeline error
    /// counts (a stream the hardware cannot take, e.g. above a Raspberry Pi
    /// 4's 1080p); after pictures only an error raised by the decoder itself
    /// or a stall does. Nothing counts while the camera connection is down.
    fn blame_decoder(
        &mut self,
        pipeline: &gstreamer::Pipeline,
        msg: Option<&gstreamer::Message>,
    ) -> super::decoder_health::Verdict {
        use super::decoder_health::{describe, raised_by, Verdict};
        if self.feed.is_none() || self.decoder_health.software() {
            return Verdict::Ignore;
        }
        if self.frame_counter.load(AtomicOrdering::Relaxed) > 0 {
            self.decoder_health.picture();
        }
        if self.subscription.as_ref().is_some_and(|subscription| {
            subscription.state().phase != crate::streams::Phase::Streaming
        }) {
            return Verdict::Ignore;
        }
        let Some(decodebin) = pipeline
            .by_name(builder::VIEW_DECODE)
            .and_then(|element| element.downcast::<gstreamer::Bin>().ok())
        else {
            return Verdict::Ignore;
        };
        let Some(decoder) = super::hwaccel::hardware_decoder_element_in(&decodebin) else {
            return Verdict::Ignore;
        };
        let caused =
            msg.is_none_or(|msg| raised_by(msg, &decoder)) || !self.decoder_health.pictures_seen();
        let verdict = self.decoder_health.failed(Instant::now(), caused);
        let name = decoder.factory().map(|f| f.name().to_string());
        let decoder_name = name.as_deref().unwrap_or("unknown");
        match verdict {
            Verdict::Software => {
                tracing::warn!(cam_id = %self.cam_id, decoder = decoder_name, reason = describe(msg.is_none()), "The hardware decoder failed on this stream; decoding it on the CPU for the rest of this view");
            }
            Verdict::Retry => {
                tracing::warn!(cam_id = %self.cam_id, decoder = decoder_name, reason = describe(msg.is_none()), "The hardware decoder failed on this stream; trying it again (a second failure within 10 minutes moves to the CPU)");
            }
            Verdict::Ignore => {}
        }
        verdict
    }

    /// Units went into the decoder in the last moments.
    fn units_flowing(&self) -> bool {
        self.last_pushed
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
    }

    /// Name the decoder this run plugged, for whoever receives its pictures
    /// (`DecoderLabel`): looked up here, never on the picture path, and only
    /// until it is found.
    fn name_decoder(&mut self, pipeline: &gstreamer::Pipeline) {
        if self.feed.is_none()
            || self.decoder_label.get().is_some()
            || self.decoder_label_tried.elapsed() < Duration::from_millis(250)
        {
            return;
        }
        self.decoder_label_tried = Instant::now();
        let label = pipeline
            .by_name(builder::VIEW_DECODE)
            .and_then(|element| element.downcast::<gstreamer::Bin>().ok())
            .and_then(|decodebin| super::hwaccel::decoder_label_in(&decodebin));
        if let Some(label) = label {
            self.decoder_label.set(label);
        }
    }

    /// Mirror the shared connection's state into the view's; whether the
    /// view waits for the camera (the hub is not streaming).
    fn follow_hub(&mut self) -> bool {
        let Some(subscription) = &self.subscription else {
            return false;
        };
        let state = subscription.state();
        if state.generation != self.hub_seen {
            self.hub_seen = state.generation;
            if state.phase == crate::streams::Phase::Retrying {
                self.hub_lost = true;
                self.reconnect_attempt = state.failures;
                let status = failure_status(state.failures);
                let reason = state.reason.unwrap_or_default();
                tracing::warn!(cam_id = %self.cam_id, attempt = state.failures, reason = %reason, "Camera connection lost; the shared connection reconnects");
                self.publish_state(status);
                self.notify_state(status, Some(&reason));
            }
        }
        state.phase != crate::streams::Phase::Streaming
    }

    /// Move units from the subscription into the decoder; `Some` state to
    /// move to.
    fn pump_shared(&mut self) -> Option<State> {
        use crate::streams::feed::Pushed;
        use crate::streams::{Delivery, Phase};
        let feed = self.feed.as_mut()?;
        if feed.backlog_bytes() > VIEW_BACKLOG_BYTES {
            // The decoder is behind: its subscription's bound decides what
            // is lost (it skips to the next keyframe), never the camera.
            std::thread::sleep(Duration::from_millis(20));
            return None;
        }
        let item = match self.pending_unit.take() {
            Some(unit) => Some(Delivery::Unit {
                unit,
                after_gap: false,
            }),
            None => self.subscription.as_ref()?.recv(Duration::from_millis(100)),
        };
        match item {
            Some(Delivery::Unit { unit, after_gap }) => match feed.push(&unit, after_gap, false) {
                Ok(Pushed::FormatChanged) => {
                    tracing::info!(cam_id = %self.cam_id, "Camera stream format changed; rebuilding the decoder");
                    self.pending_unit = Some(unit);
                    if !self.destroy_pipeline() {
                        return Some(State::Stopped);
                    }
                    Some(State::Reconnecting {
                        attempt: self.reconnect_attempt,
                    })
                }
                Ok(Pushed::Pushed) => {
                    self.last_pushed = Some(Instant::now());
                    None
                }
                Ok(_) | Err(gstreamer::FlowError::Flushing) => None,
                Err(error) => Some(State::Error {
                    reason: format!("the decoder did not take the camera's video: {error:?}"),
                }),
            },
            Some(Delivery::Discontinuity { .. }) => {
                feed.restart();
                None
            }
            None if self
                .subscription
                .as_ref()
                .is_some_and(|s| s.state().phase == Phase::Closed) =>
            {
                self.subscription = None;
                if !self.destroy_pipeline() {
                    return Some(State::Stopped);
                }
                Some(State::Reconnecting {
                    attempt: self.reconnect_attempt,
                })
            }
            None => None,
        }
    }

    /// Move the always-on decoder's latest picture into the view; `Some`
    /// state to move to. The subscription's compressed units are discarded:
    /// it is kept for the connection's state only.
    fn pump_borrowed(&mut self) -> Option<State> {
        use crate::streams::Phase;
        let subscription = self.subscription.as_ref()?;
        let mut closed = false;
        if subscription.recv(Duration::from_millis(10)).is_none()
            && subscription.state().phase == Phase::Closed
        {
            closed = true;
        }
        while subscription.recv(Duration::ZERO).is_some() {}
        let streaming = subscription.state().phase == Phase::Streaming;
        if closed {
            self.subscription = None;
            if !self.destroy_pipeline() {
                return Some(State::Stopped);
            }
            return Some(State::Reconnecting {
                attempt: self.reconnect_attempt,
            });
        }
        let Some((frame, _)) = self.borrowed.as_mut().and_then(|receiver| receiver.take()) else {
            if !streaming {
                // The camera is reconnecting: no fault of the decoder.
                self.borrow_seen = Instant::now();
            } else if self.borrow_seen.elapsed() >= BORROW_TIMEOUT {
                tracing::warn!(cam_id = %self.cam_id, "The always-on decoder's pictures stopped; this view decodes the camera itself");
                self.borrow_failed = true;
                if !self.destroy_pipeline() {
                    return Some(State::Stopped);
                }
                return Some(State::Reconnecting {
                    attempt: self.reconnect_attempt,
                });
            }
            return None;
        };
        self.borrow_seen = Instant::now();
        let input = self.raw_input.as_ref()?;
        let caps = frame.sample.caps()?.to_owned();
        if self.raw_caps.as_ref() != Some(&caps) {
            input.set_caps(Some(&caps));
            self.raw_caps = Some(caps);
        }
        let buffer = frame.sample.buffer()?;
        let pts = u64::try_from(self.raw_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        // A shallow copy: new timestamps, the same picture memory.
        let mut out = buffer.copy();
        if let Some(writable) = out.get_mut() {
            writable.set_pts(gstreamer::ClockTime::from_nseconds(pts));
            writable.set_dts(gstreamer::ClockTime::NONE);
            writable.set_duration(gstreamer::ClockTime::NONE);
        }
        if let (Some(identity), Ok(mut ids)) = (frame.identity, self.frame_ids.lock()) {
            ids.put(pts, identity);
        }
        match input.push_buffer(out) {
            Ok(_) | Err(gstreamer::FlowError::Flushing) => None,
            Err(error) => Some(State::Error {
                reason: format!("the view did not take the decoded pictures: {error:?}"),
            }),
        }
    }

    /// Return true only after GStreamer reports that the pipeline is NULL.
    fn destroy_pipeline(&mut self) -> bool {
        if let Some(pipeline) = self.pipeline.take() {
            let started = Instant::now();
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
            let elapsed = started.elapsed();
            if elapsed >= SLOW_TEARDOWN {
                // A replacement view (stream switch) waits for this.
                tracing::warn!(cam_id = %self.cam_id, teardown_ms = elapsed.as_millis() as u64, "Decoder teardown was slow");
            } else {
                tracing::debug!(cam_id = %self.cam_id, teardown_ms = elapsed.as_millis() as u64, "Pipeline destroyed");
            }
        }
        // Drop after pipeline so callback is unregistered first.
        self.feed = None;
        self.raw_input = None;
        self.raw_caps = None;
        self.borrowed = None;
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

pub(crate) fn backoff_duration(attempt: u32) -> Duration {
    let secs = (1u64 << attempt.min(5)).min(MAX_BACKOFF_SECS);
    Duration::from_secs(secs)
}

/// Spread a backoff by ±20% so cameras that failed together (a switch or
/// NVR-side outage) do not all retry in the same instant.
pub(crate) fn jittered(base: Duration, cam_id: &CameraId, attempt: u32) -> Duration {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(cam_id.as_bytes());
    hasher.write_u32(attempt);
    // 0..=400 per mille, recentred to -200..=+200.
    let offset = (hasher.finish() % 401) as i64 - 200;
    let millis = base.as_millis() as i64;
    Duration::from_millis((millis + millis * offset / 1000).max(0) as u64)
}

/// Status of a camera that refused the username or password (RTSP 401):
/// retrying cannot help until they change. Shown as "Wrong password", not as
/// an offline or reconnecting camera.
pub(crate) const UNAUTHORIZED: &str = "unauthorized";

/// The wait after a refused username or password. Cameras lock a client out
/// after a few wrong logins (Hikvision for 30 minutes), and every attempt
/// builds a connection: retrying every half minute kept the account locked
/// and the CPU busy. Saving new credentials reconnects at once.
pub(crate) const REFUSED_RETRY: Duration = Duration::from_secs(300);

/// Whether a connection failed because the camera refused the credentials.
pub(crate) fn refused_credentials(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("unauthorized") || reason.contains("not authorized")
}

/// The wait before the next connection attempt after a failure for `reason`.
pub(crate) fn retry_delay(attempt: u32, reason: &str, cam_id: &CameraId) -> Duration {
    let base = if refused_credentials(reason) {
        REFUSED_RETRY
    } else {
        backoff_duration(attempt)
    };
    jittered(base, cam_id, attempt)
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
pub(crate) fn probe_source(url: &str) -> Result<(), String> {
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
pub(crate) fn backoff_slice(until: Instant, now: Instant) -> Option<Duration> {
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
pub(crate) mod tests {
    use std::time::{Duration, Instant};

    use nw_core::camera::CameraId;

    use super::{
        backoff_duration, backoff_slice, failure_status, jittered, probe_source,
        refused_credentials, retry_delay, source_endpoint, BACKOFF_POLL_SLICE,
        OFFLINE_AFTER_FAILURES, REFUSED_RETRY,
    };

    #[test]
    fn a_refused_password_waits_minutes_before_asking_again() {
        let cam = CameraId::from("cam-door".to_owned());
        assert!(refused_credentials(
            "Unauthorized: gstrtspsrc.c(7071): Unauthorized (401)"
        ));
        assert!(refused_credentials("Not Authorized"));
        assert!(!refused_credentials("Could not read from resource."));
        let refused = retry_delay(0, "Unauthorized (401)", &cam);
        assert!(refused >= REFUSED_RETRY * 4 / 5 && refused <= REFUSED_RETRY * 6 / 5);
        // Other failures keep the quick backoff.
        assert!(retry_delay(0, "Could not read from resource.", &cam) <= Duration::from_secs(2));
    }

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

    impl super::PipelineSupervisor {
        /// Decode any shareable source (a file) through its hub, even with
        /// `NW_STREAM_HUB=0`.
        pub(crate) fn with_shared_any(mut self) -> Self {
            self.shared_any = true;
            self
        }
    }

    /// A local assignment fence for a supervisor run in a test.
    pub(crate) fn fenced(
        camera: &nw_core::camera::CameraConfig,
    ) -> (
        nw_core::control_plane::CameraAssignment,
        std::sync::Arc<dyn crate::assignment::AssignmentRegistry>,
    ) {
        use crate::assignment::AssignmentRegistry as _;
        let now = time::OffsetDateTime::now_utc();
        let at = |t: time::OffsetDateTime| {
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let assignment = nw_core::control_plane::CameraAssignment {
            protocol_version: nw_core::control_plane::CONTROL_PROTOCOL_VERSION,
            assignment_id: format!("test-{}", camera.id),
            camera_id: camera.id.clone(),
            node_id: "local-standalone".to_owned(),
            lease: nw_core::control_plane::Lease {
                token: format!("test-{}", camera.id),
                issued_at_utc: at(now),
                expires_at_utc: at(now + time::Duration::hours(1)),
                duration_seconds: 3600,
            },
            fencing_generation: 1,
            assigned_at_utc: at(now),
        };
        let registry = std::sync::Arc::new(crate::assignment::LocalAssignmentRegistry::new());
        registry.install(assignment.clone()).unwrap();
        (assignment, registry)
    }

    /// A desktop view of a stream the recorder decodes all the time shows
    /// those pictures with no decoder of its own, and decodes again when they
    /// stop coming.
    #[test]
    fn a_view_of_an_always_decoded_stream_borrows_its_pictures_then_falls_back() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        use crate::streams::queue::ConsumerKind;
        use crate::streams::StreamHubs;

        #[derive(Default)]
        struct Count(AtomicU64);
        impl crate::taps::FrameTap for Count {
            fn wants(&self, _camera_id: &str) -> bool {
                true
            }
            fn submit_sample(&self, _camera_id: &str, _sample: &gstreamer::Sample) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let fixture = crate::streams::testing::CameraFile::new(1000);
        let camera = fixture.camera.clone();
        let hubs = Arc::new(StreamHubs::default());
        let decoder = crate::streams::always::AlwaysDecoder::start(
            (*hubs).clone(),
            camera.clone(),
            "main".into(),
        )
        .unwrap();
        let (assignment, registry) = fenced(&camera);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (state_tx, state_rx) = tokio::sync::watch::channel(String::new());
        let (info_tx, _info_rx) = tokio::sync::watch::channel(super::StreamInfo::default());
        let frames = Arc::new(Count::default());
        let health = Arc::new(crate::stats::health::HealthCounters::default());
        let supervisor = super::PipelineSupervisor::new(
            camera.clone(),
            crate::pipeline::hwaccel::HwAccelKind::Software,
            Arc::new(nw_core::traits::LogEventBus),
            stop_rx,
            state_tx,
            info_tx,
        )
        .with_assignment(assignment, registry)
        .with_headless(true)
        .with_frame_tap(frames.clone())
        .with_stream_hubs(Arc::clone(&hubs))
        .with_shared_any()
        .with_health_counters(Arc::clone(&health))
        .with_borrowed_pictures(true);
        let view = std::thread::spawn(move || supervisor.run());
        let row = || hubs.diagnostics(std::slice::from_ref(&camera)).remove(0);
        let wait = |what: &str, done: &dyn Fn() -> bool, seconds: u64| {
            let deadline = Instant::now() + Duration::from_secs(seconds);
            while !done() {
                assert!(Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        wait(
            "borrowed pictures on screen",
            &|| state_rx.borrow().as_str() == "streaming" && frames.0.load(Ordering::Relaxed) > 5,
            20,
        );
        assert!(
            health.decoded_frames.load(Ordering::Relaxed) > 0,
            "pictures shown count as the view's frames"
        );
        let borrowed = row();
        assert!(borrowed.always_decoded, "{borrowed:?}");
        assert_eq!(borrowed.shared_views, 1, "{borrowed:?}");
        assert!(
            !borrowed.roles.contains_key(&ConsumerKind::Desktop),
            "the view runs no decoder of its own: {borrowed:?}"
        );

        // The always-on decoder stops: the view decodes the camera itself.
        drop(decoder);
        wait(
            "the view decoding again",
            &|| row().roles.get(&ConsumerKind::Desktop) == Some(&1),
            30,
        );
        let before = frames.0.load(Ordering::Relaxed);
        wait(
            "pictures from its own decoder",
            &|| frames.0.load(Ordering::Relaxed) > before + 5,
            20,
        );
        assert_eq!(row().shared_views, 0);
        stop_tx.send(true).unwrap();
        view.join().unwrap();
    }

    /// Shared-streams stage 3 gate: a desktop view decodes the camera's
    /// shared connection (here held open by a recorder) instead of opening
    /// its own, reports the stream, and starting or stopping it never
    /// interrupts the recorder's units.
    #[test]
    fn a_desktop_view_decodes_the_shared_connection_without_one_of_its_own() {
        use gstreamer::prelude::*;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        use crate::streams::{Delivery, StreamHubs, SubscriberPolicy};

        /// Wants every frame, and counts them.
        #[derive(Default)]
        struct Count(AtomicU64);
        impl crate::taps::FrameTap for Count {
            fn wants(&self, _camera_id: &str) -> bool {
                true
            }
            fn submit_sample(&self, _camera_id: &str, _sample: &gstreamer::Sample) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        gstreamer::init().unwrap();
        let dir =
            std::env::temp_dir().join(format!("nw-hub-view-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("camera.mkv");
        // 6 s of H.264 with B-frames, a keyframe every second.
        let make = crate::pipeline::builder::build(&format!(
            "videotestsrc num-buffers=150 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 \
             ! x264enc key-int-max=25 bframes=2 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        make.set_state(gstreamer::State::Playing).unwrap();
        make.bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(30),
                &[gstreamer::MessageType::Eos],
            )
            .unwrap();
        make.set_state(gstreamer::State::Null).unwrap();

        let hubs = Arc::new(StreamHubs::default());
        let camera = nw_core::camera::CameraConfig::new("Door", path.to_str().unwrap());
        let recorder = hubs
            .subscribe(&camera, SubscriberPolicy::recorder(), None)
            .unwrap();
        let (assignment, registry) = fenced(&camera);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (state_tx, state_rx) = tokio::sync::watch::channel(String::new());
        let (info_tx, info_rx) = tokio::sync::watch::channel(super::StreamInfo::default());
        let frames = Arc::new(Count::default());
        let supervisor = super::PipelineSupervisor::new(
            camera,
            crate::pipeline::hwaccel::HwAccelKind::Software,
            Arc::new(nw_core::traits::LogEventBus),
            stop_rx,
            state_tx,
            info_tx,
        )
        .with_assignment(assignment, registry)
        .with_headless(true)
        .with_frame_tap(frames.clone())
        .with_stream_hubs(Arc::clone(&hubs))
        .with_shared_any();
        // Let the recorder get going first: the view joins mid-stream.
        std::thread::sleep(Duration::from_millis(700));
        let view = std::thread::spawn(move || supervisor.run());

        let deadline = Instant::now() + Duration::from_secs(10);
        while state_rx.borrow().as_str() != "streaming" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state_rx.borrow().as_str(), "streaming");
        assert_eq!(hubs.open_count(), 1, "one camera connection");
        assert_eq!(hubs.summary()[0].1, 2, "the recorder and the view share it");
        std::thread::sleep(Duration::from_millis(1500));
        let info = info_rx.borrow().clone();
        assert_eq!(info.codec, "H264");
        assert_eq!(info.resolution, Some((320, 240)));
        stop_tx.send(true).unwrap();
        view.join().unwrap();
        let decoded = frames.0.load(Ordering::Relaxed);
        assert!(decoded >= 25, "the view decoded only {decoded} frames");
        assert_eq!(state_rx.borrow().as_str(), "offline");

        // The recorder saw the whole file once, from its first keyframe,
        // without a gap or a discontinuity while the view came and went.
        let mut units = 0;
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            match recorder.recv(Duration::from_millis(200)) {
                Some(Delivery::Unit { after_gap, .. }) => {
                    assert!(!after_gap, "the recorder lost units");
                    units += 1;
                }
                Some(Delivery::Discontinuity { .. }) => break, // the file ended
                None => {}
            }
        }
        assert_eq!(units, 150, "every unit of the file reached the recorder");
        assert_eq!(recorder.dropped(), 0);
        drop(recorder);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// When the shared connection loses the camera and reconnects (here a
    /// 2 s file ends and is opened again), the view says `reconnecting`,
    /// keeps its decoder, and goes on decoding the new stream: `streaming`.
    #[test]
    fn a_desktop_view_follows_its_shared_connection_through_a_reconnect() {
        use gstreamer::prelude::*;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        #[derive(Default)]
        struct Count(AtomicU64);
        impl crate::taps::FrameTap for Count {
            fn wants(&self, _camera_id: &str) -> bool {
                true
            }
            fn submit_sample(&self, _camera_id: &str, _sample: &gstreamer::Sample) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        gstreamer::init().unwrap();
        let path = std::env::temp_dir().join(format!(
            "nw-hub-view-loss-{}.mkv",
            uuid::Uuid::new_v4().simple()
        ));
        let make = crate::pipeline::builder::build(&format!(
            "videotestsrc num-buffers=50 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 \
             ! x264enc key-int-max=25 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        make.set_state(gstreamer::State::Playing).unwrap();
        make.bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(30),
                &[gstreamer::MessageType::Eos],
            )
            .unwrap();
        make.set_state(gstreamer::State::Null).unwrap();

        let hubs = Arc::new(crate::streams::StreamHubs::default());
        let camera = nw_core::camera::CameraConfig::new("Door", path.to_str().unwrap());
        let (assignment, registry) = fenced(&camera);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(String::new());
        let (info_tx, _info_rx) = tokio::sync::watch::channel(super::StreamInfo::default());
        let frames = Arc::new(Count::default());
        let supervisor = super::PipelineSupervisor::new(
            camera,
            crate::pipeline::hwaccel::HwAccelKind::Software,
            Arc::new(nw_core::traits::LogEventBus),
            stop_rx,
            state_tx,
            info_tx,
        )
        .with_assignment(assignment, registry)
        .with_headless(true)
        .with_frame_tap(frames.clone())
        .with_stream_hubs(Arc::clone(&hubs))
        .with_shared_any();
        let view = std::thread::spawn(move || supervisor.run());
        let mut states = Vec::new();
        let mut frames_at_reconnect = None;
        let deadline = Instant::now() + Duration::from_secs(9);
        while Instant::now() < deadline {
            if state_rx.has_changed().unwrap_or(false) {
                let state = state_rx.borrow_and_update().clone();
                if state == "reconnecting" && frames_at_reconnect.is_none() {
                    frames_at_reconnect = Some(frames.0.load(Ordering::Relaxed));
                }
                if states.last() != Some(&state) {
                    states.push(state);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop_tx.send(true).unwrap();
        view.join().unwrap();
        let _ = std::fs::remove_file(path);
        let before = frames_at_reconnect.expect("the view said reconnecting");
        let after = frames.0.load(Ordering::Relaxed);
        assert!(
            states
                .windows(2)
                .any(|w| w == ["reconnecting", "streaming"]),
            "back to streaming after the reconnect: {states:?}"
        );
        assert!(
            after > before + 20,
            "decoding went on: {before} then {after}"
        );
    }

    /// Against a real RTSP camera (`NW_RTSP_TEST_URL`): the desktop view and
    /// a recorder share one camera connection.
    #[test]
    #[ignore = "requires an RTSP H.264 stream at NW_RTSP_TEST_URL"]
    fn a_desktop_view_shares_a_real_cameras_connection() {
        use std::sync::Arc;
        gstreamer::init().unwrap();
        let url = std::env::var("NW_RTSP_TEST_URL").expect("NW_RTSP_TEST_URL");
        let hubs = Arc::new(crate::streams::StreamHubs::default());
        let camera = nw_core::camera::CameraConfig::new("Cam", &url);
        let recorder = hubs
            .subscribe(&camera, crate::streams::SubscriberPolicy::recorder(), None)
            .unwrap();
        let (assignment, registry) = fenced(&camera);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (state_tx, state_rx) = tokio::sync::watch::channel(String::new());
        let (info_tx, info_rx) = tokio::sync::watch::channel(super::StreamInfo::default());
        let supervisor = super::PipelineSupervisor::new(
            camera,
            crate::pipeline::hwaccel::HwAccelKind::Software,
            Arc::new(nw_core::traits::LogEventBus),
            stop_rx,
            state_tx,
            info_tx,
        )
        .with_assignment(assignment, registry)
        .with_headless(true)
        .with_stream_hubs(Arc::clone(&hubs));
        let view = std::thread::spawn(move || supervisor.run());
        let deadline = Instant::now() + Duration::from_secs(15);
        while state_rx.borrow().as_str() != "streaming" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state_rx.borrow().as_str(), "streaming");
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(hubs.open_count(), 1, "one camera connection");
        assert_eq!(hubs.summary()[0].1, 2);
        assert_eq!(info_rx.borrow().codec, "H264");
        assert!(info_rx.borrow().resolution.is_some());
        stop_tx.send(true).unwrap();
        view.join().unwrap();
        assert_eq!(recorder.dropped(), 0);
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
