//! One bound camera: compressed units in, motion events out. Decodes the
//! camera's units itself (NeuroWatch never decodes for a plugin) into 320×180
//! GRAY8 at 5 fps, the size the detector works at.
use crate::activity::{Action, Activity};
use crate::decoding;
use crate::detector::{Detector, HEIGHT, WIDTH};
use crate::drawing;
use crate::manifest::{DEBUG, DETECTIONS};
use crate::settings::MotionSettings;
use gstreamer::prelude::*;
use gstreamer_video::prelude::*;
use nw_analytics_protocol::{
    frame::MediaUnit,
    messages::{
        BindingState, BindingStatus, Codec, EventEnd, EventStart, EventUpdate, Overlay,
        OverlayItem, Status, StreamFormat,
    },
    method,
    rpc::Message,
};
use nw_analytics_sdk::{
    pictures::{Next, Pictures},
    ring::{Picture, PictureIdentity, RawFormat, RawLayout, RingReader},
};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

/// Units waiting for the decoder; more and the oldest are lost (the worker
/// restarts from the next keyframe).
const QUEUE: usize = 64;
const STATUS_EVERY: Duration = Duration::from_secs(2);
/// How long a drawing stays without a newer one (5 fps analysis sends
/// several meanwhile; a stalled analysis leaves nothing stale).
const DRAWING_TTL_MS: u32 = 1500;
/// A decoder that failed is rebuilt after this pause, from a keyframe.
const RETRY_AFTER: Duration = Duration::from_secs(3);
/// A hardware decoder with no picture to show for itself after this long has
/// failed, whatever it says: the next one takes over.
const DECODER_TRIAL: Duration = Duration::from_secs(10);

/// Sends JSON-RPC messages to NeuroWatch (shared by a session's workers).
pub trait Sender: Send + Sync + 'static {
    fn send(&self, message: Message);
}

pub enum Input {
    Format(StreamFormat),
    Unit(MediaUnit),
    Moving(bool),
    Settings(MotionSettings),
    Decoded(RingReader),
    Withdrawn,
}

/// Options that apply to every session and worker. The decoder is each
/// camera's own setting ([`MotionSettings::decoder`]).
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// A session that hears nothing for this long is closed (NeuroWatch
    /// pings every 5 s and gives up on a plugin after 15 s itself).
    pub silence_limit: std::time::Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            silence_limit: std::time::Duration::from_secs(15),
        }
    }
}

pub struct Worker {
    tx: Option<mpsc::SyncSender<Input>>,
    lost: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub fn start(
        binding: u32,
        camera: String,
        settings: MotionSettings,
        events: Arc<AtomicU64>,
        out: Arc<dyn Sender>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let lost = Arc::new(AtomicBool::new(false));
        let thread = {
            let lost = Arc::clone(&lost);
            std::thread::Builder::new()
                .name(format!("motion-{camera}"))
                .spawn(move || {
                    let mut run = Run::new(binding, settings, events, out);
                    run.serve(&rx, &lost);
                    run.finish_activity();
                })?
        };
        Ok(Self {
            tx: Some(tx),
            lost,
            thread: Some(thread),
        })
    }

