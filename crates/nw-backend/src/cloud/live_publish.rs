//! Browser live view of a camera's own compressed stream.
//!
//! A live publication subscribes to the camera stream it was granted through
//! the camera's shared connection ([`crate::streams`], opened by
//! [`CameraLiveSourceOpener`]): the recording and every other viewer of that
//! stream use the same connection. Units are rewritten to Annex-B with the
//! parameter sets before each keyframe ([`crate::streams::annexb`]);
//! nothing is decoded. A publication that falls behind loses only its own
//! units (its subscription's policy), so it cannot stall the recording, and
//! the recording cannot stall it. With `NW_STREAM_HUB=0` each publication
//! opens a dedicated connection instead ([`crate::pipeline::encoded`]).
//!
//! `main` is the camera's main stream and `sub` its configured sub stream. A
//! camera without a sub stream has no `sub`: the main stream is never
//! relabelled. [`StreamCodecs`] remembers each stream's codec once seen, for
//! the cloud inventory, so the portal offers only streams a browser can get.
//!
//! The camera's sound, decoded to samples for the publication's audio track
//! (only the sound is decoded, never the video), comes from the same shared
//! connection; watching the sub stream of a camera whose sub stream has no
//! sound, it comes from the main stream's (the recording's) connection. On a
//! dedicated connection a camera known to send audio gets an audio-only
//! connection to its main stream ([`crate::pipeline::audio::LiveAudio`]).
//!
//! The shared connection reconnects on its own; a dedicated source does so
//! with a capped backoff, mirroring the camera supervisor: a shared
//! publication outlives any one viewer, so a camera blip must not leave it
//! dark until the session ends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;
use nw_cloud_protocol::media_session::Representation;
use nw_core::camera::CameraConfig;
use nw_core::encoded::VideoCodec;
use tokio::sync::{watch, RwLock};

use crate::app_state::AppState;
use crate::cloud::media_publish::{
    EncodedSubmitter, LiveSourceOpener, LiveSourceRun, PublishError, AUDIO_RATE,
};
use crate::pipeline::audio::{self, AudioOutput, LiveAudio};
use crate::pipeline::encoded::{self, EncodedOutputs};
use crate::pipeline::sanitize_credentials;
use crate::streams::annexb::AnnexB;
use crate::streams::sound::SoundDecoder;
use crate::streams::{Delivery, Media, Phase, SharedUnit, SubscriberPolicy, Subscription};
use nw_core::encoded::EncodedAccessUnit;

/// A running pipeline that delivers no access unit for this long is rebuilt.
const NO_UNIT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Longest single wait, so a stop is observed promptly.
const POLL_SLICE: Duration = Duration::from_millis(200);

/// The URL of `camera`'s `representation` stream, if it has one.
pub fn stream_url(camera: &CameraConfig, representation: Representation) -> Option<String> {
    match representation {
        Representation::Main => Some(camera.url.clone()),
        Representation::Sub => camera.sub_url.clone(),
    }
    .filter(|url| !url.trim().is_empty())
}

/// The codec each forwarded stream was last seen in, keyed by camera and
/// representation and tied to the stream's URL, so editing a camera forgets
/// what was learnt about its old stream.
/// Per camera stream: the URL it was seen at, and its codec or the caps name
/// of a codec that cannot be forwarded.
type SeenCodecs = HashMap<(String, Representation), (String, Result<VideoCodec, String>)>;

#[derive(Default)]
pub struct StreamCodecs {
    seen: Mutex<SeenCodecs>,
}

