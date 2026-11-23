//! Recorder-side transcoding of an H.265 camera stream for a browser that
//! cannot play H.265 (browser live view, `cloud::live_publish`).
//!
//! This is the one place browser live view decodes video, and only while a
//! session asked for it (`PublishDirective::transcode`) and its granted
//! stream is H.265. An H.264 stream is forwarded unchanged, as ever.
//!
//! **One transcode per camera stream, shared.** A [`Registry`] (owned by
//! [`StreamHubs`]) keeps one [`Transcode`] per resolved source. Every session
//! that asked joins it with a [`TranscodeSink`] and gets a [`TranscodeViewer`]
//! handle; the transcode reads the stream's shared connection with a
//! subscription of its own (video only, role `browser`, so diagnostics counts
//! it there and the camera is not connected to twice), and stops when the last
//! viewer's handle is dropped.
//!
//! Two outputs, decided per transcode and kept for its life:
//!
//! * **Encoder** (preferred): `appsrc ! decodebin ! videoconvert ! NV12 !
//!   <encoder> ! h264parse ! appsink`. The decoder follows the normal
//!   hardware policy (`decodebin` ranking, [`DecoderHealth`] and
//!   `hwaccel::software_only` for a decoder given up). The encoders are
//!   tried in [`ENCODERS`] order and probed at runtime ([`next_trial`]): one
//!   that fails to build or negotiate, errors, or gives no output within
//!   [`TRIAL`] is passed over for this transcode's life. Never `x264enc`
//!   (GPL) nor `openh264enc` (its patent licence covers only Cisco's binary).
//!   Output is Annex-B H.264 access units, parameter sets before each
//!   keyframe, no B-frames, a keyframe every [`KEYFRAME_EVERY`]: they look
//!   like a camera's H.264 stream to the publication path, which keeps its
//!   GOP resend, catch-up stamping and `EncodedSubmitter`. A GOP cache here
//!   gives a viewer that joins a running transcode its first picture at once.
//! * **CPU** (no GStreamer H.264 encoder works): the same decode into I420
//!   pictures ([`Picture`]) handed to each sink; the session publishes a raw
//!   track that libwebrtc encodes as VP8 in software (the VP8 encode is per
//!   session, the decode shared).
//!
//! Lock order: registry map, then a transcode's state. The pipeline's appsink
//! callbacks only take the state lock and call sinks, which never block.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_video::prelude::VideoFrameExt;
use nw_core::camera::CameraConfig;
use nw_core::encoded::{EncodedAccessUnit, VideoCodec};
use serde::Serialize;

use super::queue::Unit;
use super::{
    always, decoded, feed::VideoFeed, frames, lock, Delivery, Media, Phase, SourceKey, StreamHubs,
    SubscriberPolicy,
};
use crate::pipeline::decoder_health::{DecoderHealth, Verdict};
use crate::pipeline::hwaccel;

/// A trial encoder that produces nothing for this long after its first unit
/// went in is given up.
pub const TRIAL: Duration = Duration::from_secs(6);
/// The encoder is asked for a keyframe this often (its own interval is a
/// backstop: [`BACKSTOP_GOP`] pictures).
pub const KEYFRAME_EVERY: Duration = Duration::from_secs(2);
const BACKSTOP_GOP: u32 = 120;
/// No picture out of a running CPU decode for this long: rebuild it.
const CPU_IDLE: Duration = Duration::from_secs(20);
/// Waiting for the first picture of a stream before trying again.
const FIRST_UNIT_WAIT: Duration = Duration::from_secs(30);
/// Compressed bytes waiting in the decoder's input beyond which units are
/// skipped to the next keyframe (a decoder that cannot keep up).
const BACKLOG_BYTES: u64 = 24 * 1024 * 1024;
/// Waits before trying a failed run again: 2, 5, 10, then 30 s.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// Most units / bytes the joining viewers' GOP cache keeps.
const GOP_MAX_UNITS: usize = 1024;
const GOP_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Failures of an encoder that did produce before it is given up.
const FAILURES_BEFORE_NEXT_ENCODER: usize = 3;
/// Bits per pixel a picture is encoded at (for the target below the grant's cap).
const BITS_PER_PIXEL: f64 = 0.08;
/// Never targeted below this (kbit/s).
const MIN_TARGET_KBPS: u32 = 400;

// ---------------------------------------------------------------------------
// Encoders
// ---------------------------------------------------------------------------

/// One GStreamer H.264 encoder and how a person names the API it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encoder {
    pub factory: &'static str,
    pub label: &'static str,
}

/// The encoders tried, in order: VA-API, NVENC, Quick Sync, Media
/// Foundation, V4L2. None of them is a CPU encoder.
pub const ENCODERS: [Encoder; 6] = [
    Encoder {
        factory: "vah264enc",
        label: "VA-API",
    },
    Encoder {
        factory: "vah264lpenc",
        label: "VA-API",
    },
    Encoder {
        factory: "nvh264enc",
        label: "NVENC",
    },
    Encoder {
        factory: "qsvh264enc",
        label: "Quick Sync",
    },
    Encoder {
        factory: "mfh264enc",
        label: "Media Foundation",
    },
    Encoder {
        factory: "v4l2h264enc",
        label: "V4L2",
    },
];

