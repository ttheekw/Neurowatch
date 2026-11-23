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
//! The one exception: a session whose browser cannot play H.265 asks for a
//! transcode (`PublishDirective::transcode`). For an H.265 stream the video
//! then comes from the shared [`crate::streams::transcode`] (decode once per
//! camera stream, H.264 from a GPU encoder, else raw pictures the session
//! encodes as VP8); an H.264 stream is forwarded unchanged as ever.
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
use crate::streams::transcode::{TranscodeSink, TranscodeViewer};
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
            return self.open_shared(camera_id, representation, units, None);
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

    /// Transcoding needs the shared connection (one decode for every viewer
    /// of the stream); with `NW_STREAM_HUB=0` the stream is forwarded as it is.
    fn open_transcoded(
        &self,
        camera_id: &str,
        representation: Representation,
        units: Arc<EncodedSubmitter>,
        max_bitrate_kbps: u32,
    ) -> Result<Box<dyn LiveSourceRun>, PublishError> {
        if crate::streams::enabled() {
            return self.open_shared(camera_id, representation, units, Some(max_bitrate_kbps));
        }
        self.open(camera_id, representation, units)
    }
}

impl CameraLiveSourceOpener {
    fn open_shared(
        &self,
        camera_id: &str,
        representation: Representation,
        units: Arc<EncodedSubmitter>,
        transcode_kbps: Option<u32>,
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
            transcode_kbps,
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
    /// The session's browser cannot play H.265 and its grant allows this
    /// many kbit/s: an H.265 stream is transcoded for it
    /// ([`crate::streams::transcode`]), anything else forwarded unchanged.
    transcode_kbps: Option<u32>,
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
                // The cached GOP is the viewer's start, however far into it
                // the camera is: cut short, the viewer waits for the next
                // keyframe (seconds on a camera with a long GOP).
                SubscriberPolicy {
                    whole_gop: true,
                    ..SubscriberPolicy::live_video()
                },
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
                    .subscribe_stream(
                        &self.camera_id,
                        "main",
                        SubscriberPolicy {
                            consumer: crate::streams::queue::ConsumerKind::Browser,
                            ..SubscriberPolicy::live_sound()
                        },
                    )
                    .ok()
            })
            .flatten();
        (false, main)
    }

    /// Join (starting it if need be) the transcode of this camera stream,
    /// delivering to this publication.
    fn join_transcode(&self, url: &str, kbps: u32) -> Result<TranscodeViewer, String> {
        let state = self.state.blocking_read();
        let camera = state
            .config
            .cameras
            .iter()
            .find(|camera| camera.id == self.camera_id)
            .ok_or_else(|| "Camera not found".to_owned())?;
        let mut source = camera.clone();
        source.url = url.to_owned();
        let stream = match self.representation {
            Representation::Main => "main",
            Representation::Sub => "sub",
        };
        state.streams.transcode(
            &source,
            stream,
            kbps,
            Arc::clone(&self.units) as Arc<dyn TranscodeSink>,
        )
    }

    fn forward(&self, subscription: &Subscription, url: &str) -> Forwarded {
        let mut converter: Option<(gstreamer::Caps, AnnexB)> = None;
        // Decided at the first picture, once the stream was described.
        let mut sound_from: Option<(bool, Option<Subscription>)> = None;
        let mut sound = Sound::default();
        // Joined once the stream proves to be H.265 and this session asked
        // for a transcode; the video then comes from it, the sound as ever
        // from this subscription.
        let mut transcoding: Option<TranscodeViewer> = None;
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
                        .is_none_or(|(caps, _)| *caps != unit.caps)
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
                    let h265 = converter
                        .as_ref()
                        .is_some_and(|(_, convert)| convert.codec == VideoCodec::H265);
                    if let (Some(kbps), true, None) = (self.transcode_kbps, h265, &transcoding) {
                        match self.join_transcode(url, kbps) {
                            Ok(viewer) => transcoding = Some(viewer),
                            Err(error) => {
                                tracing::warn!(cam_id = %self.camera_id, %error, "Live view cannot transcode this camera stream");
                                return Forwarded::GaveUp(PublishError::UnsupportedSource);
                            }
                        }
                    }
                    if transcoding.is_some() {
                        // The transcode's own subscription carries the video.
                    } else if let Some((_, convert)) = &converter {
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
        frame: unit.frame,
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

    // ---- sources, driven without a camera: local files and empty states ----

    use crate::cloud::media_publish::{encoded_channel, EncodedFeed};
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;
    use crate::streams::testing::CameraFile;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn state_with(cameras: Vec<CameraConfig>) -> Arc<RwLock<AppState>> {
        let config = AppConfig {
            cameras,
            ..AppConfig::default()
        };
        let path = std::env::temp_dir().join(format!(
            "neurowatch-live-publish-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        Arc::new(RwLock::new(AppState::new(
            config,
            path,
            HwAccelKind::Software,
        )))
    }

    fn channel() -> (Arc<EncodedSubmitter>, EncodedFeed) {
        gstreamer::init().unwrap();
        let (submitter, feed) = encoded_channel();
        (Arc::new(submitter), feed)
    }

    fn file_camera(fixture: &CameraFile) -> CameraConfig {
        let mut camera = fixture.camera.clone();
        camera.id = "cam-1".into();
        camera
    }

    fn avcc() -> Vec<u8> {
        let sps = [0x67u8, 0x42, 0x00, 0x1e];
        let pps = [0x68u8, 0xce, 0x3c];
        let mut data = vec![1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, sps.len() as u8];
        data.extend_from_slice(&sps);
        data.extend_from_slice(&[1, 0, pps.len() as u8]);
        data.extend_from_slice(&pps);
        data
    }

    fn h264_caps(width: i32) -> gstreamer::Caps {
        gstreamer::init().unwrap();
        gstreamer::Caps::builder("video/x-h264")
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("width", width)
            .field("height", 360i32)
            .field("codec_data", gstreamer::Buffer::from_slice(avcc()))
            .build()
    }

    fn shared(caps: gstreamer::Caps, media: Media, data: Vec<u8>, delta: bool) -> SharedUnit {
        let mut buffer = gstreamer::Buffer::from_slice(data);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gstreamer::ClockTime::from_mseconds(40));
            if delta {
                buffer.set_flags(gstreamer::BufferFlags::DELTA_UNIT);
            }
        }
        SharedUnit {
            media,
            buffer,
            caps,
            arrived_us: 1_234,
            frame: None,
        }
    }

    #[test]
    fn a_hub_unit_becomes_an_annexb_access_unit_with_its_timing() {
        let caps = h264_caps(640);
        let converter = AnnexB::from_caps(&caps).unwrap();
        // One IDR slice with a 4-byte length prefix.
        let data = vec![0, 0, 0, 3, 0x65, 0x88, 0x84];
        let key = access_unit(&converter, &shared(caps.clone(), Media::Video, data, false))
            .expect("a keyframe converts");
        assert!(key.keyframe);
        assert_eq!((key.width, key.height), (640, 360));
        assert_eq!(key.codec, VideoCodec::H264);
        assert_eq!(key.timestamp_us, 1_234);
        assert_eq!(key.pts_us, Some(40_000));
        // Parameter sets come first, then the slice.
        assert_eq!(&key.payload[..5], &[0, 0, 0, 1, 0x67]);
        assert!(key.payload.ends_with(&[0, 0, 0, 1, 0x65, 0x88, 0x84]));

        let data = vec![0, 0, 0, 3, 0x41, 0x9a, 0x02];
        let delta = access_unit(&converter, &shared(caps, Media::Video, data, true)).unwrap();
        assert!(!delta.keyframe);
        assert_eq!(delta.payload, vec![0, 0, 0, 1, 0x41, 0x9a, 0x02]);
    }

    #[test]
    fn a_malformed_unit_or_an_unsized_stream_gives_no_access_unit() {
        let caps = h264_caps(640);
        let converter = AnnexB::from_caps(&caps).unwrap();
        // The length prefix claims more than there is.
        let torn = shared(caps, Media::Video, vec![0, 0, 0, 9, 0x65], false);
        assert!(access_unit(&converter, &torn).is_none());

        let unsized_caps = h264_caps(0);
        let converter = AnnexB::from_caps(&unsized_caps).unwrap();
        let unit = shared(
            unsized_caps,
            Media::Video,
            vec![0, 0, 0, 3, 0x65, 0x88, 0x84],
            false,
        );
        assert!(access_unit(&converter, &unit).is_none());
    }

    #[test]
    fn source_runs_stop_by_flag_and_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        let shared_run = SharedRun(Arc::clone(&flag));
        shared_run.stop();
        assert!(flag.load(Ordering::Relaxed));

        let flag = Arc::new(AtomicBool::new(false));
        drop(SharedRun(Arc::clone(&flag)));
        assert!(flag.load(Ordering::Relaxed), "dropping stops the source");

        let flag = Arc::new(AtomicBool::new(false));
        let (audio_stop, audio_rx) = watch::channel(false);
        drop(SourceRun(Arc::clone(&flag), audio_stop));
        assert!(flag.load(Ordering::Relaxed));
        assert!(*audio_rx.borrow(), "the audio connection is told to stop");
    }

    #[test]
    fn sound_in_a_codec_nobody_reads_is_given_up_on_once() {
        gstreamer::init().unwrap();
        let (units, _feed) = channel();
        let caps = gstreamer::Caps::builder("audio/x-nw-unknown").build();
        let unit = shared(caps, Media::Audio, vec![0; 16], false);
        let mut sound = Sound::default();
        sound.push(&unit, &units, "cam-1");
        assert!(sound.failed);
        assert!(sound.decoder.is_none());
        // Not retried for every unit.
        sound.push(&unit, &units, "cam-1");
        assert!(sound.failed);
        sound.reset();
        assert!(sound.decoder.is_none());
    }

    #[test]
    fn camera_sound_is_decoded_into_the_publications_audio() {
        gstreamer::init().unwrap();
        let (units, feed) = channel();
        let mut audio = feed.take_audio().unwrap();
        let caps = gstreamer::Caps::builder("audio/x-mulaw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build();
        let mut sound = Sound::default();
        let mut got = false;
        let rt = runtime();
        for index in 0..50u64 {
            let mut unit = shared(caps.clone(), Media::Audio, vec![0xff; 160], false);
            unit.buffer
                .get_mut()
                .unwrap()
                .set_pts(gstreamer::ClockTime::from_mseconds(index * 20));
            sound.push(&unit, &units, "cam-1");
            if let Ok(Some(samples)) = rt.block_on(async {
                tokio::time::timeout(Duration::from_millis(100), audio.recv()).await
            }) {
                assert!(!samples.is_empty());
                got = true;
                break;
            }
        }
        assert!(got, "decoded samples reach the audio queue");
        assert!(sound.decoder.is_some());
        // A change of caps rebuilds the decoder; a restart drops it.
        let other = gstreamer::Caps::builder("audio/x-alaw")
            .field("rate", 8000i32)
            .field("channels", 1i32)
            .build();
        sound.push(
            &shared(other, Media::Audio, vec![0xd5; 160], false),
            &units,
            "cam-1",
        );
        assert!(sound.decoder.is_some());
        sound.reset();
        assert!(sound.decoder.is_none());
    }

    #[test]
    fn opening_a_stream_the_camera_does_not_have_is_refused() {
        let mut sub_less = camera("rtsp://127.0.0.1:9/main", None);
        sub_less.id = "cam-1".into();
        let mut mjpeg = camera("http://127.0.0.1:9/mjpeg", None);
        mjpeg.id = "cam-2".into();
        let state = state_with(vec![sub_less, mjpeg]);
        let opener = CameraLiveSourceOpener::new(state, Arc::new(StreamCodecs::default()));
        let (units, _feed) = channel();
        let refused = |camera: &str, representation| {
            opener
                .open(camera, representation, Arc::clone(&units))
                .err()
        };
        assert_eq!(
            refused("missing", Representation::Main),
            Some(PublishError::StreamUnavailable)
        );
        assert_eq!(
            refused("cam-1", Representation::Sub),
            Some(PublishError::StreamUnavailable)
        );
        assert_eq!(
            refused("cam-2", Representation::Main),
            Some(PublishError::UnsupportedSource)
        );
    }

    #[test]
    fn a_source_for_a_camera_that_vanished_fails_its_publication() {
        let state = state_with(Vec::new());
        let (units, feed) = channel();
        let source = SharedSource {
            state,
            codecs: Arc::new(StreamCodecs::default()),
            camera_id: "gone".into(),
            representation: Representation::Main,
            units,
            transcode_kbps: None,
            stop: Arc::new(AtomicBool::new(false)),
        };
        source.run();
        assert_eq!(feed.failure(), Some(PublishError::StreamUnavailable));
    }

    #[test]
    fn a_source_for_an_unforwardable_stream_fails_its_publication() {
        let mut mjpeg = camera("http://127.0.0.1:9/mjpeg", None);
        mjpeg.id = "cam-1".into();
        let (units, feed) = channel();
        let source = SharedSource {
            state: state_with(vec![mjpeg]),
            codecs: Arc::new(StreamCodecs::default()),
            camera_id: "cam-1".into(),
            representation: Representation::Main,
            units,
            transcode_kbps: None,
            stop: Arc::new(AtomicBool::new(false)),
        };
        source.run();
        assert_eq!(feed.failure(), Some(PublishError::UnsupportedSource));
    }

    #[test]
    fn a_stopped_source_forwards_nothing() {
        let (units, feed) = channel();
        let source = SharedSource {
            state: state_with(Vec::new()),
            codecs: Arc::new(StreamCodecs::default()),
            camera_id: "cam-1".into(),
            representation: Representation::Main,
            units,
            transcode_kbps: None,
            stop: Arc::new(AtomicBool::new(true)),
        };
        source.run();
        assert_eq!(feed.failure(), None);
    }

    #[test]
    fn the_shared_connection_feeds_a_publication_and_records_the_codec() {
        let fixture = CameraFile::new(250);
        let camera = file_camera(&fixture);
        let state = state_with(vec![camera.clone()]);
        let codecs = Arc::new(StreamCodecs::default());
        let opener = CameraLiveSourceOpener::new(state, Arc::clone(&codecs));
        let (units, feed) = channel();
        let run = opener
            .open("cam-1", Representation::Main, Arc::clone(&units))
            .unwrap_or_else(|_| panic!("a file stream can be forwarded"));
        let rt = runtime();
        let first = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(20), feed.next()).await })
            .expect("a picture arrives")
            .expect("the feed is open");
        assert!(first.keyframe, "a viewer starts at a keyframe");
        assert_eq!(first.codec, VideoCodec::H264);
        assert_eq!((first.width, first.height), (320, 240));
        assert!(first.payload.starts_with(&[0, 0, 0, 1]));
        assert_eq!(
            codecs.live_streams(&camera),
            vec![(Representation::Main, Some(VideoCodec::H264))]
        );
        run.stop();
    }

    #[test]
    fn an_h264_stream_is_forwarded_unchanged_even_when_a_transcode_was_asked_for() {
        let fixture = CameraFile::new(250);
        let state = state_with(vec![file_camera(&fixture)]);
        let opener = CameraLiveSourceOpener::new(Arc::clone(&state), Arc::default());
        let (units, feed) = channel();
        let run = opener
            .open_transcoded("cam-1", Representation::Main, Arc::clone(&units), 2_000)
            .unwrap_or_else(|_| panic!("a file stream can be forwarded"));
        let first = runtime()
            .block_on(async { tokio::time::timeout(Duration::from_secs(20), feed.next()).await })
            .expect("a picture arrives")
            .expect("the feed is open");
        assert_eq!(first.codec, VideoCodec::H264);
        assert!(first.keyframe);
        assert!(
            state.try_read().unwrap().streams.transcodes().is_empty(),
            "nothing is decoded for an H.264 stream"
        );
        run.stop();
    }

    #[test]
    fn two_sessions_that_asked_share_one_transcode_of_an_h265_stream() {
        let Some(fixture) = CameraFile::h265(400, 640, 360) else {
            eprintln!("skipped: no x265enc to make an H.265 camera");
            return;
        };
        let state = state_with(vec![file_camera(&fixture)]);
        let opener = CameraLiveSourceOpener::new(Arc::clone(&state), Arc::default());
        let (first_units, first_feed) = channel();
        let (second_units, second_feed) = channel();
        let first_run = opener
            .open_transcoded("cam-1", Representation::Main, first_units, 2_000)
            .unwrap_or_else(|_| panic!("a file stream can be transcoded"));
        let second_run = opener
            .open_transcoded("cam-1", Representation::Main, second_units, 800)
            .unwrap_or_else(|_| panic!("a file stream can be transcoded"));
        let rt = runtime();
        let wait = |feed: &EncodedFeed| {
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(40), async {
                    tokio::select! {
                        unit = feed.next() => unit.map(Some),
                        _ = feed.next_picture() => Some(None),
                    }
                })
                .await
            })
            .expect("the transcode delivers")
            .expect("the feed is open")
        };
        for feed in [&first_feed, &second_feed] {
            // An encoder: H.264 at the camera's size, from a keyframe; with
            // none here, pictures for the CPU path instead.
            if let Some(unit) = wait(feed) {
                assert_eq!(unit.codec, VideoCodec::H264);
                assert!(unit.keyframe);
                assert_eq!((unit.width, unit.height), (640, 360));
            }
        }
        // Listed once its decoder is named (a moment after the first picture).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut listed = state.try_read().unwrap().streams.transcodes();
        while listed.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            listed = state.try_read().unwrap().streams.transcodes();
        }
        assert_eq!(listed.len(), 1, "one transcode: {listed:?}");
        assert_eq!(listed[0].viewers, 2);
        assert_eq!((listed[0].from, listed[0].stream), ("h265", "main"));
        first_run.stop();
        second_run.stop();
        // Both sessions ended: the transcode stops with them.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !state.try_read().unwrap().streams.transcodes().is_empty()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(state.try_read().unwrap().streams.transcodes().is_empty());
    }

    #[test]
    fn a_dedicated_source_forwards_a_file_until_it_ends() {
        let fixture = CameraFile::new(25);
        let camera = file_camera(&fixture);
        let codecs = Arc::new(StreamCodecs::default());
        let (units, feed) = channel();
        let source = DedicatedSource {
            state: state_with(vec![camera.clone()]),
            codecs: Arc::clone(&codecs),
            camera_id: "cam-1".into(),
            representation: Representation::Main,
            units,
            stop: Arc::new(AtomicBool::new(false)),
        };
        let (found, url) = source.camera().unwrap();
        assert_eq!(url, camera.url);
        assert_eq!(found.url, url);
        let outcome = source.attempt();
        assert!(
            matches!(
                outcome,
                Attempt::Failed {
                    delivered_units: true
                }
            ),
            "the end of the file is a failed connection that delivered"
        );
        assert_eq!(
            codecs.live_streams(&camera),
            vec![(Representation::Main, Some(VideoCodec::H264))]
        );
        let rt = runtime();
        let first = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), feed.next()).await })
            .unwrap()
            .unwrap();
        assert!(first.keyframe);
    }

    #[test]
    fn a_dedicated_source_stops_or_gives_up_without_retrying() {
        let stopped = DedicatedSource {
            state: state_with(Vec::new()),
            codecs: Arc::new(StreamCodecs::default()),
            camera_id: "cam-1".into(),
            representation: Representation::Main,
            units: channel().0,
            stop: Arc::new(AtomicBool::new(true)),
        };
        assert!(matches!(stopped.attempt(), Attempt::Stopped));
        stopped.run();

        let (units, feed) = channel();
        let gone = DedicatedSource {
            state: state_with(Vec::new()),
            codecs: Arc::new(StreamCodecs::default()),
            camera_id: "gone".into(),
            representation: Representation::Sub,
            units,
            stop: Arc::new(AtomicBool::new(false)),
        };
        assert!(matches!(
            gone.attempt(),
            Attempt::GaveUp(PublishError::StreamUnavailable)
        ));
        gone.run();
        assert_eq!(feed.failure(), Some(PublishError::StreamUnavailable));

        // A source that is no compressed stream at all.
        let mut mjpeg = camera("http://127.0.0.1:9/mjpeg", None);
        mjpeg.id = "cam-1".into();
        let (units, feed) = channel();
        let unsupported = DedicatedSource {
            state: state_with(vec![mjpeg]),
            codecs: Arc::new(StreamCodecs::default()),
            camera_id: "cam-1".into(),
            representation: Representation::Main,
            units,
            stop: Arc::new(AtomicBool::new(false)),
        };
        unsupported.run();
        assert_eq!(feed.failure(), Some(PublishError::UnsupportedSource));
    }
}