impl StreamCodecs {
    fn record(
        &self,
        camera_id: &str,
        representation: Representation,
        url: &str,
        codec: Result<VideoCodec, String>,
    ) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(
                (camera_id.to_owned(), representation),
                (url.to_owned(), codec),
            );
        }
    }

    /// The streams of `camera` that browser live view can forward, each with
    /// its codec once seen (`None` until then). A stream with no compressed
    /// video, or one seen in a codec browsers are not sent, is left out.
    pub fn live_streams(&self, camera: &CameraConfig) -> Vec<(Representation, Option<VideoCodec>)> {
        self.live_streams_described(camera, None)
    }

    /// [`Self::live_streams`], taking the main stream's codec from what the
    /// recording connection saw (`described_main`) while no publication has
    /// opened it yet. Reported as unknown, an H.265 stream was offered to
    /// browsers and its first live view always failed (the media server
    /// accepts no H.265), with a publication built and torn down for nothing.
    pub fn live_streams_described(
        &self,
        camera: &CameraConfig,
        described_main: Option<VideoCodec>,
    ) -> Vec<(Representation, Option<VideoCodec>)> {
        let seen = self.seen.lock().ok();
        [Representation::Main, Representation::Sub]
            .into_iter()
            .filter_map(|representation| {
                let url = stream_url(camera, representation)?;
                if !encoded::forwardable(&url) {
                    return None;
                }
                let known = seen
                    .as_ref()
                    .and_then(|seen| seen.get(&(camera.id.clone(), representation)))
                    .filter(|(seen_url, _)| *seen_url == url)
                    .map(|(_, codec)| codec.clone());
                match known {
                    Some(Err(_)) => None,
                    Some(Ok(codec)) => Some((representation, Some(codec))),
                    None if representation == Representation::Main => {
                        Some((representation, described_main))
                    }
                    None => Some((representation, None)),
                }
            })
            .collect()
    }
}

pub struct CameraLiveSourceOpener {
    state: Arc<RwLock<AppState>>,
    codecs: Arc<StreamCodecs>,
}

impl CameraLiveSourceOpener {
    pub fn new(state: Arc<RwLock<AppState>>, codecs: Arc<StreamCodecs>) -> Self {
        Self { state, codecs }
    }
}

