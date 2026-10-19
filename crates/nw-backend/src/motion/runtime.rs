//! A bounded, opt-in analysis worker per camera. Never shares recording's tee,
//! state machine, or media-thread callbacks. Desktop raw pictures can replace
//! its decoder without replacing the detector or its ongoing event.
use super::{
    analysis,
    detector::{Detector, HEIGHT, WIDTH},
};
use crate::app_state::AppState;
use crate::events::{EventHub, NewEvent};
use crate::pipeline::{builder, hwaccel::HwAccelKind};
use crate::streams::{
    feed::{Pushed, VideoFeed},
    Delivery, Phase, SharedUnit, StreamHubs, SubscriberPolicy, Subscription, Unit,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use gstreamer::prelude::*;
use gstreamer_video::prelude::*;
use nw_core::camera::CameraConfig;
use nw_core::event::{EventKind, EventSource};
use nw_core::stats::MotionMetrics;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};

#[derive(Debug, Clone, Serialize)]
pub struct MotionStatus {
    pub state: &'static str,
    pub detail: Option<&'static str>,
    pub analysis_fps: f32,
    pub requested_fps: u8,
    pub frames: u64,
    pub active: bool,
    pub score: f32,
    pub using_substream: bool,
    pub low_cpu: bool,
    pub decoder_running: bool,
    pub decoder: Option<String>,
    pub input_fps: f32,
    pub dropped_frames: u64,
    pub dropped_fps: f32,
    pub skipped_delta_frames: u64,
    pub analysis_ms: f32,
    pub blocks: Vec<u16>,
    #[serde(skip)]
    preview: Option<Arc<[u8]>>,
}

impl MotionStatus {
    fn new(camera: &CameraConfig) -> Self {
        Self {
            state: "connecting",
            detail: None,
            analysis_fps: 0.0,
            requested_fps: 5,
            frames: 0,
            active: false,
            score: 0.0,
            using_substream: camera.sub_url.as_ref().is_some_and(|v| !v.is_empty()),
            low_cpu: camera.motion.low_cpu,
            decoder_running: false,
            decoder: None,
            input_fps: 0.0,
            dropped_frames: 0,
            dropped_fps: 0.0,
            skipped_delta_frames: 0,
            analysis_ms: 0.0,
            blocks: vec![],
            preview: None,
        }
    }

    fn clear_live(&mut self) {
        self.active = false;
        self.score = 0.0;
        self.analysis_fps = 0.0;
        self.input_fps = 0.0;
        self.dropped_fps = 0.0;
        self.analysis_ms = 0.0;
        self.decoder_running = false;
        self.blocks.clear();
        self.preview = None;
    }
}

#[derive(Default)]
pub struct BuiltinMotion {
    status: Mutex<HashMap<String, MotionStatus>>,
}
struct Running {
    camera: CameraConfig,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl BuiltinMotion {
    pub fn status(&self, id: &str) -> Option<MotionStatus> {
        let statuses = self.status.lock().ok()?;
        let status = statuses.get(id)?;
        // Routine status/health requests never copy the image.
        let mut value = status.clone();
        value.preview = None;
        Some(value)
    }
    /// Capture metadata and pixels from the same observation. Encode after
    /// releasing the lock so polling never stalls a producer.
    pub fn snapshot(
        &self,
        id: &str,
        include_preview: bool,
    ) -> (Option<MotionStatus>, Option<serde_json::Value>) {
        let status = self
            .status
            .lock()
            .ok()
            .and_then(|statuses| statuses.get(id).cloned());
        let preview = status.as_ref().filter(|_| include_preview).and_then(|s| {
            s.preview.as_ref().map(|pixels| {
                serde_json::json!({"width": WIDTH, "height": HEIGHT,
                "gray_b64": STANDARD.encode(pixels), "sequence": s.frames})
            })
        });
        (status, preview)
    }
    pub fn metrics(&self) -> Vec<MotionMetrics> {
        let Ok(statuses) = self.status.lock() else {
            return vec![];
        };
        let mut metrics: Vec<_> = statuses
            .iter()
            .map(|(id, s)| MotionMetrics {
                camera_id: id.clone(),
                state: s.state.into(),
                decoder_running: s.decoder_running,
                decoder: s.decoder.clone(),
                low_cpu: s.low_cpu,
                using_substream: s.using_substream,
                analysis_fps: s.analysis_fps,
                input_fps: s.input_fps,
                frames: s.frames,
                dropped_frames: s.dropped_frames,
                dropped_fps: s.dropped_fps,
                skipped_delta_frames: s.skipped_delta_frames,
                analysis_ms: s.analysis_ms,
            })
            .collect();
        drop(statuses);
        metrics.sort_by(|a, b| a.camera_id.cmp(&b.camera_id));
        metrics
    }
    fn update(&self, id: &str, update: impl FnOnce(&mut MotionStatus)) {
        if let Ok(mut status) = self.status.lock() {
            if let Some(status) = status.get_mut(id) {
                update(status);
            }
        }
    }
    pub async fn run(
        self: Arc<Self>,
        state: Arc<RwLock<AppState>>,
        hub: Arc<EventHub>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let mut running: HashMap<String, Running> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! { _ = shutdown.recv() => break, _ = tick.tick() => {} }
            let (cameras, acceleration, streams) = {
                let state = state.read().await;
                (
                    state
                        .config
                        .cameras
                        .iter()
                        .filter(|c| c.motion.effective_source().uses_builtin())
                        .map(|c| (c.id.clone(), c.clone()))
                        .collect::<HashMap<_, _>>(),
                    state.hwaccel,
                    Arc::clone(&state.streams),
                )
            };
            let retired: Vec<_> = running
                .iter()
                .filter(|(id, run)| cameras.get(*id) != Some(&run.camera))
                .map(|(id, _)| id.clone())
                .collect();
            for id in retired {
                if let Some(run) = running.remove(&id) {
                    run.stop.store(true, Ordering::Release);
                    let _ = run.task.await;
                }
                if let Ok(mut statuses) = self.status.lock() {
                    statuses.remove(&id);
                }
            }
            for (id, camera) in cameras {
                if running.contains_key(&id) {
                    continue;
                }
                let stop = Arc::new(AtomicBool::new(false));
                if let Ok(mut statuses) = self.status.lock() {
                    statuses.insert(id.clone(), MotionStatus::new(&camera));
                }
                let owner = Arc::clone(&self);
                let events = Arc::clone(&hub);
                let worker_camera = camera.clone();
                let stopped = Arc::clone(&stop);
                let streams = Arc::clone(&streams);
                let task = tokio::task::spawn_blocking(move || {
                    owner.worker(worker_camera, acceleration, events, stopped, streams)
                });
                running.insert(id, Running { camera, stop, task });
            }
        }
        for run in running.values() {
            run.stop.store(true, Ordering::Release);
        }
        for (_, run) in running {
            let _ = run.task.await;
        }
        if let Ok(mut statuses) = self.status.lock() {
            statuses.clear();
        }
    }