    /// Pass input on without waiting. A full queue loses the unit; the worker
    /// then restarts from the next keyframe.
    pub fn offer(&self, input: Input) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        match input {
            Input::Unit(_) => {
                if tx.try_send(input).is_err() {
                    self.lost.store(true, Ordering::Release);
                }
            }
            // Rare control input waits for room.
            other => {
                let _ = tx.send(other);
            }
        }
    }

    /// Units were lost before reaching this worker (NeuroWatch's `gap`).
    pub fn lost(&self) {
        self.lost.store(true, Ordering::Release);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channel ends the worker; it ends its open event.
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

type AnalyzedPicture = (Vec<u8>, Option<i64>, Option<u64>);

struct Decoder {
    pipeline: gstreamer::Pipeline,
    src: gstreamer_app::AppSrc,
    sink: gstreamer_app::AppSink,
    bus: gstreamer::Bus,
    name: Arc<std::sync::Mutex<Option<String>>>,
    identities: std::sync::Mutex<BTreeMap<u64, PictureIdentity>>,
}

/// Pictures analysed a second; the detector's work and its timing assume it.
const ANALYSIS_FPS: u32 = 5;
/// How far behind schedule a picture's timestamp means a new timeline.
const TIMELINE_JUMP_NS: u64 = 2_000_000_000;

/// At most `fps` pictures a second, judged by their own timestamps alone.
struct RateLimit {
    interval_ns: u64,
    next_ns: Option<u64>,
}

impl RateLimit {
    fn new(fps: u32) -> Self {
        Self {
            interval_ns: 1_000_000_000 / u64::from(fps.max(1)),
            next_ns: None,
        }
    }

    fn admit(&mut self, pts_ns: u64) -> bool {
        // Timestamps far behind what was expected: a new timeline, not an old picture.
        let new_timeline = self
            .next_ns
            .is_some_and(|next| pts_ns.saturating_add(TIMELINE_JUMP_NS) < next);
        match self.next_ns {
            Some(next) if pts_ns < next && !new_timeline => false,
            Some(next) if !new_timeline => {
                // On schedule, so 25 fps becomes a steady 5; far later (a gap)
                // starts the schedule again from this picture.
                self.next_ns = Some((next + self.interval_ns).max(pts_ns + self.interval_ns / 2));
                true
            }
            _ => {
                self.next_ns = Some(pts_ns + self.interval_ns);
                true
            }
        }
    }
}

/// Every element the decoding pipelines below name, and the software
/// decoders every camera can fall back to.
pub const REQUIRED_ELEMENTS: [&str; 8] = [
    "appsrc",
    "appsink",
    "h264parse",
    "h265parse",
    "videoscale",
    "videoconvert",
    "avdec_h264",
    "avdec_h265",
];

/// The [`REQUIRED_ELEMENTS`] this GStreamer lacks (`gstreamer::init` first).
pub fn missing_elements() -> Vec<&'static str> {
    REQUIRED_ELEMENTS
        .into_iter()
        .filter(|name| gstreamer::ElementFactory::find(name).is_none())
        .collect()
}

/// What keeps this GStreamer's VA-API from being used, if anything.
pub fn va_blocked() -> Option<String> {
    let plugin = gstreamer::Registry::get().find_plugin("va")?;
    let version = plugin.version();
    decoding::va_leaks(&version)
        .then(|| format!("GStreamer {version}'s VA-API leaks memory (fixed in 1.24.6)"))
}

/// The hardware decoders GStreamer has here, for `--check`.
pub fn hardware_decoders() -> Vec<&'static str> {
    [
        "vah264dec",
        "vah265dec",
        "nvh264dec",
        "nvh265dec",
        "qsvh264dec",
        "qsvh265dec",
        "v4l2slh264dec",
        "v4l2h264dec",
        "d3d11h264dec",
        "d3d12h264dec",
    ]
    .into_iter()
    .filter(|name| gstreamer::ElementFactory::find(name).is_some())
    .collect()
}

impl Decoder {
    /// `element`: the decoder [`decoding::pick`] chose, named outright.
    fn new(format: &StreamFormat, element: &str) -> anyhow::Result<Self> {
        let (caps, parser) = match format.codec {
            Codec::H264 => ("video/x-h264", "h264parse"),
            Codec::H265 => ("video/x-h265", "h265parse"),
        };
        let description = format!(
            "appsrc name=src is-live=true format=time \
               caps=\"{caps},stream-format=byte-stream,alignment=au\" \
             ! {parser} ! {element} name=decode \
             ! videoscale ! videoconvert \
             ! video/x-raw,format=GRAY8,width={WIDTH},height={HEIGHT} \
             ! appsink name=sink max-buffers=1 drop=true sync=false emit-signals=false"
        );
        let decoder = Self::build(&description)?;
        if let Ok(mut name) = decoder.name.lock() {
            *name = Some(element.to_owned());
        }
        Ok(decoder)
    }

    fn raw(layout: RawLayout) -> anyhow::Result<Self> {
        let name = match layout.format {
            RawFormat::Rgb24 => "RGB",
            RawFormat::Nv12 => "NV12",
            RawFormat::Gray8 => "GRAY8",
        };
        Self::build(&format!("appsrc name=src is-live=true format=time caps=\"video/x-raw,format={name},width={},height={},framerate=5/1\" ! videoscale ! videoconvert ! video/x-raw,format=GRAY8,width={WIDTH},height={HEIGHT} ! appsink name=sink max-buffers=1 drop=true sync=false emit-signals=false", layout.width, layout.height))
    }