/// What `encoder` in the CPU case is called (the VP8 fallback).
pub const CPU: &str = "CPU";

/// An encoder and whether its output is held to a WebRTC-friendly profile
/// (constrained baseline, baseline or main). A strict trial that fails to
/// negotiate is followed by a relaxed one before the encoder is given up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trial {
    pub encoder: Encoder,
    pub strict: bool,
}

/// The next encoder to try: the first installed one (in [`ENCODERS`] order,
/// strict before relaxed) not in `failed`; `None` once none is left, which
/// means the CPU fallback. `installed` says whether a factory exists.
pub fn next_trial(installed: impl Fn(&str) -> bool, failed: &[Trial]) -> Option<Trial> {
    ENCODERS
        .iter()
        .filter(|encoder| installed(encoder.factory))
        .flat_map(|encoder| {
            [true, false].map(|strict| Trial {
                encoder: *encoder,
                strict,
            })
        })
        .find(|trial| !failed.contains(trial))
}

/// The encoder a transcode would use now, as the inventory reports it:
/// the first installed encoder's label, else `"CPU"`.
pub fn encoder_label(installed: impl Fn(&str) -> bool) -> &'static str {
    next_trial(installed, &[]).map_or(CPU, |trial| trial.encoder.label)
}

/// [`encoder_label`] for this computer's GStreamer registry.
pub fn available_encoder_label() -> &'static str {
    if gstreamer::init().is_err() {
        return CPU;
    }
    encoder_label(|name| gstreamer::ElementFactory::find(name).is_some())
}

fn exclude(failed: &mut Vec<Trial>, trial: Trial, both: bool) {
    for strict in [true, false] {
        if (both || strict == trial.strict) && !failed.contains(&Trial { strict, ..trial }) {
            failed.push(Trial { strict, ..trial });
        }
    }
}

/// Bit rate (kbit/s) one transcode targets: what the picture needs at a
/// typical frame rate, never above the grants' cap.
pub fn target_kbps(width: u32, height: u32, cap_kbps: u32) -> u32 {
    let wanted = (f64::from(width) * f64::from(height) * 30.0 * BITS_PER_PIXEL / 1000.0) as u32;
    let wanted = wanted.max(MIN_TARGET_KBPS);
    if cap_kbps == 0 {
        wanted
    } else {
        wanted.min(cap_kbps)
    }
}

// ---------------------------------------------------------------------------
// Sinks and pictures
// ---------------------------------------------------------------------------

/// A decoded picture, I420, planes packed (strides are the widths).
#[derive(Debug)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// Arrival time of the camera's unit, µs since the Unix epoch.
    pub timestamp_us: i64,
    pub frame: Option<nw_core::frame::FrameIdentity>,
}

/// Where a session receives a transcode's output. Every method returns at
/// once; a sink that is behind drops what it cannot take.
pub trait TranscodeSink: Send + Sync {
    /// An H.264 access unit (encoder output).
    fn unit(&self, unit: &EncodedAccessUnit);
    /// A decoded picture (CPU fallback; the newest wins).
    fn picture(&self, picture: &Arc<Picture>);
    /// The stream restarted: resume at the next keyframe.
    fn resync(&self);
    /// This stream cannot be transcoded.
    fn fail(&self);
}

/// What the status and diagnostics report about one running transcode.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TranscodeInfo {
    pub camera_id: String,
    pub camera_name: String,
    /// `main` or `sub`.
    pub stream: &'static str,
    pub from: &'static str,
    /// `h264` (an encoder) or `vp8` (the CPU fallback).
    pub to: &'static str,
    pub decoder: &'static str,
    pub encoder: &'static str,
    pub viewers: u32,
}

/// A stream row's `transcode` object (diagnostics).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamTranscode {
    pub from: &'static str,
    pub to: &'static str,
    pub decoder: &'static str,
    pub encoder: &'static str,
    pub viewers: u32,
}

impl From<&TranscodeInfo> for StreamTranscode {
    fn from(info: &TranscodeInfo) -> Self {
        Self {
            from: info.from,
            to: info.to,
            decoder: info.decoder,
            encoder: info.encoder,
            viewers: info.viewers,
        }
    }
}

#[derive(Default)]
struct GopCache {
    units: Vec<EncodedAccessUnit>,
    bytes: usize,
    overflowed: bool,
}