    fn worker(
        &self,
        camera: CameraConfig,
        acceleration: HwAccelKind,
        hub: Arc<EventHub>,
        stop: Arc<AtomicBool>,
        streams: Arc<StreamHubs>,
    ) {
        let Some(analysis) = analysis_camera(&camera) else {
            return;
        };
        let subscription = if crate::streams::enabled()
            && builder::shared_view_supported(&analysis.url)
        {
            match streams.subscribe(&analysis, SubscriberPolicy::analysis(), None) {
                Ok(subscription) => Some(subscription),
                Err(reason) => {
                    tracing::warn!(cam_id = %camera.id, %reason, "Motion analysis could not use the camera's shared connection");
                    return;
                }
            }
        } else {
            None
        };
        self.worker_source(camera, acceleration, hub, stop, subscription);
    }

    fn worker_source(
        &self,
        camera: CameraConfig,
        acceleration: HwAccelKind,
        hub: Arc<EventHub>,
        stop: Arc<AtomicBool>,
        subscription: Option<Subscription>,
    ) {
        let Some(analysis) = analysis_camera(&camera) else {
            return;
        };
        let mut shared = subscription.map(|subscription| SharedAnalysis {
            subscription,
            pending: None,
        });
        // A runtime with the leaking VA plugin blocks local live view, but
        // `hwaccel::probe` has already removed that plugin from the registry:
        // the analysis decodebin then picks a safe (software) decoder, as the
        // other non-live decoders do. See docs/gstreamer-va-reliability.md.
        while !stop.load(Ordering::Acquire) {
            self.update(&camera.id, |s| {
                s.state = "connecting";
                s.detail = None;
                s.clear_live();
                s.decoder = None;
            });
            let description = if shared.is_some() {
                analysis::shared_pipeline()
            } else {
                analysis::pipeline(&analysis, acceleration)
            };
            let result = builder::build(&description).and_then(|pipeline| {
                let result = self.analyze(&camera, &pipeline, &hub, &stop, shared.as_mut());
                let _ = pipeline.set_state(gstreamer::State::Null);
                result
            });
            if shared
                .as_ref()
                .is_some_and(|shared| shared.subscription.state().phase == Phase::Closed)
            {
                break;
            }
            if result.is_err() && !stop.load(Ordering::Acquire) {
                self.update(&camera.id, |s| {
                    s.state = "backoff";
                    s.clear_live();
                    s.detail = Some(ANALYSIS_FAILURE);
                });
                // Short, cancellable sleep on this dedicated blocking worker.
                for _ in 0..30 {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        self.update(&camera.id, |s| {
            s.state = "stopped";
            s.clear_live();
        });
    }

    fn analyze(
        &self,
        camera: &CameraConfig,
        pipeline: &gstreamer::Pipeline,
        hub: &EventHub,
        stop: &AtomicBool,
        shared: Option<&mut SharedAnalysis>,
    ) -> anyhow::Result<()> {
        let decoded = shared
            .as_ref()
            .map(|s| s.subscription.decoded().subscribe());
        self.analyze_frames(camera, pipeline, hub, stop, shared, decoded)
    }

    fn analyze_frames(
        &self,
        camera: &CameraConfig,
        pipeline: &gstreamer::Pipeline,
        hub: &EventHub,
        stop: &AtomicBool,
        mut shared: Option<&mut SharedAnalysis>,
        mut decoded: Option<crate::streams::decoded::Receiver>,
    ) -> anyhow::Result<()> {
        let mut input = AnalysisInput::new(pipeline.clone(), camera.motion.low_cpu)?;
        let mut sharing: Option<Arc<str>> = None;
        let mut last_shared = Instant::now();
        let mut last_raw_pts = None;
        let mut last_gray_caps = None;
        let mut input_base = 0;
        let mut skipped_base = 0;
        let mut waiting_keyframe = true;
        let mut hub_waiting = false;
        let clock = Instant::now();
        let mut last_picture = clock;
        let mut meter = clock;
        let mut frames = 0u64;
        let mut measured = 0u64;
        let mut previous_input = 0;
        let mut previous_dropped = 0;
        let mut processing = Duration::ZERO;
        let (base_frames, base_dropped, base_skipped) = self
            .status(&camera.id)
            .map(|s| (s.frames, s.dropped_frames, s.skipped_delta_frames))
            .unwrap_or_default();
        let mut detector = Detector::new(&camera.motion);
        let mut settling = false;
        let mut event = OpenEvent {
            hub,
            id: None,
            updated: clock,
            peak: 0.0,
            bbox: None,
            zones: std::collections::BTreeSet::new(),
        };
        while !stop.load(Ordering::Acquire) {
            let taken = decoded.as_mut().and_then(|receiver| receiver.take());
            let entering = sharing.is_none() && taken.as_ref().is_some_and(|(_, stable)| *stable);
            let leaving = sharing.is_some()
                && taken.is_none()
                && last_shared.elapsed() >= crate::streams::decoded::SWITCH_DELAY;
            if entering || leaving {
                // Only the media input changes: detector, clock, event and
                // cumulative counters stay alive across both switches.
                input_base += input.counters.received.load(Ordering::Relaxed);
                skipped_base += input.counters.skipped.load(Ordering::Relaxed);
                input.pipeline.set_state(gstreamer::State::Null)?;
                let description = if entering {
                    analysis::decoded_pipeline()
                } else {
                    if let Some(shared) = shared.as_deref_mut() {
                        shared.subscription.restart_video();
                        shared.pending = None;
                    }
                    analysis::shared_pipeline()
                };
                let replacement = builder::build(&description)?;
                if entering {
                    if let (Some(filter), Some(caps)) =
                        (replacement.by_name("analysis_format"), &last_gray_caps)
                    {
                        filter.set_property("caps", caps);
                    }
                }
                input = AnalysisInput::new(replacement, camera.motion.low_cpu)?;
                sharing = taken
                    .as_ref()
                    .filter(|_| entering)
                    .map(|(f, _)| Arc::clone(&f.decoder));
                waiting_keyframe = true;
                last_raw_pts = None;
                last_picture = Instant::now();
            }
            if let (Some(name), Some((frame, _))) = (sharing.as_mut(), taken) {
                *name = frame.decoder;
                last_shared = Instant::now();
                if let (Some(feed), Some(buffer), Some(caps)) = (
                    input.feed.as_mut(),
                    frame.sample.buffer(),
                    frame.sample.caps(),
                ) {
                    // VideoFeed changes only buffer headers, retaining the
                    // desktop's memory and rebasing restarts/format changes.
                    let unit = SharedUnit {
                        media: crate::streams::Media::Video,
                        buffer: buffer.to_owned(),
                        caps: caps.to_owned(),
                        arrived_us: 0,
                    };
                    if last_raw_pts.is_some_and(|pts| buffer.pts().is_some_and(|next| next < pts)) {
                        feed.restart();
                    }
                    last_raw_pts = buffer.pts();
                    if feed.backlog_bytes() < 1024 * 1024 {
                        feed.push(&unit, false, true)?;
                    }
                }
            }
            let mut shared_waiting = false;
            if let (Some(shared), Some(feed)) = (shared.as_deref_mut(), input.feed.as_mut()) {
                let state = shared.subscription.state();
                if state.phase == Phase::Closed {
                    return Ok(());
                }
                shared_waiting = state.phase != Phase::Streaming;
                if shared_waiting {
                    // The hub owns camera retries; no decoder timeout while
                    // its connection is down. End activity as on the old path.
                    last_picture = Instant::now();
                    if !hub_waiting {
                        event.finish();
                        detector = Detector::new(&camera.motion);
                    }
                    self.update(&camera.id, |s| {
                        s.state = if state.phase == Phase::Retrying {
                            "backoff"
                        } else {
                            "connecting"
                        };
                        s.clear_live();
                        s.detail = (state.phase == Phase::Retrying).then_some(ANALYSIS_FAILURE);
                    });
                }
                hub_waiting = shared_waiting;
                if sharing.is_some() {
                    // Keep the compressed subscription, but never feed a
                    // decoder while borrowing desktop frames.
                    let _ = shared.subscription.recv(Duration::from_millis(10));
                } else if feed.backlog_bytes() >= 1024 * 1024 {
                    std::thread::sleep(Duration::from_millis(10));
                } else {
                    let delivery = shared
                        .pending
                        .take()
                        .map(|unit| Delivery::Unit {
                            unit,
                            after_gap: true,
                        })
                        .or_else(|| shared.subscription.recv(Duration::from_millis(10)));
                    match delivery {
                        Some(Delivery::Discontinuity { .. }) => {
                            feed.restart();
                            waiting_keyframe = true;
                            event.finish();
                            detector = Detector::new(&camera.motion);
                        }
                        Some(Delivery::Unit { unit, after_gap }) => {
                            if camera.motion.low_cpu && !unit.keyframe() {
                                input.counters.skipped.fetch_add(1, Ordering::Relaxed);
                            } else if unit.keyframe() || !waiting_keyframe {
                                waiting_keyframe = false;
                                if feed.push(&unit, after_gap, false)? == Pushed::FormatChanged {
                                    shared.pending = Some(unit);
                                    anyhow::bail!("analysis format changed");
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
            if input
                .bus
                .pop_filtered(&[gstreamer::MessageType::Error, gstreamer::MessageType::Eos])
                .is_some()
                || (!shared_waiting && last_picture.elapsed() > Duration::from_secs(15))
            {
                anyhow::bail!("analysis stream unavailable");
            }
            let wait = if shared.is_some() { 0 } else { 50 };
            let Some(sample) = input
                .sink
                .try_pull_sample(gstreamer::ClockTime::from_mseconds(wait))
            else {
                continue;
            };
            if shared_waiting {
                continue;
            }
            let caps = sample
                .caps()
                .ok_or_else(|| anyhow::anyhow!("missing analysis caps"))?;
            let info = gstreamer_video::VideoInfo::from_caps(caps)?;
            last_gray_caps = Some(caps.to_owned());
            if info.width() != WIDTH as u32
                || info.height() != HEIGHT as u32
                || info.format() != gstreamer_video::VideoFormat::Gray8
            {
                anyhow::bail!("invalid analysis format");
            }
            let buffer = sample
                .buffer()
                .ok_or_else(|| anyhow::anyhow!("missing analysis buffer"))?;
            let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)?;
            let stride = usize::try_from(frame.plane_stride()[0])?;
            let plane = frame.plane_data(0)?;
            let mut pixels = vec![0u8; WIDTH * HEIGHT];
            for row in 0..HEIGHT {
                pixels[row * WIDTH..(row + 1) * WIDTH]
                    .copy_from_slice(&plane[row * stride..row * stride + WIDTH]);
            }
            let now = Instant::now();
            last_picture = now;
            frames += 1;
            measured += 1;
            if hub.activity_paused(&camera.id) {
                if !settling {
                    detector = Detector::new(&camera.motion);
                    event.finish();
                    settling = true;
                }
                self.update(&camera.id, |s| {
                    s.state = "settling";
                    s.active = false;
                    s.score = 0.0;
                    s.blocks.clear();
                    s.preview = None;
                    s.analysis_fps = 0.0;
                    s.input_fps = 0.0;
                });
                continue;
            }
            if settling {
                settling = false;
                meter = now;
                measured = 0;
                processing = Duration::ZERO;
            }
            let processing_start = Instant::now();
            let result = detector
                .observe(&pixels, clock.elapsed().as_millis() as i64)
                .map_err(anyhow::Error::msg)?;
            processing += processing_start.elapsed();
            if result.active && event.id.is_none() {
                let mut start = NewEvent::new(
                    Some(camera.id.clone()),
                    EventKind::Motion,
                    EventSource::Builtin,
                );
                start.score = Some(result.score);
                event.id = Some(hub.start(start).id);
                event.updated = now.checked_sub(Duration::from_secs(2)).unwrap_or(now);
            }
            if event.id.is_some() {
                event.peak = event.peak.max(result.score);
                if let Some(bbox) = result.bbox {
                    event.bbox = Some(union_bbox(event.bbox, bbox));
                }
                event.zones.extend(result.zones.iter().cloned());
                if !result.active {
                    event.finish();
                } else if event.updated.elapsed() >= Duration::from_secs(2) {
                    event.flush();
                    event.updated = now;
                }
            }
            let received = input_base + buffer.offset();
            let dropped = received.saturating_sub(frames);
            let meter_values = if meter.elapsed() >= Duration::from_secs(1) {
                let elapsed = meter.elapsed().as_secs_f32();
                let values = (
                    measured as f32 / elapsed,
                    received.saturating_sub(previous_input) as f32 / elapsed,
                    dropped.saturating_sub(previous_dropped) as f32 / elapsed,
                    processing.as_secs_f32() * 1000.0 / measured.max(1) as f32,
                );
                measured = 0;
                meter = now;
                previous_input = received;
                previous_dropped = dropped;
                processing = Duration::ZERO;
                Some(values)
            } else {
                None
            };
            let preview: Arc<[u8]> = pixels.into();
            self.update(&camera.id, |status| {
                status.state = "running";
                status.detail = None;
                status.frames = base_frames + frames;
                status.active = result.active;
                status.score = result.score;
                status.blocks = result.blocks;
                status.preview = Some(preview);
                status.dropped_frames = base_dropped + dropped;
                status.skipped_delta_frames =
                    base_skipped + skipped_base + input.counters.skipped.load(Ordering::Relaxed);
                // `shared:` names the desktop decoder; it is not another
                // running motion decoder in stats.get_performance.
                status.decoder = sharing
                    .as_ref()
                    .map(|name| format!("shared:{name}"))
                    .or_else(|| input.counters.decoder.lock().ok().and_then(|v| v.clone()));
                status.decoder_running = sharing.is_none() && status.decoder.is_some();
                if let Some((fps, input_fps, dropped_fps, analysis_ms)) = meter_values {
                    status.analysis_fps = fps;
                    status.input_fps = input_fps;
                    status.dropped_fps = dropped_fps;
                    status.analysis_ms = analysis_ms;
                }
            });
        }
        Ok(())
    }
}

const ANALYSIS_FAILURE: &str = "The analysis stream failed or stopped sending pictures. Check the stream, credentials and whether the camera permits another connection; recording is unaffected";

/// Every replacement owns its teardown, including early errors and shutdown.
struct AnalysisInput {
    pipeline: gstreamer::Pipeline,
    sink: gstreamer_app::AppSink,
    bus: gstreamer::Bus,
    counters: Arc<analysis::Counters>,
    feed: Option<VideoFeed>,
}

impl AnalysisInput {
    fn new(pipeline: gstreamer::Pipeline, low_cpu: bool) -> anyhow::Result<Self> {
        let sink = pipeline
            .by_name("analysis_sink")
            .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or_else(|| anyhow::anyhow!("missing analysis sink"))?;
        let bus = pipeline
            .bus()
            .ok_or_else(|| anyhow::anyhow!("missing analysis bus"))?;
        let counters = analysis::instrument(&pipeline, low_cpu);
        let feed = pipeline
            .by_name("analysis_source")
            .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
            .map(VideoFeed::new);
        let input = Self {
            pipeline,
            sink,
            bus,
            counters,
            feed,
        };
        input.pipeline.set_state(gstreamer::State::Playing)?;
        Ok(input)
    }
}

impl Drop for AnalysisInput {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
    }
}

struct SharedAnalysis {
    subscription: Subscription,
    // A format-changing keyframe starts the replacement decoder.
    pending: Option<SharedUnit>,
}

struct OpenEvent<'a> {
    hub: &'a EventHub,
    id: Option<i64>,
    updated: Instant,
    peak: f32,
    bbox: Option<[f32; 4]>,
    zones: std::collections::BTreeSet<String>,
}
impl OpenEvent<'_> {
    fn flush(&self) {
        if let Some(id) = self.id {
            self.hub.update(id, |record| {
                record.score = Some(self.peak);
                record.bbox = self.bbox;
                record.zones = self.zones.iter().cloned().collect();
            });
        }
    }
    fn finish(&mut self) {
        self.flush();
        if let Some(id) = self.id.take() {
            self.hub.end(id);
        }
        self.peak = 0.0;
        self.bbox = None;
        self.zones.clear();
    }
}
impl Drop for OpenEvent<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

fn union_bbox(previous: Option<[f32; 4]>, next: [f32; 4]) -> [f32; 4] {
    let Some(old) = previous else {
        return next;
    };
    let x = old[0].min(next[0]);
    let y = old[1].min(next[1]);
    [
        x,
        y,
        (old[0] + old[2]).max(next[0] + next[2]) - x,
        (old[1] + old[3]).max(next[1] + next[3]) - y,
    ]
}

fn analysis_camera(camera: &CameraConfig) -> Option<CameraConfig> {
    if !camera.motion.effective_source().uses_builtin() {
        return None;
    }
    let mut analysis = camera.clone();
    if let Some(sub) = camera.sub_url.as_ref().filter(|v| !v.is_empty()) {
        analysis.url = sub.clone();
    }
    Some(analysis)
}
#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::camera::{CameraConfig, CameraMotionConfig, MotionSource};

    struct Tile {
        stop: tokio::sync::watch::Sender<bool>,
        task: Option<std::thread::JoinHandle<()>>,
        frames: Arc<std::sync::atomic::AtomicU64>,
    }

    impl Tile {
        fn start(camera: &CameraConfig, streams: &Arc<StreamHubs>) -> Self {
            use crate::pipeline::supervisor::{tests::fenced, PipelineSupervisor, StreamInfo};
            struct Count(Arc<std::sync::atomic::AtomicU64>);
            impl crate::taps::FrameTap for Count {
                fn wants(&self, _: &str) -> bool {
                    true
                }
                fn submit_sample(&self, _: &str, _: &gstreamer::Sample) {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
            let frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let (stop, stop_rx) = tokio::sync::watch::channel(false);
            let (status, _) = tokio::sync::watch::channel(String::new());
            let (info, _) = tokio::sync::watch::channel(StreamInfo::default());
            let (assignment, registry) = fenced(camera);
            let supervisor = PipelineSupervisor::new(
                camera.clone(),
                HwAccelKind::Software,
                Arc::new(nw_core::traits::LogEventBus),
                stop_rx,
                status,
                info,
            )
            .with_assignment(assignment, registry)
            .with_headless(true)
            .with_frame_tap(Arc::new(Count(Arc::clone(&frames))))
            .with_stream_hubs(Arc::clone(streams))
            .with_shared_any();
            Self {
                stop,
                task: Some(std::thread::spawn(move || supervisor.run())),
                frames,
            }
        }
    }

    impl Drop for Tile {
        fn drop(&mut self) {
            let _ = self.stop.send(true);
            if let Some(task) = self.task.take() {
                task.join().unwrap();
            }
        }
    }

    struct MotionRun {
        owner: Arc<BuiltinMotion>,
        events: Arc<EventHub>,
        stop: Arc<AtomicBool>,
        task: Option<std::thread::JoinHandle<()>>,
        camera: CameraConfig,
    }

    impl MotionRun {
        fn start(camera: &CameraConfig, streams: &StreamHubs, sharing: bool) -> Self {
            let owner = Arc::new(BuiltinMotion::default());
            owner
                .status
                .lock()
                .unwrap()
                .insert(camera.id.clone(), MotionStatus::new(camera));
            let events = Arc::new(EventHub::new(Arc::new(Mutex::new(
                crate::db::connection::open_in_memory().unwrap(),
            ))));
            let stop = Arc::new(AtomicBool::new(false));
            let subscription = streams
                .subscribe(
                    &analysis_camera(camera).unwrap(),
                    SubscriberPolicy::analysis(),
                    None,
                )
                .unwrap();
            let (run_owner, hub, stopped, cam) = (
                Arc::clone(&owner),
                Arc::clone(&events),
                Arc::clone(&stop),
                camera.clone(),
            );
            let task = std::thread::spawn(move || {
                if sharing {
                    run_owner.worker_source(
                        cam,
                        HwAccelKind::Software,
                        hub,
                        stopped,
                        Some(subscription),
                    );
                } else {
                    let pipeline = builder::build(&analysis::shared_pipeline()).unwrap();
                    let mut shared = SharedAnalysis {
                        subscription,
                        pending: None,
                    };
                    run_owner
                        .analyze_frames(&cam, &pipeline, &hub, &stopped, Some(&mut shared), None)
                        .unwrap();
                }
            });
            Self {
                owner,
                events,
                stop,
                task: Some(task),
                camera: camera.clone(),
            }
        }

        fn wait(
            &self,
            timeout: Duration,
            event: Option<i64>,
            ready: impl Fn(&MotionStatus) -> bool,
        ) -> MotionStatus {
            let deadline = Instant::now() + timeout;
            loop {
                let status = self.owner.status(&self.camera.id).unwrap();
                assert_ne!(status.state, "backoff", "{status:?}");
                if let Some(id) = event {
                    assert!(
                        self.events.open_events().iter().any(|e| e.id == id),
                        "switch closed the ongoing motion event: {status:?}"
                    );
                }
                let metrics = self.owner.metrics();
                assert_eq!(metrics.len(), 1);
                if ready(&status) {
                    return status;
                }
                assert!(Instant::now() < deadline, "timed out: {status:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for MotionRun {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(task) = self.task.take() {
                task.join().unwrap();
            }
        }
    }

    fn exercise_decoded_switches(low_cpu: bool, keep_event: bool) {
        let fixture = if keep_event {
            crate::streams::testing::CameraFile::sized(750, 1280, 720)
        } else {
            crate::streams::testing::CameraFile::new(750)
        };
        let mut camera = fixture.camera.clone();
        camera.motion.source = MotionSource::Builtin;
        camera.sub_url = Some(camera.url.clone());
        camera.motion.low_cpu = low_cpu;
        camera.motion.sensitivity = 10;
        camera.motion.min_duration_sec = 0.0;
        camera.motion.cooldown_sec = 60.0;
        let streams = Arc::new(StreamHubs::default());
        let motion = MotionRun::start(&camera, &streams, true);
        motion.wait(Duration::from_secs(5), None, |s| {
            s.decoder_running
                && s.frames >= 3
                && (!keep_event || !motion.events.open_events().is_empty())
        });
        let event = keep_event.then(|| motion.events.open_events()[0].id);
        let tile = Tile::start(&camera, &streams);
        let shared = motion.wait(Duration::from_secs(3), event, |s| {
            s.state == "running"
                && !s.decoder_running
                && s.decoder
                    .as_deref()
                    .is_some_and(|name| name.starts_with("shared:"))
        });
        assert!(shared.using_substream);
        assert_eq!(streams.open_count(), 1);
        assert_eq!(streams.summary()[0].1, 2);
        assert!(tile.frames.load(Ordering::Relaxed) >= 10);
        let shared_start = Instant::now();
        let later = motion.wait(Duration::from_secs(2), event, |s| {
            s.frames >= shared.frames + 5
        });
        assert!(!later.decoder_running);
        // Five more frames require five publication intervals. The status
        // meter can still include the initial cached-GOP burst at a switch.
        assert!(shared_start.elapsed() >= Duration::from_millis(900));
        assert!(later.analysis_fps > 2.0, "{later:?}");
        assert_eq!(later.skipped_delta_frames, shared.skipped_delta_frames);
        assert_eq!(
            motion.owner.metrics()[0].decoder_running,
            later.decoder_running
        );
        drop(tile);
        let own = motion.wait(Duration::from_secs(2), event, |s| {
            s.decoder_running && s.frames > later.frames
        });
        assert_eq!(streams.summary()[0].1, 1);
        motion.wait(Duration::from_secs(2), event, |s| {
            s.frames >= own.frames + 2
        });
        let tile = Tile::start(&camera, &streams);
        let again = motion.wait(Duration::from_secs(3), event, |s| {
            !s.decoder_running
                && s.decoder
                    .as_deref()
                    .is_some_and(|d| d.starts_with("shared:"))
        });
        motion.wait(Duration::from_secs(2), event, |s| {
            s.frames >= again.frames + 3
        });
        drop(tile);
        drop(motion);
        streams.shutdown();
    }

    #[test]
    fn motion_borrows_the_desktop_decoder_and_resumes_its_own_when_the_tile_stops() {
        exercise_decoded_switches(false, false);
    }

    #[test]
    fn low_cpu_motion_analyses_shared_desktop_pictures_at_the_normal_rate() {
        exercise_decoded_switches(true, false);
    }

    #[test]
    fn an_ongoing_motion_event_survives_both_desktop_decoder_switches() {
        exercise_decoded_switches(false, true);
    }

    /// Opt-in CPU measurement, no performance assertion. Run alone with:
    /// `env -u GIO_MODULE_DIR cargo test -p nw-backend --lib decoded_fanout_cpu -- --ignored --nocapture --test-threads=1`
    /// Three runs per mode and resolution observe detailed 720p/1080p H.264
    /// for 10 s after warm-up, reversing the order between each pair. It runs
    /// twice: with the decoders `decodebin` picks on this machine (a GPU
    /// decoder costs little CPU, so sharing saves little CPU there), then with
    /// the hardware decoders removed from the registry (software decoding, as
    /// on machines without a usable GPU decoder or with the VA guard, where
    /// sharing saves a whole decode). Tests never run `hwaccel::probe`, so the
    /// first pass may use a VA plugin that production would exclude.
    #[test]
    #[ignore = "120 s measured CPU plus warm-up and encoding; run alone for useful numbers"]
    #[cfg(unix)]
    fn decoded_fanout_cpu() {
        fn cpu_seconds() -> f64 {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            // SAFETY: getrusage initializes the supplied rusage on success.
            assert_eq!(
                unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
                0
            );
            let usage = unsafe { usage.assume_init() };
            let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
            seconds(usage.ru_utime) + seconds(usage.ru_stime)
        }
        for decoding in ["machine default", "software"] {
            if decoding == "software" {
                let removed = crate::pipeline::hwaccel::remove_hardware_decoders();
                println!("software decoding: {removed} hardware decoder features removed");
            }
            for (width, height, bitrate) in [(1280, 720, 4000), (1920, 1080, 8000)] {
                let fixture =
                    crate::streams::testing::CameraFile::detailed(1000, width, height, bitrate);
                let mut camera = fixture.camera.clone();
                camera.motion.source = MotionSource::Builtin;
                let mut results = [Vec::new(), Vec::new()];
                for (run, sharing) in [false, true, true, false, false, true]
                    .into_iter()
                    .enumerate()
                {
                    let streams = Arc::new(StreamHubs::default());
                    let motion = MotionRun::start(&camera, &streams, sharing);
                    let tile = Tile::start(&camera, &streams);
                    let before = motion.wait(Duration::from_secs(5), None, |s| {
                        s.state == "running"
                            && s.frames >= 8
                            && s.decoder_running != sharing
                            && s.decoder
                                .as_deref()
                                .is_some_and(|name| name.starts_with("shared:") == sharing)
                    });
                    assert!(tile.frames.load(Ordering::Relaxed) > 10);
                    let decoder = before.decoder.clone().unwrap_or_default();
                    let start = cpu_seconds();
                    let until = Instant::now() + Duration::from_secs(10);
                    while Instant::now() < until {
                        let status = motion.owner.status(&camera.id).unwrap();
                        assert_eq!(status.state, "running");
                        assert_eq!(status.decoder_running, !sharing);
                        assert!(status
                            .decoder
                            .as_deref()
                            .is_some_and(|name| name.starts_with("shared:") == sharing));
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let cpu = cpu_seconds() - start;
                    results[usize::from(sharing)].push(cpu);
                    let mode = if sharing { "shared" } else { "separate" };
                    println!(
                    "{decoding}: {width}x{height} @ {bitrate} kbit/s, run {}/6, {mode} ({decoder}): {cpu:.3} CPU s",
                    run + 1
                );
                    assert!(motion.owner.status(&camera.id).unwrap().frames > before.frames + 30);
                    drop(tile);
                    drop(motion);
                    streams.shutdown();
                }
                for times in &mut results {
                    times.sort_by(f64::total_cmp);
                }
                let separate = results[0][1];
                let shared = results[1][1];
                let difference = separate - shared;
                let percent = difference / separate * 100.0;
                println!("{decoding}: {width}x{height} medians: separate={separate:.3} CPU s, shared={shared:.3} CPU s; separate - shared={difference:+.3} CPU s ({percent:+.2}%)");
            }
        }
    }

    #[test]
    fn motion_analysis_shares_a_connection_and_resumes_after_its_reconnect() {
        let fixture = crate::streams::testing::CameraFile::new(50);
        let mut camera = fixture.camera.clone();
        camera.motion.source = MotionSource::Builtin;
        let streams = Arc::new(StreamHubs::default());
        let recorder = streams
            .subscribe(&camera, SubscriberPolicy::recorder(), None)
            .unwrap();
        let subscription = streams
            .subscribe(&camera, SubscriberPolicy::analysis(), None)
            .unwrap();
        let owner = Arc::new(BuiltinMotion::default());
        owner
            .status
            .lock()
            .unwrap()
            .insert(camera.id.clone(), MotionStatus::new(&camera));
        let events = Arc::new(EventHub::new(Arc::new(Mutex::new(
            crate::db::connection::open_in_memory().unwrap(),
        ))));
        let stopped = Arc::new(AtomicBool::new(false));
        let run_owner = Arc::clone(&owner);
        let stop = Arc::clone(&stopped);
        let cam = camera.clone();
        let task = std::thread::spawn(move || {
            run_owner.worker_source(cam, HwAccelKind::Software, events, stop, Some(subscription))
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut before = None;
        let mut resumed = false;
        let mut gray_preview = false;
        while Instant::now() < deadline {
            let (status, preview) = owner.snapshot(&camera.id, true);
            let status = status.unwrap();
            assert_eq!(streams.open_count(), 1);
            assert_eq!(streams.summary()[0].1, 2);
            if let Some(preview) = preview {
                assert_eq!(preview["width"], 320);
                assert_eq!(preview["height"], 180);
                assert_eq!(
                    STANDARD
                        .decode(preview["gray_b64"].as_str().unwrap())
                        .unwrap()
                        .len(),
                    WIDTH * HEIGHT
                );
                gray_preview = true;
            }
            if recorder.state().phase == Phase::Retrying && status.state == "backoff" {
                before.get_or_insert(status.frames);
            }
            if before.is_some_and(|frames| status.frames > frames + 3) && status.state == "running"
            {
                resumed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stopped.store(true, Ordering::Release);
        task.join().unwrap();
        streams.shutdown();
        assert!(gray_preview, "analysis produced a validated GRAY8 frame");
        assert!(
            resumed,
            "analysis follows the hub through backoff and recovery"
        );
    }

    #[test]
    fn shared_low_cpu_analysis_never_pushes_delta_units_into_its_decoder() {
        use std::sync::atomic::AtomicU64;
        let fixture = crate::streams::testing::CameraFile::new(50);
        let mut camera = fixture.camera.clone();
        camera.motion.source = MotionSource::Builtin;
        camera.motion.low_cpu = true;
        let streams = StreamHubs::default();
        let recorder = streams
            .subscribe(&camera, SubscriberPolicy::recorder(), None)
            .unwrap();
        let subscription = streams
            .subscribe(&camera, SubscriberPolicy::analysis(), None)
            .unwrap();
        let pipeline = builder::build(&analysis::shared_pipeline()).unwrap();
        let source = pipeline.by_name("analysis_source").unwrap();
        let deltas = Arc::new(AtomicU64::new(0));
        let inputs = Arc::new(AtomicU64::new(0));
        let delta_count = Arc::clone(&deltas);
        let input_count = Arc::clone(&inputs);
        // Check before decodebin, ahead of the old decoder pad filter.
        source.static_pad("src").unwrap().add_probe(
            gstreamer::PadProbeType::BUFFER,
            move |_, info| {
                input_count.fetch_add(1, Ordering::Relaxed);
                if info
                    .buffer()
                    .is_some_and(|b| b.flags().contains(gstreamer::BufferFlags::DELTA_UNIT))
                {
                    delta_count.fetch_add(1, Ordering::Relaxed);
                }
                gstreamer::PadProbeReturn::Ok
            },
        );
        let owner = Arc::new(BuiltinMotion::default());
        owner
            .status
            .lock()
            .unwrap()
            .insert(camera.id.clone(), MotionStatus::new(&camera));
        let events = EventHub::new(Arc::new(Mutex::new(
            crate::db::connection::open_in_memory().unwrap(),
        )));
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let run_owner = Arc::clone(&owner);
        let cam = camera.clone();
        let pipe = pipeline.clone();
        let task = std::thread::spawn(move || {
            let mut shared = SharedAnalysis {
                subscription,
                pending: None,
            };
            run_owner.analyze(&cam, &pipe, &events, &stop, Some(&mut shared))
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut before_reconnect = None;
        let mut resumed = false;
        while Instant::now() < deadline {
            let status = owner.status(&camera.id).unwrap();
            if recorder.state().phase == Phase::Retrying && status.state == "backoff" {
                before_reconnect.get_or_insert(status.frames);
            }
            if before_reconnect.is_some_and(|before| status.frames >= before + 2) {
                resumed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stopped.store(true, Ordering::Release);
        let result = task.join().unwrap();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        let status = owner.status(&camera.id).unwrap();
        assert_eq!(streams.summary()[0].1, 1);
        streams.shutdown();
        drop(recorder);
        assert!(result.is_ok(), "{result:?}");
        assert!(
            resumed,
            "the same decoder resumed without failing its analysis run"
        );
        assert!(pipeline
            .iterate_recurse()
            .into_iter()
            .flatten()
            .find(|e| e.factory().is_some_and(|f| f.name() == "decodebin"))
            .unwrap()
            .property::<bool>("force-sw-decoders"));
        assert!(status.frames >= 3, "{status:?}");
        assert!(status.skipped_delta_frames >= 15, "{status:?}");
        assert!(
            status
                .decoder
                .as_deref()
                .is_some_and(|name| !name.contains("va") && !name.contains("nv")),
            "{status:?}"
        );
        assert!(inputs.load(Ordering::Relaxed) >= 3);
        assert_eq!(deltas.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn analysis_preview_is_opt_in_bounded_and_atomic_with_block_metadata() {
        let camera = CameraConfig::new("test", "unused");
        let owner = BuiltinMotion::default();
        let mut status = MotionStatus::new(&camera);
        status.frames = 42;
        status.blocks = vec![1, 143];
        status.preview = Some(vec![40; WIDTH * HEIGHT].into());
        owner
            .status
            .lock()
            .unwrap()
            .insert(camera.id.clone(), status);
        let (status, preview) = owner.snapshot(&camera.id, false);
        assert!(preview.is_none());
        assert!(serde_json::to_value(status)
            .unwrap()
            .get("preview")
            .is_none());
        let (status, preview) = owner.snapshot(&camera.id, true);
        assert_eq!(status.unwrap().blocks, vec![1, 143]);
        let preview = preview.unwrap();
        assert_eq!(preview["sequence"], 42);
        assert_eq!(preview["gray_b64"].as_str().unwrap().len(), 76800);
        assert!(serde_json::to_string(&owner.metrics()).unwrap().len() < 1000);
        owner.update(&camera.id, MotionStatus::clear_live);
        let (status, preview) = owner.snapshot(&camera.id, true);
        assert!(preview.is_none());
        assert!(status.unwrap().blocks.is_empty());
    }

    #[test]
    fn source_selection_prefers_substream_and_opt_out_never_opens_a_decoder() {
        let mut camera: CameraConfig = serde_json::from_value(serde_json::json!({
            "id":"test", "name":"Test", "url":"rtsp://main.invalid/video",
            "sub_url":"rtsp://sub.invalid/video", "username":null,"password_b64":null,
            "transport":"tcp","notes":"","wall_position":null
        }))
        .unwrap();
        assert!(analysis_camera(&camera).is_none());
        camera.motion = CameraMotionConfig {
            source: MotionSource::Builtin,
            ..Default::default()
        };
        let analysis = analysis_camera(&camera).unwrap();
        assert_eq!(analysis.url, "rtsp://sub.invalid/video");
        let source = analysis::pipeline(&analysis, crate::pipeline::hwaccel::HwAccelKind::Software);
        assert!(source.contains("format=GRAY8,width=320,height=180"));
        assert!(source.contains("max-buffers=1 drop=true"));
        assert!(!source.contains("splitmuxsink"));
        assert!(!source.contains("emit-signals=true"));
    }

    #[test]
    fn raw_analysis_worker_produces_and_closes_a_local_event_without_a_desktop_view() {
        gstreamer::init().unwrap();
        let pipeline = builder::build("appsrc name=test_source is-live=true format=time caps=video/x-raw,format=GRAY8,width=320,height=180,framerate=5/1 ! appsink name=analysis_sink max-buffers=1 drop=true sync=false").unwrap();
        let source = pipeline
            .by_name("test_source")
            .unwrap()
            .downcast::<gstreamer_app::AppSrc>()
            .unwrap();
        let camera: CameraConfig = serde_json::from_value(serde_json::json!({
            "id":"analysis-test", "name":"Test", "url":"unused", "sub_url":null,
            "username":null,"password_b64":null,"transport":"tcp","notes":"","wall_position":null,
            "motion":{"source":"builtin","min_duration_sec":0,"cooldown_sec":0}
        }))
        .unwrap();
        let db = Arc::new(Mutex::new(crate::db::connection::open_in_memory().unwrap()));
        let hub = Arc::new(EventHub::new(db));
        let owner = Arc::new(BuiltinMotion::default());
        let stopped = Arc::new(AtomicBool::new(false));
        let events = Arc::clone(&hub);
        let stop = Arc::clone(&stopped);
        let pipe = pipeline.clone();
        let task = std::thread::spawn(move || owner.analyze(&camera, &pipe, &events, &stop, None));
        source
            .push_buffer(gstreamer::Buffer::from_mut_slice(vec![
                30u8;
                WIDTH * HEIGHT
            ]))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let mut pixels = vec![30u8; WIDTH * HEIGHT];
        for y in 40..100 {
            for x in 30..110 {
                pixels[y * WIDTH + x] = 180;
            }
        }
        source
            .push_buffer(gstreamer::Buffer::from_mut_slice(pixels))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while hub.open_events().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let opened = hub.open_events();
        stopped.store(true, Ordering::Release);
        assert!(task.join().unwrap().is_ok());
        pipeline.set_state(gstreamer::State::Null).unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].source, EventSource::Builtin);
        assert!(opened[0].bbox.is_some());
        assert!(
            hub.open_events().is_empty(),
            "stopping analysis must finish its event"
        );
        hub.flush();
    }
}