    fn build(description: &str) -> anyhow::Result<Self> {
        let pipeline = gstreamer::parse::launch(description)?
            .downcast::<gstreamer::Pipeline>()
            .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
            .ok_or_else(|| anyhow::anyhow!("missing appsrc"))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or_else(|| anyhow::anyhow!("missing appsink"))?;
        let bus = pipeline
            .bus()
            .ok_or_else(|| anyhow::anyhow!("missing bus"))?;
        // The analysis rate, right after the decoder: before the costly scaling
        // and conversion, and a skipped hardware picture is never copied to
        // memory. Not `videorate`: joining a stream already running (large
        // timestamps) or one whose caps have no frame rate, it lets every
        // picture through.
        if let Some(decode) = pipeline.by_name("decode") {
            let limit = std::sync::Mutex::new(RateLimit::new(ANALYSIS_FPS));
            decode
                .static_pad("src")
                .ok_or_else(|| anyhow::anyhow!("the decoder has no output"))?
                .add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
                    let admitted = match info.buffer().and_then(|buffer| buffer.pts()) {
                        Some(pts) => limit
                            .lock()
                            .map_or(true, |mut limit| limit.admit(pts.nseconds())),
                        None => true,
                    };
                    if admitted {
                        gstreamer::PadProbeReturn::Ok
                    } else {
                        gstreamer::PadProbeReturn::Drop
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("could not limit the analysis rate"))?;
        }
        let name = Arc::new(std::sync::Mutex::new(None));
        pipeline.set_state(gstreamer::State::Playing)?;
        Ok(Self {
            pipeline,
            src,
            sink,
            bus,
            name,
            identities: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    fn push(&self, unit: &MediaUnit, base_us: i64) -> anyhow::Result<()> {
        let mut buffer = gstreamer::Buffer::from_slice(unit.data.clone());
        {
            let buffer = buffer
                .get_mut()
                .ok_or_else(|| anyhow::anyhow!("new buffer is shared"))?;
            // The stream's own times when it has them, else arrival times.
            let arrival = || {
                gstreamer::ClockTime::from_useconds(
                    u64::try_from(unit.time_us.saturating_sub(base_us)).unwrap_or(0),
                )
            };
            buffer.set_pts(
                unit.pts_ns
                    .map(gstreamer::ClockTime::from_nseconds)
                    .or_else(|| Some(arrival())),
            );
            buffer.set_dts(unit.dts_ns.map(gstreamer::ClockTime::from_nseconds));
            if !unit.keyframe() {
                buffer.set_flags(gstreamer::BufferFlags::DELTA_UNIT);
            }
        }
        if let Some(pts) = buffer.pts() {
            self.remember(
                pts.nseconds(),
                PictureIdentity {
                    seq: unit.seq,
                    time_us: unit.time_us,
                    pts_ns: unit.pts_ns,
                },
            );
        }
        self.src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|flow| anyhow::anyhow!("decoder refused input: {flow:?}"))
    }

    fn remember(&self, pts: u64, identity: PictureIdentity) {
        if let Ok(mut identities) = self.identities.lock() {
            identities.insert(pts, identity);
            while identities.len() > 4096 {
                identities.pop_first();
            }
        }
    }

    fn push_raw(&self, picture: Picture) -> anyhow::Result<()> {
        let format = match picture.layout.format {
            RawFormat::Rgb24 => gstreamer_video::VideoFormat::Rgb,
            RawFormat::Nv12 => gstreamer_video::VideoFormat::Nv12,
            RawFormat::Gray8 => gstreamer_video::VideoFormat::Gray8,
        };
        let info = gstreamer_video::VideoInfo::builder(
            format,
            picture.layout.width,
            picture.layout.height,
        )
        .build()?;
        let mut buffer = gstreamer::Buffer::with_size(info.size())?;
        let pts = u64::try_from(picture.identity.time_us)?.saturating_mul(1000);
        {
            let writable = buffer
                .get_mut()
                .ok_or_else(|| anyhow::anyhow!("new raw buffer shared"))?;
            writable.set_pts(gstreamer::ClockTime::from_nseconds(pts));
            let mut map = writable.map_writable()?;
            let mut start = 0;
            for (i, (width, rows)) in nw_analytics_sdk::ring::plane_sizes(picture.layout)?
                .into_iter()
                .enumerate()
            {
                if rows == 0 {
                    continue;
                }
                let stride = usize::try_from(info.stride()[i])?;
                for row in 0..rows {
                    let destination = info.offset()[i] + row * stride;
                    map[destination..destination + width]
                        .copy_from_slice(&picture.data[start..start + width]);
                    start += width;
                }
            }
        }
        self.remember(pts, picture.identity);
        self.src
            .push_buffer(buffer)
            .map_err(|flow| anyhow::anyhow!("raw input refused: {flow:?}"))?;
        Ok(())
    }

    /// The next 320×180 picture, if one is ready, with its capture time and identity.
    fn pull(&self) -> anyhow::Result<Option<AnalyzedPicture>> {
        let Some(sample) = self.sink.try_pull_sample(gstreamer::ClockTime::ZERO) else {
            return Ok(None);
        };
        let caps = sample
            .caps()
            .ok_or_else(|| anyhow::anyhow!("missing caps"))?;
        let info = gstreamer_video::VideoInfo::from_caps(caps)?;
        let buffer = sample
            .buffer()
            .ok_or_else(|| anyhow::anyhow!("missing buffer"))?;
        let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)?;
        let stride = usize::try_from(frame.plane_stride()[0])?;
        let plane = frame.plane_data(0)?;
        let mut pixels = vec![0u8; WIDTH * HEIGHT];
        for row in 0..HEIGHT {
            pixels[row * WIDTH..(row + 1) * WIDTH]
                .copy_from_slice(&plane[row * stride..row * stride + WIDTH]);
        }
        let identity = buffer
            .pts()
            .and_then(|pts| self.identities.lock().ok()?.get(&pts.nseconds()).copied());
        let time_ms = identity.map(|id| id.time_us / 1000);
        Ok(Some((pixels, time_ms, identity.map(|id| id.seq))))
    }

    fn failed(&self) -> Option<String> {
        self.bus
            .pop_filtered(&[gstreamer::MessageType::Error, gstreamer::MessageType::Eos])
            .map(|message| match message.view() {
                gstreamer::MessageView::Error(error) => error.error().to_string(),
                _ => "the decoder stopped".into(),
            })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

struct Run {
    binding: u32,
    settings: MotionSettings,
    events: Arc<AtomicU64>,
    out: Arc<dyn Sender>,
    format: Option<StreamFormat>,
    pictures: Pictures,
    raw_layout: Option<RawLayout>,
    picture_seq: Option<u64>,
    decoder: Option<Decoder>,
    /// Decoders that let this camera down, until its setting changes.
    failed_decoders: Vec<&'static str>,
    /// Why this camera is not on the decoder chosen for it.
    decoder_note: Option<String>,
    /// The hardware decoder on trial: since when, until its first picture.
    decoder_trial: Option<Instant>,
    current_decoder: Option<&'static str>,
    /// Arrival time of the first unit of this decoder's run.
    base_us: i64,
    waiting_keyframe: bool,
    retry_at: Option<Instant>,
    moving: bool,
    detector: Detector,
    activity: Activity,
    /// The open event's number.
    event: Option<u64>,
    /// Layers that show something now (cleared when that ends).
    drawn: [bool; 2],
    last_ms: Option<i64>,
    state: BindingState,
    detail: Option<String>,
    frames: u64,
    measured: u64,
    meter: Instant,
    fps: f32,
    reported: Instant,
}

impl Run {
    fn new(
        binding: u32,
        settings: MotionSettings,
        events: Arc<AtomicU64>,
        out: Arc<dyn Sender>,
    ) -> Self {
        let now = Instant::now();
        Self {
            binding,
            detector: Detector::new(&settings),
            settings,
            events,
            out,
            format: None,
            pictures: Pictures::default(),
            raw_layout: None,
            picture_seq: None,
            decoder: None,
            failed_decoders: vec![],
            decoder_note: None,
            decoder_trial: None,
            current_decoder: None,
            base_us: 0,
            waiting_keyframe: true,
            retry_at: None,
            moving: false,
            activity: Activity::default(),
            event: None,
            drawn: [false; 2],
            last_ms: None,
            state: BindingState::Waiting,
            detail: None,
            frames: 0,
            measured: 0,
            meter: now,
            fps: 0.0,
            reported: now - STATUS_EVERY,
        }
    }

    fn serve(&mut self, inputs: &mpsc::Receiver<Input>, lost: &AtomicBool) {
        loop {
            match inputs.recv_timeout(Duration::from_millis(20)) {
                Ok(input) => self.input(input, lost),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            // Take everything else already waiting before pulling pictures.
            for _ in 0..QUEUE {
                let Ok(input) = inputs.try_recv() else { break };
                self.input(input, lost);
            }
            self.source();
            self.pictures();
            self.report();
        }
    }

    fn input(&mut self, input: Input, lost: &AtomicBool) {
        match input {
            Input::Format(format) => {
                if self.format.as_ref() != Some(&format) {
                    self.pictures.gap();
                    self.format = Some(format);
                    self.restart();
                }
            }
            Input::Unit(unit) => {
                if lost.swap(false, Ordering::AcqRel) || unit.discontinuity() {
                    self.pictures.gap();
                    if !self.pictures.active() {
                        self.restart();
                    }
                }
                let unit = Arc::new(unit);
                if self.pictures.unit(Arc::clone(&unit)) {
                    self.unit(&unit);
                }
            }
            Input::Moving(moving) => {
                if moving && !self.moving {
                    self.reset_detection();
                }
                self.moving = moving;
            }
            Input::Decoded(ring) => {
                if !self.settings.low_cpu {
                    self.restart();
                    self.pictures.offer(ring);
                }
            }
            Input::Withdrawn => self.pictures.withdraw(),
            Input::Settings(settings) => {
                if settings.low_cpu {
                    self.pictures.withdraw();
                }
                let decoder_changed = settings.decoder != self.settings.decoder;
                self.settings = settings;
                if decoder_changed {
                    self.failed_decoders.clear();
                    self.decoder_note = None;
                    self.decoder_trial = None;
                    self.current_decoder = None;
                    self.retry_at = None;
                    // Desktop pictures need no decoder of this camera's own.
                    if self.raw_layout.is_none() {
                        self.restart();
                    }
                }
                self.reset_detection();
            }
        }
    }

    fn source(&mut self) {
        match self.pictures.poll() {
            Next::Picture(picture) => {
                let layout = picture.layout;
                if self.raw_layout != Some(layout) || self.decoder.is_none() {
                    self.restart();
                    match Decoder::raw(layout) {
                        Ok(decoder) => {
                            self.decoder = Some(decoder);
                            self.raw_layout = Some(layout);
                        }
                        Err(error) => {
                            self.fail(error.to_string());
                            self.pictures.withdraw();
                            return;
                        }
                    }
                }
                if let Some(Err(error)) = self
                    .decoder
                    .as_ref()
                    .map(|decoder| decoder.push_raw(picture))
                {
                    self.fail(error.to_string());
                    self.pictures.withdraw();
                }
            }
            Next::Fallback(units) => {
                self.restart();
                for unit in units {
                    self.unit(&unit);
                }
            }
            Next::Waiting => {}
        }
    }

    /// A new decoder from the next keyframe, and a fresh detector: what
    /// follows may be a different moment or picture.
    fn restart(&mut self) {
        self.decoder = None;
        // A decoder on trial starts its trial again with the next one.
        self.decoder_trial = None;
        self.current_decoder = None;
        self.raw_layout = None;
        self.picture_seq = None;
        self.waiting_keyframe = true;
        self.reset_detection();
    }

    fn reset_detection(&mut self) {
        self.finish_activity();
        self.draw(DETECTIONS, vec![]);
        self.draw(DEBUG, vec![]);
        self.detector = Detector::new(&self.settings);
        self.last_ms = None;
    }

    fn unit(&mut self, unit: &MediaUnit) {
        if self.waiting_keyframe && !unit.keyframe() {
            return;
        }
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if self.decoder.is_none() {
            let Some(format) = self.format.as_ref() else {
                return;
            };
            let blocked = va_blocked();
            let Some(pick) = decoding::pick(
                self.settings.decoder,
                format.codec,
                &self.failed_decoders,
                blocked.as_deref(),
                &|name| gstreamer::ElementFactory::find(name).is_some(),
            ) else {
                self.fail("No decoder for this video is installed".to_owned());
                return;
            };
            self.decoder_note = pick.note;
            match Decoder::new(format, pick.element) {
                Ok(decoder) => {
                    tracing::info!(binding = self.binding, decoder = pick.element, "Decoding");
                    self.decoder = Some(decoder);
                    self.current_decoder = Some(pick.element);
                    self.decoder_trial = pick.hardware.then(Instant::now);
                    self.base_us = unit.time_us;
                    self.retry_at = None;
                }
                Err(error) if pick.hardware => {
                    self.current_decoder = Some(pick.element);
                    self.decoder_trial = Some(Instant::now());
                    self.fail(format!("Could not start {}: {error}", pick.element));
                    return;
                }
                Err(error) => {
                    self.fail(format!("Could not start a decoder: {error}"));
                    return;
                }
            }
        } else if self
            .decoder_trial
            .is_some_and(|since| since.elapsed() >= DECODER_TRIAL)
        {
            self.fail("It gave no pictures".to_owned());
            return;
        }
        self.waiting_keyframe = false;
        let pushed = self
            .decoder
            .as_ref()
            .map(|decoder| decoder.push(unit, self.base_us));
        if let Some(Err(error)) = pushed {
            self.fail(error.to_string());
        }
    }

    fn fail(&mut self, detail: String) {
        // A hardware decoder that never gave a picture will not start now: on
        // to the next one at once. One that was working gets the usual retry.
        if self.decoder_trial.take().is_some() {
            if let Some(element) = self.current_decoder.take() {
                tracing::warn!(binding = self.binding, decoder = element, %detail, "Decoder failed; trying the next");
                self.failed_decoders.push(element);
                self.decoder = None;
                self.waiting_keyframe = true;
                return;
            }
        }
        tracing::warn!(binding = self.binding, %detail, "Motion analysis failed; retrying");
        self.decoder = None;
        self.waiting_keyframe = true;
        self.retry_at = Some(Instant::now() + RETRY_AFTER);
        self.state = BindingState::Error;
        self.detail = Some(detail);
        self.reset_detection();
    }

    fn pictures(&mut self) {
        let Some(decoder) = self.decoder.as_ref() else {
            return;
        };
        if let Some(problem) = decoder.failed() {
            self.fail(problem);
            return;
        }
        loop {
            let pulled = match self.decoder.as_ref().map(Decoder::pull) {
                Some(Ok(Some(picture))) => picture,
                Some(Ok(None)) | None => return,
                Some(Err(error)) => {
                    self.fail(error.to_string());
                    return;
                }
            };
            self.picture(pulled);
        }
    }

    fn picture(&mut self, (pixels, time_ms, seq): AnalyzedPicture) {
        if self.raw_layout.is_none() {
            self.decoder_trial = None; // it works
        }
        self.picture_seq = seq;
        self.frames += 1;
        self.measured += 1;
        self.detail = if self.raw_layout.is_none() {
            self.decoder_note.clone()
        } else {
            None
        };
        if self.moving {
            self.state = BindingState::Settling;
            self.draw(DETECTIONS, vec![]);
            self.draw(DEBUG, vec![]);
            return;
        }
        self.state = BindingState::Running;
        let Some(now_ms) = time_ms else {
            return;
        };
        if self.last_ms.is_some_and(|last| now_ms < last) {
            // Timestamps went back (a new timeline): start again.
            self.reset_detection();
        }
        self.last_ms = Some(now_ms);
        match self.detector.observe(&pixels, now_ms) {
            Ok(sample) => {
                let actions = self.activity.observe(&sample, now_ms);
                self.emit(actions);
                self.draw(DETECTIONS, drawing::detections(&sample));
                let debug = drawing::debug(&sample, &self.settings, self.fps);
                self.draw(DEBUG, debug);
            }
            Err(error) => tracing::debug!(binding = self.binding, %error, "Picture skipped"),
        }
    }

    /// Send what `layer` shows now; an empty layer is sent once, to clear it.
    fn draw(&mut self, layer: &str, items: Vec<OverlayItem>) {
        let slot = usize::from(layer == DEBUG);
        if items.is_empty() && !self.drawn[slot] {
            return;
        }
        self.drawn[slot] = !items.is_empty();
        self.out.send(Message::notification(
            method::OVERLAY,
            Overlay {
                binding: self.binding,
                layer: layer.into(),
                items,
                seq: self.picture_seq,
                ttl_ms: Some(DRAWING_TTL_MS),
            },
        ));
    }

    fn finish_activity(&mut self) {
        let actions = self.activity.finish();
        self.emit(actions);
    }

    fn emit(&mut self, actions: Vec<Action>) {
        for action in actions {
            let message = match action {
                Action::Start { score } => {
                    let event = self.events.fetch_add(1, Ordering::Relaxed) + 1;
                    self.event = Some(event);
                    Message::notification(
                        method::EVENT_START,
                        EventStart {
                            binding: self.binding,
                            event,
                            kind: "motion".into(),
                            score: Some(score),
                            seq: self.picture_seq,
                        },
                    )
                }
                Action::Update { peak, bbox, zones } => {
                    let Some(event) = self.event else { continue };
                    Message::notification(
                        method::EVENT_UPDATE,
                        EventUpdate {
                            binding: self.binding,
                            event,
                            score: Some(peak),
                            bbox,
                            zones,
                        },
                    )
                }
                Action::End => {
                    let Some(event) = self.event.take() else {
                        continue;
                    };
                    Message::notification(
                        method::EVENT_END,
                        EventEnd {
                            binding: self.binding,
                            event,
                        },
                    )
                }
            };
            self.out.send(message);
        }
    }

    fn report(&mut self) {
        let elapsed = self.meter.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.fps = self.measured as f32 / elapsed.as_secs_f32();
            self.measured = 0;
            self.meter = Instant::now();
        }
        if self.reported.elapsed() < STATUS_EVERY {
            return;
        }
        self.reported = Instant::now();
        if self.decoder.is_none() && self.state == BindingState::Running {
            self.state = BindingState::Waiting;
        }
        let decoder = if self.raw_layout.is_some() {
            Some("NeuroWatch's pictures (no decoding here)".into())
        } else {
            self.decoder
                .as_ref()
                .and_then(|d| d.name.lock().ok().and_then(|name| name.clone()))
                .map(|name| decoding::status_text(&name, self.decoder_note.as_deref()))
        };
        let status = Status {
            bindings: vec![BindingStatus {
                binding: self.binding,
                state: self.state,
                detail: self.detail.clone(),
                fps: self.fps,
                frames: self.frames,
                decoder,
                figures: vec![],
            }],
        };
        self.out.send(Message::notification(method::STATUS, status));
    }
}

#[cfg(test)]
mod rate_tests {
    use super::RateLimit;

    #[test]
    fn the_rate_limit_holds_whatever_the_timestamps_start_at() {
        for start_ns in [0, 600_000_000_000, 86_400_000_000_000] {
            let mut limit = RateLimit::new(5);
            let passed = (0..250u64)
                .filter(|i| limit.admit(start_ns + i * 1_000_000_000 / 25))
                .count();
            assert_eq!(passed, 50, "10 s of 25 fps video at 5 fps, from {start_ns}");
        }
        // Keyframes a second apart (low CPU) are all kept.
        let mut limit = RateLimit::new(5);
        let passed = (0..10u64)
            .filter(|i| limit.admit(i * 1_000_000_000))
            .count();
        assert_eq!(passed, 10);
    }

    #[test]
    fn the_rate_limit_survives_a_gap_and_a_new_timeline() {
        let mut limit = RateLimit::new(5);
        assert!(limit.admit(0));
        assert!(!limit.admit(100_000_000));
        // A gap of a minute: no burst to catch up afterwards.
        assert!(limit.admit(60_000_000_000));
        assert!(!limit.admit(60_040_000_000));
        // The camera restarted: timestamps from zero again.
        assert!(limit.admit(0));
        assert!(!limit.admit(40_000_000));
        assert!(limit.admit(200_000_000));
    }
}