impl GopCache {
    fn push(&mut self, unit: &EncodedAccessUnit) {
        if unit.keyframe {
            self.units.clear();
            self.bytes = 0;
            self.overflowed = false;
        }
        if self.overflowed || self.units.is_empty() && !unit.keyframe {
            return;
        }
        if self.units.len() >= GOP_MAX_UNITS || self.bytes + unit.payload.len() > GOP_MAX_BYTES {
            self.units.clear();
            self.bytes = 0;
            self.overflowed = true;
            return;
        }
        self.bytes += unit.payload.len();
        self.units.push(unit.clone());
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

struct Viewer {
    id: u64,
    cap_kbps: u32,
    sink: Arc<dyn TranscodeSink>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    viewers: Vec<Viewer>,
    gop: GopCache,
    /// `h264` / `vp8`, set while a run is up.
    to: Option<&'static str>,
    decoder: Option<&'static str>,
    encoder: Option<&'static str>,
}

/// One camera stream's transcode, shared by its viewers.
struct Transcode {
    camera_id: String,
    camera_name: String,
    stream: &'static str,
    state: Mutex<State>,
    stop: AtomicBool,
    /// The lowest cap (kbit/s) among the viewers, 0 when none states one.
    cap_kbps: AtomicU32,
}

impl Transcode {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn refresh_cap(&self, state: &State) {
        let cap = state
            .viewers
            .iter()
            .map(|viewer| viewer.cap_kbps)
            .filter(|cap| *cap > 0)
            .min()
            .unwrap_or(0);
        self.cap_kbps.store(cap, Ordering::Relaxed);
    }

    fn info(&self) -> Option<TranscodeInfo> {
        let state = lock(&self.state);
        Some(TranscodeInfo {
            camera_id: self.camera_id.clone(),
            camera_name: self.camera_name.clone(),
            stream: self.stream,
            from: "h265",
            to: state.to?,
            decoder: state.decoder?,
            encoder: state.encoder?,
            viewers: u32::try_from(state.viewers.len()).unwrap_or(u32::MAX),
        })
    }

    fn set_running(&self, to: Option<&'static str>, encoder: Option<&'static str>) {
        let mut state = lock(&self.state);
        state.to = to;
        state.encoder = encoder;
        state.decoder = None;
        state.gop.clear();
    }

    fn set_decoder(&self, decoder: &'static str) {
        lock(&self.state).decoder = Some(decoder);
    }

    fn publish_unit(&self, unit: &EncodedAccessUnit) {
        let mut state = lock(&self.state);
        state.gop.push(unit);
        for viewer in &state.viewers {
            viewer.sink.unit(unit);
        }
    }

    fn publish_picture(&self, picture: &Arc<Picture>) {
        let state = lock(&self.state);
        for viewer in &state.viewers {
            viewer.sink.picture(picture);
        }
    }

    fn resync(&self) {
        let mut state = lock(&self.state);
        state.gop.clear();
        for viewer in &state.viewers {
            viewer.sink.resync();
        }
    }

    fn fail_viewers(&self) {
        let state = lock(&self.state);
        for viewer in &state.viewers {
            viewer.sink.fail();
        }
    }
}

/// The running transcodes, one per camera connection (see the module docs).
#[derive(Default)]
pub struct Registry {
    map: Mutex<HashMap<SourceKey, Arc<Transcode>>>,
}

impl Registry {
    /// Every running transcode that has its decoder and encoder named.
    pub fn list(&self) -> Vec<TranscodeInfo> {
        let map = lock(&self.map);
        let mut all: Vec<_> = map.values().filter_map(|t| t.info()).collect();
        all.sort_by(|a, b| (&a.camera_id, a.stream).cmp(&(&b.camera_id, b.stream)));
        all
    }

    pub(super) fn row(&self, key: &SourceKey) -> Option<StreamTranscode> {
        let transcode = lock(&self.map).get(key).cloned()?;
        transcode.info().as_ref().map(StreamTranscode::from)
    }

    pub(super) fn shutdown(&self) {
        for transcode in lock(&self.map).values() {
            transcode.stop.store(true, Ordering::Release);
        }
    }

    /// Removes `transcode` from the map if it is still the one registered.
    fn remove(&self, key: &SourceKey, transcode: &Arc<Transcode>) {
        let mut map = lock(&self.map);
        if map.get(key).is_some_and(|t| Arc::ptr_eq(t, transcode)) {
            map.remove(key);
        }
    }

    fn leave(&self, key: &SourceKey, transcode: &Arc<Transcode>, id: u64) {
        let mut map = lock(&self.map);
        let mut state = lock(&transcode.state);
        state.viewers.retain(|viewer| viewer.id != id);
        transcode.refresh_cap(&state);
        if state.viewers.is_empty() {
            transcode.stop.store(true, Ordering::Release);
            drop(state);
            if map.get(key).is_some_and(|t| Arc::ptr_eq(t, transcode)) {
                map.remove(key);
            }
        }
    }
}

/// A session's membership of a transcode; dropping it leaves, and the
/// transcode stops with its last viewer.
pub struct TranscodeViewer {
    registry: Arc<Registry>,
    key: SourceKey,
    transcode: Arc<Transcode>,
    id: u64,
}

impl Drop for TranscodeViewer {
    fn drop(&mut self) {
        self.registry.leave(&self.key, &self.transcode, self.id);
    }
}

impl StreamHubs {
    /// Join (starting it if none runs) the transcode of `camera`'s stream
    /// (its `url`; `stream` is `main` or `sub`), sending `sink` its output.
    /// `cap_kbps` is the session's granted bit rate; the encoder targets the
    /// lowest of its viewers'. Returns at once.
    pub fn transcode(
        &self,
        camera: &CameraConfig,
        stream: &'static str,
        cap_kbps: u32,
        sink: Arc<dyn TranscodeSink>,
    ) -> Result<TranscodeViewer, String> {
        let key = SourceKey::of(camera);
        let mut map = lock(&self.transcodes.map);
        if self.shutdown.load(Ordering::SeqCst) {
            return Err("shutting down".into());
        }
        let transcode = match map.get(&key) {
            Some(existing) if !existing.stopped() => Arc::clone(existing),
            _ => {
                let transcode = Arc::new(Transcode {
                    camera_id: camera.id.clone(),
                    camera_name: camera.name.clone(),
                    stream,
                    state: Mutex::default(),
                    stop: AtomicBool::new(false),
                    cap_kbps: AtomicU32::new(0),
                });
                let context = Context {
                    hubs: self.clone(),
                    registry: Arc::clone(&self.transcodes),
                    key: key.clone(),
                    transcode: Arc::clone(&transcode),
                    camera: camera.clone(),
                };
                std::thread::Builder::new()
                    .name(format!("transcode-{}", camera.id))
                    .spawn(move || run(context))
                    .map_err(|error| format!("could not start the transcode: {error}"))?;
                map.insert(key.clone(), Arc::clone(&transcode));
                transcode
            }
        };
        let id = {
            let mut state = lock(&transcode.state);
            let id = state.next_id;
            state.next_id += 1;
            // A joining viewer starts from the cached GOP, keyframe first.
            for unit in &state.gop.units {
                let mut cached = unit.clone();
                cached.frame = None;
                sink.unit(&cached);
            }
            state.viewers.push(Viewer { id, cap_kbps, sink });
            transcode.refresh_cap(&state);
            id
        };
        Ok(TranscodeViewer {
            registry: Arc::clone(&self.transcodes),
            key,
            transcode,
            id,
        })
    }

    /// The running transcodes (the desktop's status).
    pub fn transcodes(&self) -> Vec<TranscodeInfo> {
        self.transcodes.list()
    }
}

// ---------------------------------------------------------------------------
// The transcode's thread
// ---------------------------------------------------------------------------

struct Context {
    hubs: StreamHubs,
    registry: Arc<Registry>,
    key: SourceKey,
    transcode: Arc<Transcode>,
    camera: CameraConfig,
}

#[derive(Clone, Copy)]
enum Mode {
    Encoder(Trial),
    Cpu,
}

struct Failure {
    reason: String,
    /// The run delivered output before failing.
    produced: bool,
    /// It was the trial's own limit, not an error.
    timed_out: bool,
}

impl Failure {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            produced: false,
            timed_out: false,
        }
    }
}