impl LiveSourceOpener for CameraLiveSourceOpener {
    fn open(
        &self,
        camera_id: &str,
        representation: Representation,
        units: Arc<EncodedSubmitter>,
    ) -> Result<Box<dyn LiveSourceRun>, PublishError> {
        if crate::streams::enabled() {
            return self.open_shared(camera_id, representation, units);
        }
        // Called from the runtime, so the config is only peeked at; the
        // source checks again on every connection.
        let mut with_audio = None;
        if let Ok(state) = self.state.try_read() {
            let camera = state
                .config
                .cameras
                .iter()
                .find(|camera| camera.id == camera_id)
                .ok_or(PublishError::StreamUnavailable)?;
            let url = stream_url(camera, representation).ok_or(PublishError::StreamUnavailable)?;
            if !encoded::forwardable(&url) {
                return Err(PublishError::UnsupportedSource);
            }
            with_audio = state
                .camera_audio(camera_id)
                .filter(|label| audio::recordable(label))
                .filter(|_| audio::live_audio_supported(camera))
                .map(|_| camera.clone());
        }
        let (audio_stop, audio_stop_rx) = watch::channel(false);
        if let Some(camera) = with_audio {
            let sink = Arc::clone(&units);
            let listener =
                LiveAudio::new(camera, audio_stop_rx).with_output(AudioOutput::Samples {
                    rate: AUDIO_RATE,
                    sink: Arc::new(move |samples| sink.submit_audio(samples)),
                });
            std::thread::Builder::new()
                .name("nw-live-audio".into())
                .spawn(move || listener.run())
                .map_err(|_| PublishError::Transport)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let source = DedicatedSource {
            state: Arc::clone(&self.state),
            codecs: Arc::clone(&self.codecs),
            camera_id: camera_id.to_owned(),
            representation,
            units,
            stop: Arc::clone(&stop),
        };
        // A plain thread, not a tokio task: GStreamer bus polling blocks, and
        // the config is read with a blocking lock outside the runtime.
        std::thread::Builder::new()
            .name(format!("nw-live-{}", representation.code()))
            .spawn(move || source.run())
            .map_err(|_| PublishError::Transport)?;
        Ok(Box::new(SourceRun(stop, audio_stop)))
    }
}

impl CameraLiveSourceOpener {
    fn open_shared(
        &self,
        camera_id: &str,
        representation: Representation,
        units: Arc<EncodedSubmitter>,
    ) -> Result<Box<dyn LiveSourceRun>, PublishError> {
        // Called from the runtime, so the config is only peeked at; the
        // source checks again whenever it subscribes.
        if let Ok(state) = self.state.try_read() {
            let camera = state
                .config
                .cameras
                .iter()
                .find(|camera| camera.id == camera_id)
                .ok_or(PublishError::StreamUnavailable)?;
            let url = stream_url(camera, representation).ok_or(PublishError::StreamUnavailable)?;
            if !encoded::forwardable(&url) {
                return Err(PublishError::UnsupportedSource);
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let source = SharedSource {
            state: Arc::clone(&self.state),
            codecs: Arc::clone(&self.codecs),
            camera_id: camera_id.to_owned(),
            representation,
            units,
            stop: Arc::clone(&stop),
        };
        // A plain thread: it waits on its subscription and reads the config
        // with a blocking lock, outside the runtime.
        std::thread::Builder::new()
            .name(format!("nw-live-{}", representation.code()))
            .spawn(move || source.run())
            .map_err(|_| PublishError::Transport)?;
        Ok(Box::new(SharedRun(stop)))
    }
}

/// Stops a source on the shared connection (its subscriptions end with it).
struct SharedRun(Arc<AtomicBool>);

impl LiveSourceRun for SharedRun {
    fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Drop for SharedRun {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How often a running source checks whether its camera stream was edited.
const CONFIG_CHECK: Duration = Duration::from_secs(2);
/// Longest wait for a unit, so a stop is observed promptly.
const RECV_SLICE: Duration = Duration::from_millis(100);

enum Forwarded {
    Stopped,
    GaveUp(PublishError),
    /// The camera's stream changed, or its connection closed: subscribe again.
    Resubscribe,
}

/// A live publication's source on the camera's shared connection.
struct SharedSource {
    state: Arc<RwLock<AppState>>,
    codecs: Arc<StreamCodecs>,
    camera_id: String,
    representation: Representation,
    units: Arc<EncodedSubmitter>,
    stop: Arc<AtomicBool>,
}

/// The publication's sound: decoded from one subscription's audio units.
#[derive(Default)]
struct Sound {
    decoder: Option<SoundDecoder>,
    failed: bool,
}

impl Sound {
    fn push(&mut self, unit: &SharedUnit, units: &Arc<EncodedSubmitter>, camera_id: &str) {
        if self
            .decoder
            .as_ref()
            .is_some_and(|d| d.caps() != &unit.caps)
        {
            self.decoder = None;
        }
        if self.decoder.is_none() {
            if self.failed {
                return;
            }
            let sink = Arc::clone(units);
            let output = AudioOutput::Samples {
                rate: AUDIO_RATE,
                sink: Arc::new(move |samples| sink.submit_audio(samples)),
            };
            match SoundDecoder::new(&unit.caps, &output) {
                Ok(decoder) => self.decoder = Some(decoder),
                Err(error) => {
                    tracing::warn!(cam_id = %camera_id, %error, "Live view sound cannot be decoded");
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(Err(error)) = self.decoder.as_mut().map(|d| d.push(unit)) {
            tracing::warn!(cam_id = %camera_id, %error, "Live view sound decoder failed");
            self.decoder = None;
        }
    }

    /// The stream restarted: its timeline starts again.
    fn reset(&mut self) {
        self.decoder = None;
    }
}

impl SharedSource {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn run(self) {
        tracing::info!(
            cam_id = %self.camera_id,
            representation = self.representation.code(),
            "Live publication source started on the shared camera connection"
        );
        while !self.stopped() {
            let (subscription, url) = match self.subscribe() {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(cam_id = %self.camera_id, %error, "Live view cannot forward this camera stream");
                    self.units.fail(error);
                    break;
                }
            };
            // A new subscription starts the viewer again at a keyframe.
            self.units.resync();
            match self.forward(&subscription, &url) {
                Forwarded::Stopped => break,
                Forwarded::GaveUp(error) => {
                    tracing::warn!(cam_id = %self.camera_id, %error, "Live view cannot forward this camera stream");
                    self.units.fail(error);
                    break;
                }
                Forwarded::Resubscribe => {}
            }
        }
        tracing::info!(
            cam_id = %self.camera_id,
            representation = self.representation.code(),
            dropped_units = self.units.dropped(),
            "Live publication source stopped"
        );
    }

    /// The camera's URL for this stream now, read fresh so edits apply.
    fn current_url(&self) -> Result<String, PublishError> {
        let state = self.state.blocking_read();
        let camera = state
            .config
            .cameras
            .iter()
            .find(|camera| camera.id == self.camera_id)
            .ok_or(PublishError::StreamUnavailable)?;
        stream_url(camera, self.representation).ok_or(PublishError::StreamUnavailable)
    }

    fn subscribe(&self) -> Result<(Subscription, String), PublishError> {
        let url = self.current_url()?;
        if !encoded::forwardable(&url) {
            return Err(PublishError::UnsupportedSource);
        }
        let subscription = self
            .state
            .blocking_read()
            .subscribe_stream(
                &self.camera_id,
                self.representation.code(),
                SubscriberPolicy::live_video(),
            )
            .map_err(|_| PublishError::StreamUnavailable)?;
        Ok((subscription, url))
    }

    /// Where the sound comes from: this stream, when it has a track the hub
    /// keeps; for the sub stream without one, the main stream's connection.
    fn sound_source(&self, subscription: &Subscription) -> (bool, Option<Subscription>) {
        let own = subscription
            .audio_label()
            .flatten()
            .is_some_and(|label| audio::recordable(&label));
        if own || self.representation != Representation::Sub {
            return (own, None);
        }
        let state = self.state.blocking_read();
        let main_has_sound = state
            .camera_audio(&self.camera_id)
            .is_some_and(|label| audio::recordable(&label));
        let main = main_has_sound
            .then(|| {
                state
                    .subscribe_stream(&self.camera_id, "main", SubscriberPolicy::live_sound())
                    .ok()
            })
            .flatten();
        (false, main)
    }

    fn forward(&self, subscription: &Subscription, url: &str) -> Forwarded {
        let mut converter: Option<(gstreamer::Caps, AnnexB)> = None;
        // Decided at the first picture, once the stream was described.
        let mut sound_from: Option<(bool, Option<Subscription>)> = None;
        let mut sound = Sound::default();
        let mut checked = Instant::now();
        loop {
            if self.stopped() {
                return Forwarded::Stopped;
            }
            if checked.elapsed() >= CONFIG_CHECK {
                checked = Instant::now();
                match self.current_url() {
                    Ok(current) if current == url => {}
                    Ok(_) => return Forwarded::Resubscribe,
                    Err(error) => return Forwarded::GaveUp(error),
                }
            }
            match subscription.recv(RECV_SLICE) {
                Some(Delivery::Unit { unit, .. }) if unit.media == Media::Video => {
                    if converter
                        .as_ref()
                        .map_or(true, |(caps, _)| *caps != unit.caps)
                    {
                        match AnnexB::from_caps(&unit.caps) {
                            Ok(found) => {
                                self.codecs.record(
                                    &self.camera_id,
                                    self.representation,
                                    url,
                                    Ok(found.codec),
                                );
                                converter = Some((unit.caps.clone(), found));
                            }
                            Err(name) => {
                                tracing::warn!(cam_id = %self.camera_id, caps = %name, "Camera stream is not H.264 or H.265");
                                self.codecs.record(
                                    &self.camera_id,
                                    self.representation,
                                    url,
                                    Err(name),
                                );
                                return Forwarded::GaveUp(PublishError::UnsupportedSource);
                            }
                        }
                    }
                    if sound_from.is_none() {
                        sound_from = Some(self.sound_source(subscription));
                    }
                    if let Some((_, convert)) = &converter {
                        if let Some(access_unit) = access_unit(convert, &unit) {
                            self.units.submit(access_unit);
                        }
                    }
                }
                Some(Delivery::Unit { unit, .. }) => {
                    if matches!(sound_from, Some((true, _))) {
                        sound.push(&unit, &self.units, &self.camera_id);
                    }
                }
                Some(Delivery::Discontinuity { .. }) => {
                    // The connection is being rebuilt; the viewer restarts at
                    // its next keyframe, the sound with its new timeline.
                    self.units.resync();
                    sound.reset();
                    converter = None;
                }
                None if subscription.state().phase == Phase::Closed => {
                    return Forwarded::Resubscribe;
                }
                None => {}
            }
            if let Some((_, Some(main))) = &sound_from {
                while let Some(item) = main.recv(Duration::ZERO) {
                    match item {
                        Delivery::Unit { unit, .. } => {
                            sound.push(&unit, &self.units, &self.camera_id)
                        }
                        Delivery::Discontinuity { .. } => sound.reset(),
                    }
                }
            }
        }
    }
}

/// A hub video unit as the bridge's access unit: Annex-B, parameter sets
/// before keyframes, its arrival time as the capture time (the camera's
/// cadence). `None` for a malformed unit or a stream without a known size.
pub(crate) fn access_unit(converter: &AnnexB, unit: &SharedUnit) -> Option<EncodedAccessUnit> {
    if converter.width == 0 || converter.height == 0 {
        return None;
    }
    let keyframe = !unit
        .buffer
        .flags()
        .contains(gstreamer::BufferFlags::DELTA_UNIT);
    let map = unit.buffer.map_readable().ok()?;
    let payload = converter.convert(&map, keyframe)?;
    Some(EncodedAccessUnit {
        codec: converter.codec,
        payload,
        keyframe,
        width: converter.width,
        height: converter.height,
        timestamp_us: unit.arrived_us,
        pts_us: unit
            .buffer
            .pts()
            .map(|pts| i64::try_from(pts.useconds()).unwrap_or(i64::MAX)),
    })
}

/// Stops the video source and its audio connection.
struct SourceRun(Arc<AtomicBool>, watch::Sender<bool>);

impl LiveSourceRun for SourceRun {
    fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
        self.1.send_replace(true);
    }
}

impl Drop for SourceRun {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Attempt {
    Stopped,
    /// The stream cannot be forwarded; retrying would not change that.
    GaveUp(PublishError),
    Failed {
        delivered_units: bool,
    },
}

struct DedicatedSource {
    state: Arc<RwLock<AppState>>,
    codecs: Arc<StreamCodecs>,
    camera_id: String,
    representation: Representation,
    units: Arc<EncodedSubmitter>,
    stop: Arc<AtomicBool>,
}

impl DedicatedSource {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn run(self) {
        tracing::info!(
            cam_id = %self.camera_id,
            representation = self.representation.code(),
            "Encoded live publication source started"
        );
        let mut failures: u32 = 0;
        loop {
            match self.attempt() {
                Attempt::Stopped => break,
                Attempt::GaveUp(error) => {
                    tracing::warn!(
                        cam_id = %self.camera_id,
                        representation = self.representation.code(),
                        %error,
                        "Live view cannot forward this camera stream"
                    );
                    self.units.fail(error);
                    break;
                }
                Attempt::Failed { delivered_units } => {
                    failures = if delivered_units { 1 } else { failures + 1 };
                }
            }
            let backoff = Duration::from_secs(1 << failures.saturating_sub(1).min(5));
            let until = Instant::now() + backoff.min(MAX_BACKOFF);
            while Instant::now() < until {
                if self.stopped() {
                    break;
                }
                std::thread::sleep(POLL_SLICE);
            }
            if self.stopped() {
                break;
            }
        }
        tracing::info!(
            cam_id = %self.camera_id,
            representation = self.representation.code(),
            dropped_units = self.units.dropped(),
            "Encoded live publication source stopped"
        );
    }

    /// The camera's current config for this stream, read fresh so camera
    /// edits apply on the next reconnect.
    fn camera(&self) -> Result<(CameraConfig, String), PublishError> {
        let mut camera = self
            .state
            .blocking_read()
            .config
            .cameras
            .iter()
            .find(|camera| camera.id == self.camera_id)
            .cloned()
            .ok_or(PublishError::StreamUnavailable)?;
        let url =
            stream_url(&camera, self.representation).ok_or(PublishError::StreamUnavailable)?;
        camera.url = url.clone();
        Ok((camera, url))
    }

    fn attempt(&self) -> Attempt {
        if self.stopped() {
            return Attempt::Stopped;
        }
        let failed = Attempt::Failed {
            delivered_units: false,
        };
        let (camera, url) = match self.camera() {
            Ok(found) => found,
            Err(error) => return Attempt::GaveUp(error),
        };
        let Some((description, live)) = encoded::source_description(&camera) else {
            return Attempt::GaveUp(PublishError::UnsupportedSource);
        };
        // A new connection starts the viewer again at its first keyframe.
        self.units.resync();
        let delivered = Arc::new(AtomicU64::new(0));
        let unsupported = Arc::new(AtomicBool::new(false));
        let outputs = {
            let (units, counter) = (Arc::clone(&self.units), Arc::clone(&delivered));
            let (codecs, camera_id, representation, url, unsupported) = (
                Arc::clone(&self.codecs),
                self.camera_id.clone(),
                self.representation,
                url.clone(),
                Arc::clone(&unsupported),
            );
            EncodedOutputs {
                unit: Arc::new(move |unit| {
                    units.submit(unit);
                    counter.fetch_add(1, Ordering::Relaxed);
                }),
                codec: Arc::new(move |codec| {
                    if let Err(caps) = &codec {
                        tracing::warn!(cam_id = %camera_id, %caps, "Camera stream is not H.264 or H.265");
                        unsupported.store(true, Ordering::Relaxed);
                    }
                    codecs.record(&camera_id, representation, &url, codec);
                }),
            }
        };
        let pipeline = match encoded::build(&description, live, outputs) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                let reason = sanitize_credentials(&error.to_string());
                tracing::warn!(cam_id = %self.camera_id, %reason, "Live publication pipeline could not be built");
                return failed;
            }
        };
        let outcome = self.watch(&pipeline, &delivered);
        let _ = pipeline.set_state(gstreamer::State::Null);
        if unsupported.load(Ordering::Relaxed) {
            return Attempt::GaveUp(PublishError::UnsupportedSource);
        }
        outcome
    }

    fn watch(&self, pipeline: &gstreamer::Pipeline, delivered: &AtomicU64) -> Attempt {
        let failed = |delivered: &AtomicU64| Attempt::Failed {
            delivered_units: delivered.load(Ordering::Relaxed) > 0,
        };
        if pipeline.set_state(gstreamer::State::Playing).is_err() {
            return failed(delivered);
        }
        let Some(bus) = pipeline.bus() else {
            return failed(delivered);
        };
        let mut last_count = 0;
        let mut last_progress = Instant::now();
        loop {
            if self.stopped() {
                return Attempt::Stopped;
            }
            let count = delivered.load(Ordering::Relaxed);
            if count != last_count {
                last_count = count;
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= NO_UNIT_TIMEOUT {
                tracing::warn!(cam_id = %self.camera_id, "Live publication source stopped delivering video");
                return failed(delivered);
            }
            let Some(message) = bus.timed_pop(gstreamer::ClockTime::from_mseconds(
                POLL_SLICE.as_millis() as u64,
            )) else {
                continue;
            };
            match message.view() {
                gstreamer::MessageView::Error(error) => {
                    let reason = sanitize_credentials(&error.error().to_string());
                    tracing::warn!(cam_id = %self.camera_id, %reason, "Live publication source failed");
                    return failed(delivered);
                }
                gstreamer::MessageView::Eos(_) => return failed(delivered),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(url: &str, sub: Option<&str>) -> CameraConfig {
        let mut camera = CameraConfig::new("Camera", url);
        camera.id = "cam-1".into();
        camera.sub_url = sub.map(str::to_owned);
        camera
    }

    #[test]
    fn sub_is_only_offered_for_a_configured_sub_stream() {
        let codecs = StreamCodecs::default();
        assert_eq!(
            codecs.live_streams(&camera("rtsp://10.0.0.5/main", None)),
            vec![(Representation::Main, None)]
        );
        assert_eq!(
            codecs.live_streams(&camera("rtsp://10.0.0.5/main", Some("rtsp://10.0.0.5/sub"))),
            vec![(Representation::Main, None), (Representation::Sub, None)]
        );
        assert!(stream_url(&camera("rtsp://x/main", Some("  ")), Representation::Sub).is_none());
    }

    #[test]
    fn sources_without_compressed_video_offer_no_live_stream() {
        let codecs = StreamCodecs::default();
        assert!(codecs
            .live_streams(&camera("http://10.0.0.5/mjpeg", None))
            .is_empty());
        assert!(codecs.live_streams(&camera("/dev/video0", None)).is_empty());
    }

    #[test]
    fn the_recordings_codec_is_reported_before_any_live_view() {
        let codecs = StreamCodecs::default();
        let cam = camera("rtsp://10.0.0.5/main", Some("rtsp://10.0.0.5/sub"));
        // Known from the recording connection: the portal will not offer an
        // H.265 main stream to a browser it cannot reach.
        assert_eq!(
            codecs.live_streams_described(&cam, Some(VideoCodec::H265)),
            vec![
                (Representation::Main, Some(VideoCodec::H265)),
                (Representation::Sub, None)
            ]
        );
        // What a publication saw wins.
        codecs.record(
            "cam-1",
            Representation::Main,
            "rtsp://10.0.0.5/main",
            Ok(VideoCodec::H264),
        );
        assert_eq!(
            codecs.live_streams_described(&cam, Some(VideoCodec::H265))[0],
            (Representation::Main, Some(VideoCodec::H264))
        );
    }

    #[test]
    fn seen_codecs_are_reported_and_forgotten_when_the_stream_changes() {
        let codecs = StreamCodecs::default();
        let mut cam = camera("rtsp://10.0.0.5/main", Some("rtsp://10.0.0.5/sub"));
        codecs.record(
            "cam-1",
            Representation::Main,
            "rtsp://10.0.0.5/main",
            Ok(VideoCodec::H265),
        );
        codecs.record(
            "cam-1",
            Representation::Sub,
            "rtsp://10.0.0.5/sub",
            Err("image/jpeg".into()),
        );
        assert_eq!(
            codecs.live_streams(&cam),
            vec![(Representation::Main, Some(VideoCodec::H265))],
            "a stream in another codec is not offered"
        );
        cam.sub_url = Some("rtsp://10.0.0.5/sub2".into());
        cam.url = "rtsp://10.0.0.6/main".into();
        assert_eq!(
            codecs.live_streams(&cam),
            vec![(Representation::Main, None), (Representation::Sub, None)]
        );
    }
}