enum End {
    Stopped,
    Failed(Failure),
}

fn run(context: Context) {
    if gstreamer::init().is_err() {
        context.transcode.fail_viewers();
        context.registry.remove(&context.key, &context.transcode);
        return;
    }
    let installed = |name: &str| gstreamer::ElementFactory::find(name).is_some();
    let mut tried: Vec<Trial> = Vec::new();
    // An encoder that produced: kept, with its failures counted.
    let mut keep: Option<(Trial, usize)> = None;
    let mut produced_h264 = false;
    let mut health = DecoderHealth::default();
    let mut failures = 0usize;
    let mut given_up = false;
    while !context.transcode.stopped() {
        let started = Instant::now();
        let mode = match keep {
            Some((trial, _)) => Mode::Encoder(trial),
            None => match next_trial(installed, &tried) {
                Some(trial) => Mode::Encoder(trial),
                // Viewers already hold H.264 tracks: they cannot move to VP8.
                None if produced_h264 => {
                    given_up = true;
                    break;
                }
                None => Mode::Cpu,
            },
        };
        match attempt(&context, mode, &mut health) {
            End::Stopped => break,
            End::Failed(failure) => {
                let cam_id = &context.transcode.camera_id;
                match mode {
                    Mode::Encoder(trial) => {
                        tracing::warn!(cam_id = %cam_id, encoder = trial.encoder.factory, strict = trial.strict, reason = %failure.reason, produced = failure.produced, "Transcode encoder stopped");
                        if failure.produced {
                            produced_h264 = true;
                            let count = keep.map_or(0, |(_, count)| count) + 1;
                            if count >= FAILURES_BEFORE_NEXT_ENCODER {
                                exclude(&mut tried, trial, true);
                                keep = None;
                            } else {
                                keep = Some((trial, count));
                            }
                        } else {
                            exclude(&mut tried, trial, failure.timed_out);
                            keep = None;
                        }
                    }
                    Mode::Cpu => {
                        tracing::warn!(cam_id = %cam_id, reason = %failure.reason, "Transcode decoder stopped; retrying");
                    }
                }
                let wait = if failure.produced || matches!(mode, Mode::Cpu) {
                    if started.elapsed() > Duration::from_secs(60) {
                        failures = 0;
                    }
                    let wait = BACKOFF[failures.min(BACKOFF.len() - 1)];
                    failures += 1;
                    wait
                } else {
                    Duration::from_millis(200)
                };
                if !sleep_unless_stopped(&context.transcode, wait) {
                    break;
                }
            }
        }
    }
    if given_up {
        tracing::warn!(cam_id = %context.transcode.camera_id, "No H.264 encoder works any more; the transcode ends");
        context.transcode.fail_viewers();
    }
    context.transcode.stop.store(true, Ordering::Release);
    context.transcode.set_running(None, None);
    context.registry.remove(&context.key, &context.transcode);
    tracing::info!(cam_id = %context.transcode.camera_id, stream = context.transcode.stream, "Transcode stopped");
}

/// Sleeps `wait` unless stopped first; whether to go on.
fn sleep_unless_stopped(transcode: &Transcode, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if transcode.stopped() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !transcode.stopped()
}

/// Stops the pipeline however the run ends.
struct Running(gstreamer::Pipeline);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.set_state(gstreamer::State::Null);
    }
}

/// The newest decoded picture, handed from the appsink callback (which must
/// return at once) to the thread that copies it out.
#[derive(Default)]
struct SampleSlot {
    sample: Mutex<Option<gstreamer::Sample>>,
    ready: Condvar,
    closed: AtomicBool,
}

impl SampleSlot {
    fn put(&self, sample: gstreamer::Sample) {
        *lock(&self.sample) = Some(sample);
        self.ready.notify_one();
    }

    fn take(&self) -> Option<gstreamer::Sample> {
        let mut slot = lock(&self.sample);
        loop {
            if let Some(sample) = slot.take() {
                return Some(sample);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            slot = match self.ready.wait_timeout(slot, Duration::from_millis(200)) {
                Ok((slot, _)) => slot,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_all();
    }
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as i64)
}

fn identity_of(
    frames: &Mutex<frames::DecoderFrames>,
    pts: Option<gstreamer::ClockTime>,
) -> Option<frames::Identity> {
    let pts = pts?.nseconds();
    frames.lock().ok()?.get(pts)
}

/// One transcode run: `End::Stopped` when asked to stop, else why it failed.
fn attempt(context: &Context, mode: Mode, health: &mut DecoderHealth) -> End {
    let transcode = &*context.transcode;
    let policy = SubscriberPolicy {
        media: Some(Media::Video),
        ..SubscriberPolicy::live_video()
    };
    let subscription = match context.hubs.subscribe(&context.camera, policy, None) {
        Ok(subscription) => subscription,
        Err(reason) => return End::Failed(Failure::new(reason)),
    };
    // The first picture sizes the encoder.
    let waiting = Instant::now();
    let (first, first_gap) = loop {
        if transcode.stopped() {
            return End::Stopped;
        }
        match subscription.recv(Duration::from_millis(100)) {
            Some(Delivery::Unit { unit, after_gap }) if unit.media == Media::Video => {
                break (unit, after_gap)
            }
            Some(_) => {}
            None if subscription.state().phase == Phase::Closed => {
                return End::Failed(Failure::new("the camera connection closed"));
            }
            None => {}
        }
        if waiting.elapsed() > FIRST_UNIT_WAIT {
            return End::Failed(Failure::new("the camera sent no picture"));
        }
    };
    let (width, height) = first.video_size().unwrap_or((0, 0));

    let pipeline = match build(mode) {
        Ok(pipeline) => pipeline,
        Err(reason) => return End::Failed(Failure::new(reason)),
    };
    let running = Running(pipeline.clone());
    let setup = || -> Result<_, String> {
        let input = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
            .ok_or("the transcoder has no input")?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or("the transcoder has no output")?;
        let decodebin = pipeline
            .by_name("decode")
            .ok_or("the transcoder has no decodebin")?;
        Ok((input, sink, decodebin))
    };
    let (input, sink, decodebin) = match setup() {
        Ok(parts) => parts,
        Err(reason) => return End::Failed(Failure::new(reason)),
    };
    health.run_started();
    if health.software() {
        hwaccel::software_only(&decodebin);
    }
    let Ok(decodebin) = decodebin.downcast::<gstreamer::Bin>() else {
        return End::Failed(Failure::new("decodebin is not a bin"));
    };

    let frame_ids: Arc<Mutex<frames::DecoderFrames>> = Arc::default();
    let produced = Arc::new(AtomicU64::new(0));
    let out_bytes = Arc::new(AtomicU64::new(0));
    let decoded_pictures = Arc::new(AtomicU64::new(0));
    let label: Arc<decoded::DecoderLabel> = Arc::default();
    let slot = Arc::new(SampleSlot::default());
    let mut picture_thread = None;
    let mut encoder_element = None;
    let mut rate = Rate::None;
    let mut applied_cap = transcode.cap_kbps.load(Ordering::Relaxed);

    match mode {
        Mode::Encoder(trial) => {
            let Some(encoder) = pipeline.by_name("enc") else {
                return End::Failed(Failure::new("the transcoder has no encoder"));
            };
            let target = target_kbps(width, height, applied_cap);
            rate = configure(&encoder, target);
            // What the decoder delivered: told apart from what the encoder did.
            if let Some(pad) = encoder.static_pad("sink") {
                let counted = Arc::clone(&decoded_pictures);
                pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                    counted.fetch_add(1, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                });
            }
            encoder_element = Some(encoder);
            transcode.set_running(Some("h264"), Some(trial.encoder.label));
            let (target_transcode, ids, count, bytes) = (
                Arc::clone(&context.transcode),
                Arc::clone(&frame_ids),
                Arc::clone(&produced),
                Arc::clone(&out_bytes),
            );
            sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let Ok(sample) = sink.pull_sample() else {
                            return Err(gstreamer::FlowError::Eos);
                        };
                        if let Some(unit) = access_unit(&sample, &ids) {
                            bytes.fetch_add(unit.payload.len() as u64, Ordering::Relaxed);
                            count.fetch_add(1, Ordering::Relaxed);
                            target_transcode.publish_unit(&unit);
                        }
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        Mode::Cpu => {
            transcode.set_running(Some("vp8"), Some(CPU));
            let counted = Arc::clone(&decoded_pictures);
            let slot_in = Arc::clone(&slot);
            sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let Ok(sample) = sink.pull_sample() else {
                            return Err(gstreamer::FlowError::Eos);
                        };
                        counted.fetch_add(1, Ordering::Relaxed);
                        slot_in.put(sample);
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
            let (slot_out, ids, count, target_transcode) = (
                Arc::clone(&slot),
                Arc::clone(&frame_ids),
                Arc::clone(&produced),
                Arc::clone(&context.transcode),
            );
            picture_thread = std::thread::Builder::new()
                .name(format!("transcode-pictures-{}", transcode.camera_id))
                .spawn(move || {
                    while let Some(sample) = slot_out.take() {
                        if let Some(picture) = picture(&sample, &ids) {
                            count.fetch_add(1, Ordering::Relaxed);
                            target_transcode.publish_picture(&Arc::new(picture));
                        }
                    }
                })
                .ok();
        }
    }
    let _picture_guard = PictureThread(Arc::clone(&slot), picture_thread);

    let mut feed = VideoFeed::new(input).with_frames(Arc::clone(&frame_ids));
    let Some(bus) = pipeline.bus() else {
        return End::Failed(Failure::new("the transcoder has no bus"));
    };
    if let Err(error) = pipeline.set_state(gstreamer::State::Playing) {
        return End::Failed(Failure::new(error.to_string()));
    }
    tracing::info!(cam_id = %transcode.camera_id, stream = transcode.stream, "Transcode running");
    let failure = |reason: String, timed_out: bool| {
        End::Failed(Failure {
            reason,
            produced: produced.load(Ordering::Relaxed) > 0,
            timed_out,
        })
    };

    let started = Instant::now();
    let mut watch = always::StallWatch::new();
    let mut last_key = Instant::now();
    let mut last_rate = Instant::now();
    let mut last_bytes = 0u64;
    let mut skipping = false;
    let mut pending = Some((first, first_gap));
    while !transcode.stopped() {
        watch.name_decoder(&decodebin, &label);
        if let Some(name) = label.get() {
            transcode.set_decoder(name);
        }
        if let Some(message) =
            bus.pop_filtered(&[gstreamer::MessageType::Error, gstreamer::MessageType::Eos])
        {
            let reason = match message.view() {
                gstreamer::MessageView::Error(error) => error.error().to_string(),
                _ => "the transcoder stopped".into(),
            };
            if matches!(message.view(), gstreamer::MessageView::Error(_)) {
                always::blame(
                    health,
                    &decodebin,
                    &subscription,
                    decoded_pictures.load(Ordering::Relaxed),
                    Some(&message),
                    &transcode.camera_id,
                    "transcoder",
                );
            }
            return failure(reason, false);
        }
        let pictures = decoded_pictures.load(Ordering::Relaxed);
        if watch.stalled(pictures, &subscription)
            && always::blame(
                health,
                &decodebin,
                &subscription,
                pictures,
                None,
                &transcode.camera_id,
                "transcoder",
            ) == Verdict::Software
        {
            return failure("the hardware decoder stalled".into(), false);
        }
        // A trial that shows nothing is passed over.
        let idle = started.elapsed();
        if produced.load(Ordering::Relaxed) == 0 {
            match mode {
                Mode::Encoder(_) if idle > TRIAL => {
                    return failure("the encoder produced nothing".into(), true)
                }
                Mode::Cpu if idle > CPU_IDLE => {
                    return failure("the decoder produced nothing".into(), true)
                }
                _ => {}
            }
        }
        // Keyframes: on a timer, and at once after a restart.
        if let Some(encoder) = &encoder_element {
            if last_key.elapsed() >= KEYFRAME_EVERY {
                last_key = Instant::now();
                let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
                    .all_headers(true)
                    .build();
                let _ = encoder.send_event(event);
            }
        }
        // Bit rate: follow the lowest grant; hold the target of an encoder
        // that only does constant quantiser.
        if last_rate.elapsed() >= Duration::from_secs(1) {
            let seconds = last_rate.elapsed().as_secs_f64().max(0.001);
            last_rate = Instant::now();
            let bytes = out_bytes.load(Ordering::Relaxed);
            let kbps = ((bytes - last_bytes) as f64 * 8.0 / 1000.0 / seconds) as u32;
            last_bytes = bytes;
            let cap = transcode.cap_kbps.load(Ordering::Relaxed);
            if let Some(encoder) = &encoder_element {
                let target = target_kbps(width, height, cap);
                if cap != applied_cap {
                    applied_cap = cap;
                    set_property(encoder, "bitrate", &target.to_string());
                }
                if let Rate::Qp(qp) = &mut rate {
                    qp.adjust(encoder, kbps, target);
                }
            }
        }
        // Units: from the first, then the stream's.
        let next = match pending.take() {
            Some(unit) => Some(Delivery::Unit {
                unit: unit.0,
                after_gap: unit.1,
            }),
            None => subscription.recv(Duration::from_millis(100)),
        };
        match next {
            Some(Delivery::Unit { unit, after_gap }) if unit.media == Media::Video => {
                let keyframe = unit.keyframe();
                if feed.backlog_bytes() > BACKLOG_BYTES && !keyframe {
                    skipping = true;
                }
                if skipping && !keyframe {
                    continue;
                }
                let gap = after_gap || std::mem::take(&mut skipping);
                // A changed format goes on with the new caps: decodebin follows.
                if let Err(flow) = feed.push(&unit, gap, true) {
                    return failure(format!("the decoder refused input: {flow:?}"), false);
                }
                watch.pushed();
            }
            Some(Delivery::Unit { .. }) => {}
            Some(Delivery::Discontinuity { .. }) => {
                feed.restart();
                transcode.resync();
                last_key = Instant::now()
                    .checked_sub(KEYFRAME_EVERY)
                    .unwrap_or_else(Instant::now);
            }
            None if subscription.state().phase == Phase::Closed => {
                return failure("the camera connection closed".into(), false);
            }
            None => {}
        }
    }
    drop(running);
    End::Stopped
}

/// Stops the picture thread with the run.
struct PictureThread(Arc<SampleSlot>, Option<std::thread::JoinHandle<()>>);

impl Drop for PictureThread {
    fn drop(&mut self) {
        self.0.close();
        if let Some(thread) = self.1.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Pipelines
// ---------------------------------------------------------------------------

const HEAD: &str =
    "appsrc name=src format=time is-live=true do-timestamp=false block=false max-bytes=0 ! \
                    decodebin name=decode ! videoconvert !";

/// The output profiles a browser decodes in every WebRTC build.
const FRIENDLY_PROFILES: &str = "video/x-h264,profile=(string){constrained-baseline,baseline,main}";

/// The pipeline description of `mode`.
fn description(mode: Mode) -> String {
    match mode {
        Mode::Encoder(trial) => {
            let profile = if trial.strict {
                format!(" {FRIENDLY_PROFILES} !")
            } else {
                String::new()
            };
            format!(
                "{HEAD} video/x-raw,format=NV12 ! {factory} name=enc !{profile} \
                 h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! \
                 appsink name=sink sync=false max-buffers=0 emit-signals=true",
                factory = trial.encoder.factory
            )
        }
        Mode::Cpu => format!(
            "{HEAD} video/x-raw,format=I420 ! \
             appsink name=sink sync=false max-buffers=1 drop=true emit-signals=true"
        ),
    }
}

fn build(mode: Mode) -> Result<gstreamer::Pipeline, String> {
    gstreamer::parse::launch(&description(mode))
        .map_err(|error| error.to_string())?
        .downcast::<gstreamer::Pipeline>()
        .map_err(|_| "not a pipeline".to_owned())
}

/// Sets `name` on `element` from text if it has such a writable property
/// (and the text reads as its type); whether it did.
fn set_property(element: &gstreamer::Element, name: &str, value: &str) -> bool {
    let Some(spec) = element.find_property(name) else {
        return false;
    };
    if !spec.flags().contains(glib::ParamFlags::WRITABLE) {
        return false;
    }
    match glib::Value::deserialize(value, spec.value_type()) {
        Ok(value) => {
            element.set_property_from_value(name, &value);
            true
        }
        Err(_) => false,
    }
}

/// The nicknames of an enum property's values (empty if it is not one).
fn enum_nicks(element: &gstreamer::Element, name: &str) -> Vec<String> {
    element
        .find_property(name)
        .and_then(|spec| glib::EnumClass::with_type(spec.value_type()))
        .map(|class| {
            class
                .values()
                .iter()
                .map(|value| value.nick().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// How an encoder's bit rate is held.
enum Rate {
    /// Nothing to adjust (the encoder's own constant / variable bit rate, or
    /// an encoder this does not know how to steer).
    None,
    /// Constant quantiser only (an Intel low-power encoder): the quantiser
    /// follows the measured rate.
    Qp(QpControl),
}

struct QpControl {
    qp: u32,
}

impl QpControl {
    const START: u32 = 34;
    const MIN: u32 = 20;
    const MAX: u32 = 48;

    /// Next quantiser for a measured `kbps` against `target`: coarser when
    /// over, finer when well under.
    fn next(&self, kbps: u32, target: u32) -> u32 {
        if kbps == 0 {
            self.qp
        } else if kbps > target + target / 10 {
            (self.qp + 2).min(Self::MAX)
        } else if u64::from(kbps) * 10 < u64::from(target) * 6 {
            self.qp.saturating_sub(1).max(Self::MIN)
        } else {
            self.qp
        }
    }

    fn adjust(&mut self, encoder: &gstreamer::Element, kbps: u32, target: u32) {
        let qp = self.next(kbps, target);
        if qp != self.qp {
            self.qp = qp;
            apply_qp(encoder, qp);
        }
    }
}

fn apply_qp(encoder: &gstreamer::Element, qp: u32) {
    let value = qp.to_string();
    for name in ["qpi", "qpp", "qpb"] {
        set_property(encoder, name, &value);
    }
}

/// Low latency settings, whichever of these properties `encoder` has: no
/// B-frames, constant (else variable) bit rate at `kbps`, a backstop
/// keyframe interval. Properties it does not have are left alone.
fn configure(encoder: &gstreamer::Element, kbps: u32) -> Rate {
    let factory = encoder
        .factory()
        .map(|factory| factory.name().to_string())
        .unwrap_or_default();
    for name in ["key-int-max", "gop-size"] {
        set_property(encoder, name, &BACKSTOP_GOP.to_string());
    }
    for name in ["b-frames", "bframes"] {
        set_property(encoder, name, "0");
    }
    set_property(encoder, "aud", "false");
    set_property(encoder, "zerolatency", "true");
    set_property(encoder, "low-latency", "true");
    set_property(encoder, "target-usage", "6");
    if factory.starts_with("v4l2") {
        // Constant bit rate (1), bits a second, an I frame period.
        set_property(
            encoder,
            "extra-controls",
            &format!(
                "controls,video_bitrate_mode=1,video_bitrate={},h264_i_frame_period={BACKSTOP_GOP}",
                u64::from(kbps) * 1000
            ),
        );
        return Rate::None;
    }
    let mode_property = ["rate-control", "rc-mode"]
        .into_iter()
        .find(|name| encoder.find_property(name).is_some());
    let nicks = mode_property
        .map(|name| enum_nicks(encoder, name))
        .unwrap_or_default();
    let chosen = ["cbr", "vbr"]
        .into_iter()
        .find(|wanted| nicks.iter().any(|nick| nick == wanted));
    if let (Some(name), Some(wanted)) = (mode_property, chosen) {
        set_property(encoder, name, wanted);
        set_property(encoder, "bitrate", &kbps.to_string());
        if wanted == "vbr" {
            set_property(encoder, "max-bitrate", &kbps.to_string());
        }
        return Rate::None;
    }
    // Neither constant nor variable rate is offered (an Intel low-power
    // encoder on older hardware): constant quantiser, steered by the rate.
    if let Some(name) = mode_property {
        if nicks.iter().any(|nick| nick == "cqp") {
            set_property(encoder, name, "cqp");
            apply_qp(encoder, QpControl::START);
            return Rate::Qp(QpControl {
                qp: QpControl::START,
            });
        }
    }
    set_property(encoder, "bitrate", &kbps.to_string());
    Rate::None
}

/// An encoder's output buffer as an access unit.
fn access_unit(
    sample: &gstreamer::Sample,
    frame_ids: &Mutex<frames::DecoderFrames>,
) -> Option<EncodedAccessUnit> {
    let buffer = sample.buffer()?;
    let structure = sample.caps()?.structure(0)?;
    let width = u32::try_from(structure.get::<i32>("width").ok()?).ok()?;
    let height = u32::try_from(structure.get::<i32>("height").ok()?).ok()?;
    let map = buffer.map_readable().ok()?;
    let frame = identity_of(frame_ids, buffer.pts());
    Some(EncodedAccessUnit {
        frame,
        codec: VideoCodec::H264,
        payload: map.as_slice().to_vec(),
        keyframe: !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT),
        width,
        height,
        timestamp_us: frame.map_or_else(now_us, |frame| frame.observed_us),
        pts_us: buffer
            .pts()
            .map(|pts| i64::try_from(pts.useconds()).unwrap_or(i64::MAX)),
    })
}

/// A decoded I420 sample as a [`Picture`] with packed planes.
fn picture(
    sample: &gstreamer::Sample,
    frame_ids: &Mutex<frames::DecoderFrames>,
) -> Option<Picture> {
    let buffer = sample.buffer()?;
    let info = gstreamer_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    if info.format() != gstreamer_video::VideoFormat::I420 {
        return None;
    }
    let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (width, height) = (info.width() as usize, info.height() as usize);
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
    let strides = frame.plane_stride();
    let plane = |index: u32, wide: usize, rows: usize| {
        let stride = usize::try_from(*strides.get(index as usize)?).ok()?;
        pack(frame.plane_data(index).ok()?, stride, wide, rows)
    };
    let identity = identity_of(frame_ids, buffer.pts());
    Some(Picture {
        width: info.width(),
        height: info.height(),
        y: plane(0, width, height)?,
        u: plane(1, chroma_width, chroma_height)?,
        v: plane(2, chroma_width, chroma_height)?,
        timestamp_us: identity.map_or_else(now_us, |frame| frame.observed_us),
        frame: identity,
    })
}

/// `rows` rows of `width` bytes out of a plane laid out with `stride`.
fn pack(data: &[u8], stride: usize, width: usize, rows: usize) -> Option<Vec<u8>> {
    if stride < width || width == 0 || rows == 0 {
        return None;
    }
    if stride == width {
        return data.get(..width * rows).map(<[u8]>::to_vec);
    }
    let mut out = Vec::with_capacity(width * rows);
    for row in 0..rows {
        out.extend_from_slice(data.get(row * stride..row * stride + width)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests;
