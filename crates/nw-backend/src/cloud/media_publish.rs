//! Node-side LiveKit publication boundary.
//!
//! Authorization is decided here before any transport exists: a directive
//! (pulled over the node's authenticated cloud link) is validated, its grant's
//! scope matched against the node it is linked as, its signalling URL against
//! the link's endpoint policy, and the grant spent once. Only then is a
//! [`MediaBridge`] invoked, joining the room with the publisher token the
//! control plane minted for that directive. The node holds no SFU secret.
//!
//! Live view uses [`EncodedMediaBridge`]: it forwards the camera's own H.264
//! or H.265 access units into a pre-encoded LiveKit track, so the node neither
//! decodes nor encodes; the browser decodes. Units reach it through a bounded
//! channel ([`EncodedSubmitter`] / [`EncodedFeed`]) that, when the uplink falls
//! behind, drops and then resumes at the next keyframe, never mid-GOP.
//!
//! Recorded playback still uses [`LiveKitMediaBridge`]: it connects a room,
//! publishes a native video track and pumps decoded I420 frames into it
//! through a one-frame mailbox ([`FrameSubmitter`] / [`BoundedFrameSource`]).
//! Moving playback to encoded delivery is later work (see
//! `docs/plans/2026-10-04-encoded-video-handling.md`).
//!
//! In both, the submitting side never awaits and never blocks, so a GStreamer
//! streaming thread hands its data over or drops it; congestion on the uplink
//! can never backpressure capture or recording.
//!
//! Camera sound travels beside the video: decoded audio (mono,
//! [`AUDIO_RATE`] Hz) handed to [`EncodedSubmitter::submit_audio`] is
//! published, once the first of it arrives, as an audio track the LiveKit
//! SDK encodes to Opus. Audio is the one thing on this path that is decoded
//! and encoded (browsers cannot play G.711 or AAC from WebRTC); it is small
//! next to video, and a camera without audio publishes no audio track.
//!
//! Talking back: in a live room, a viewer allowed to (the portal gives camera
//! operators a token that may publish a microphone, and nothing else) can
//! publish an audio track named [`TALK_TRACK`]. The node subscribes to that
//! track only, decodes it to PCM and hands it to a [`TalkSink`], which plays
//! it on the camera's speaker (`crate::talk`). Unpublishing it, leaving,
//! or the session ending stops talking. A refusal (another talker holds the
//! speaker, the camera has none) goes back to that viewer alone on
//! [`TALK_TOPIC`].
//!
//! Nothing here runs until a linked cloud sends a directive: local capture,
//! recording and playback never depend on it.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use livekit::options::{
    TrackPublishOptions, VideoCodec as TrackCodec, VideoEncoderBackend, VideoEncoding,
};
use livekit::prelude::{
    DataPacket, LocalAudioTrack, LocalTrack, LocalVideoTrack, RemoteTrack, Room, RoomEvent,
    RoomOptions, TrackKind, TrackSource,
};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::{
    native::NativeAudioSource, AudioSourceOptions, RtcAudioSource,
};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame};
use livekit::webrtc::video_source::{native::NativeVideoSource, RtcVideoSource, VideoResolution};
use nw_cloud_protocol::media_session::{
    MediaAction, MediaGrant, PlaybackControl, PublishDirective, Representation,
    PLAYBACK_SPEEDS_PERCENT,
};
use nw_core::encoded::{EncodedAccessUnit, VideoCodec};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PublishError {
    #[error("LiveKit publishing is not configured")]
    Unconfigured,
    #[error("the publish directive is invalid")]
    InvalidDirective,
    #[error("the media grant is expired or not yet valid")]
    Expired,
    #[error("the media grant does not authorize this node/session")]
    ScopeMismatch,
    #[error("the media grant was already used by this node")]
    Replay,
    #[error("the control plane refused grant consumption")]
    ConsumptionDenied,
    #[error("the encoded GStreamer-to-LiveKit bridge is not implemented")]
    MediaBridgeUnavailable,
    #[error("the LiveKit room could not be joined or the track could not be published")]
    Transport,
    #[error("a submitted frame is not a well-formed I420 frame for this track")]
    InvalidFrame,
    #[error("there is no local recording at the requested playback time")]
    NoRecording,
    #[error("this camera stream cannot be forwarded to a browser without transcoding")]
    UnsupportedSource,
    #[error("the camera has no such stream")]
    StreamUnavailable,
    #[error("the camera stream changed codec during the publication")]
    CodecChanged,
    #[error("the media server does not accept this camera's codec")]
    CodecRefused,
}

/// Where one publication goes. The node holds no SFU secret and needs no
/// media configuration: the region, the signalling URL and the access token
/// all come from the control plane's directive, over the node's authenticated
/// link. The node itself only decides which signalling URLs it will connect
/// to ([`EndpointPolicy`](nw_cloud_protocol::media_session::EndpointPolicy)).
#[derive(Clone)]
pub struct LiveKitPublisherConfig {
    pub enabled: bool,
    /// The cloud node this installation is linked as; a grant must name it.
    pub node_id: Option<String>,
    pub region_id: String,
    pub signaling_url: String,
    /// Which signalling URLs a directive may name; loopback only under the
    /// explicit `NW_CLOUD_DEV_ALLOW_LOOPBACK_HTTP=1` development opt-in.
    pub endpoint_policy: nw_cloud_protocol::media_session::EndpointPolicy,
}

impl LiveKitPublisherConfig {
    /// One publication's destination, as its directive names it.
    pub fn for_directive(
        directive: &PublishDirective,
        node_id: String,
        endpoint_policy: nw_cloud_protocol::media_session::EndpointPolicy,
    ) -> Self {
        Self {
            enabled: true,
            node_id: Some(node_id),
            region_id: directive.sfu.region_id.clone(),
            signaling_url: directive.sfu.signaling_url.clone(),
            endpoint_policy,
        }
    }
}

/// Atomically spends the opaque NeuroWatch grant at the control plane. A real
/// implementation posts to `/v1/media/grant-consumptions` over the existing
/// authenticated outbound cloud channel.
pub trait GrantConsumer: Send + Sync {
    fn consume<'a>(&'a self, grant: &'a str) -> BoxFuture<'a, Result<(), PublishError>>;
}

/// Production HTTP grant consumer. Its bearer is a node-local adapter
/// credential; it is never taken from `PublishDirective` or exposed to a
/// browser. Construction rejects non-HTTPS and site-local endpoints.
pub struct HttpGrantConsumer {
    client: reqwest::Client,
    url: reqwest::Url,
    bearer: String,
}

impl HttpGrantConsumer {
    pub fn new(base_url: &str, bearer: String) -> Result<Self, PublishError> {
        if bearer.len() < 32 {
            return Err(PublishError::Unconfigured);
        }
        let base = reqwest::Url::parse(base_url).map_err(|_| PublishError::Unconfigured)?;
        let trust_probe = nw_cloud_protocol::media_session::SfuEndpoint {
            region_id: "control-plane".into(),
            signaling_url: base_url.into(),
            ice_servers: vec![],
        };
        if base.scheme() != "https" || trust_probe.validate().is_err() {
            return Err(PublishError::Unconfigured);
        }
        let url = base
            .join("v1/media/grant-consumptions")
            .map_err(|_| PublishError::Unconfigured)?;
        Ok(Self {
            client: reqwest::Client::new(),
            url,
            bearer,
        })
    }
}

impl GrantConsumer for HttpGrantConsumer {
    fn consume<'a>(&'a self, grant: &'a str) -> BoxFuture<'a, Result<(), PublishError>> {
        Box::pin(async move {
            let response = self
                .client
                .post(self.url.clone())
                .bearer_auth(&self.bearer)
                .json(&serde_json::json!({"grant": grant}))
                .send()
                .await
                .map_err(|_| PublishError::ConsumptionDenied)?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(PublishError::ConsumptionDenied)
            }
        })
    }
}

/// Exact remaining media integration boundary. An implementation must source
/// the authorized camera/representation from the existing GStreamer pipeline,
/// preserve encoded access-unit ordering and timestamps, packetize it for a
/// LiveKit native video track, and make disconnect cancellation-safe without
/// backpressuring recording.
pub trait MediaBridge: Send + Sync {
    fn publish<'a>(
        &'a self,
        config: &'a LiveKitPublisherConfig,
        grant: &'a MediaGrant,
        participant_token: &'a str,
    ) -> BoxFuture<'a, Result<(), PublishError>>;
}

/// Fallback bridge: authorizes exactly as usual and then publishes nothing.
/// Kept for builds and deployments with no uplink, and as the default wiring.
pub struct UnavailableMediaBridge;
impl MediaBridge for UnavailableMediaBridge {
    fn publish<'a>(
        &'a self,
        _: &'a LiveKitPublisherConfig,
        _: &'a MediaGrant,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), PublishError>> {
        Box::pin(async { Err(PublishError::MediaBridgeUnavailable) })
    }
}

// ---------------------------------------------------------------------------
// Encoded access units
// ---------------------------------------------------------------------------

/// What happened to an access unit offered to the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    Queued,
    /// The queue was full (or is waiting for a keyframe): dropped.
    DroppedQueueFull,
    /// Nothing is publishing any more.
    Closed,
}

/// Most access units waiting for the uplink: about two seconds of video.
const ENCODED_QUEUE_UNITS: usize = 64;
/// Most bytes waiting for the uplink, whatever the camera's bitrate.
const ENCODED_QUEUE_BYTES: usize = 8 * 1024 * 1024;
/// Sample rate of the audio handed to [`EncodedSubmitter::submit_audio`] (mono).
pub const AUDIO_RATE: u32 = 48_000;
/// Most audio chunks waiting for the audio track; more are dropped, so a
/// slow uplink cannot make the sound fall further and further behind.
const AUDIO_QUEUE_CHUNKS: usize = 32;
/// The audio track's own buffer: it takes samples at real time.
const AUDIO_SOURCE_QUEUE_MS: u32 = 50;

/// Producer half of the encoded hand-off, called from a GStreamer thread.
///
/// Compressed pictures depend on earlier ones, so this is a bounded queue,
/// not a latest-value mailbox: when it is full the unit is dropped and every
/// unit after it too, until the next keyframe, where a viewer can resume.
pub struct EncodedSubmitter {
    sender: mpsc::Sender<EncodedAccessUnit>,
    queued_bytes: std::sync::Arc<AtomicUsize>,
    awaiting_keyframe: AtomicBool,
    dropped: AtomicU64,
    /// Capture timestamp of the last unit queued, or `i64::MIN`.
    last_timestamp_us: AtomicI64,
    failure: watch::Sender<Option<PublishError>>,
    audio: mpsc::Sender<Vec<i16>>,
}

impl EncodedSubmitter {
    /// Capture timestamp of the last unit queued, if any: a new source
    /// (a recording sought) goes on from it.
    pub fn last_timestamp_us(&self) -> Option<i64> {
        let last = self.last_timestamp_us.load(Ordering::Relaxed);
        (last != i64::MIN).then_some(last)
    }

    /// Hands decoded audio (mono, [`AUDIO_RATE`] Hz) to the publication's
    /// audio track. Never blocks: dropped when the track is behind or gone.
    pub fn submit_audio(&self, samples: Vec<i16>) {
        if !samples.is_empty() {
            let _ = self.audio.try_send(samples);
        }
    }

    pub fn submit(&self, unit: EncodedAccessUnit) -> SubmitOutcome {
        if self.sender.is_closed() {
            return SubmitOutcome::Closed;
        }
        if self.awaiting_keyframe.load(Ordering::Relaxed) && !unit.keyframe {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return SubmitOutcome::DroppedQueueFull;
        }
        let size = unit.payload.len();
        if self.queued_bytes.load(Ordering::Relaxed) + size > ENCODED_QUEUE_BYTES {
            return self.drop_until_keyframe();
        }
        let timestamp_us = unit.timestamp_us;
        match self.sender.try_send(unit) {
            Ok(()) => {
                self.last_timestamp_us
                    .store(timestamp_us, Ordering::Relaxed);
                self.queued_bytes.fetch_add(size, Ordering::Relaxed);
                self.awaiting_keyframe.store(false, Ordering::Relaxed);
                SubmitOutcome::Queued
            }
            Err(mpsc::error::TrySendError::Full(_)) => self.drop_until_keyframe(),
            Err(mpsc::error::TrySendError::Closed(_)) => SubmitOutcome::Closed,
        }
    }

    /// Resume at the next keyframe, e.g. after the camera connection was rebuilt.
    pub fn resync(&self) {
        self.awaiting_keyframe.store(true, Ordering::Relaxed);
    }

    /// End the publication with `error`: the stream cannot be forwarded.
    pub fn fail(&self, error: PublishError) {
        self.failure.send_replace(Some(error));
    }

    fn drop_until_keyframe(&self) -> SubmitOutcome {
        self.awaiting_keyframe.store(true, Ordering::Relaxed);
        self.dropped.fetch_add(1, Ordering::Relaxed);
        SubmitOutcome::DroppedQueueFull
    }

    /// Units discarded because the uplink was behind, since construction.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Consumer half of the encoded hand-off.
pub struct EncodedFeed {
    receiver: AsyncMutex<mpsc::Receiver<EncodedAccessUnit>>,
    queued_bytes: std::sync::Arc<AtomicUsize>,
    failure: watch::Receiver<Option<PublishError>>,
    audio: Mutex<Option<mpsc::Receiver<Vec<i16>>>>,
}

impl EncodedFeed {
    /// The audio handed to [`EncodedSubmitter::submit_audio`]. Taken once.
    pub fn take_audio(&self) -> Option<mpsc::Receiver<Vec<i16>>> {
        self.audio.lock().ok()?.take()
    }

    /// The next unit, or `None` once the source is gone or has failed.
    pub async fn next(&self) -> Option<EncodedAccessUnit> {
        let mut failure = self.failure.clone();
        if failure.borrow_and_update().is_some() {
            return None;
        }
        let mut receiver = self.receiver.lock().await;
        let unit = tokio::select! {
            unit = receiver.recv() => unit?,
            _ = failure.changed() => return None,
        };
        self.queued_bytes
            .fetch_sub(unit.payload.len(), Ordering::Relaxed);
        Some(unit)
    }

    /// Why the source gave up, if it did.
    pub fn failure(&self) -> Option<PublishError> {
        self.failure.borrow().clone()
    }
}

/// The bounded encoded hand-off; it starts by waiting for a keyframe.
pub fn encoded_channel() -> (EncodedSubmitter, EncodedFeed) {
    let (sender, receiver) = mpsc::channel(ENCODED_QUEUE_UNITS);
    let queued_bytes = std::sync::Arc::new(AtomicUsize::new(0));
    let (failure, failed) = watch::channel(None);
    let (audio, audio_rx) = mpsc::channel(AUDIO_QUEUE_CHUNKS);
    (
        EncodedSubmitter {
            sender,
            queued_bytes: std::sync::Arc::clone(&queued_bytes),
            awaiting_keyframe: AtomicBool::new(true),
            dropped: AtomicU64::new(0),
            last_timestamp_us: AtomicI64::new(i64::MIN),
            failure,
            audio,
        },
        EncodedFeed {
            receiver: AsyncMutex::new(receiver),
            queued_bytes,
            failure: failed,
            audio: Mutex::new(Some(audio_rx)),
        },
    )
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A live, published video track. Obtaining one is proof that the room was
/// joined and the track accepted.
pub trait PublishedVideoTrack: Send + Sync {
    /// Hands one compressed access unit to the pre-encoded track, unchanged.
    fn capture_encoded(&self, unit: &EncodedAccessUnit) -> Result<(), PublishError>;

    /// Whether a viewer asked for a keyframe (PLI/FIR) since the last call.
    fn take_keyframe_request(&self) -> bool {
        false
    }

    /// Whether a viewer has subscribed to this track; `None` for a
    /// transport that cannot tell, which then sends from the first unit.
    /// Pictures are held until one has (see [`AWAIT_VIEWER_LIMIT`]).
    fn viewer_attached(&self) -> Option<bool> {
        None
    }

    /// The codec this track is actually sent in (`video/h264`, …), once
    /// negotiated; `None` until then.
    fn negotiated_codec(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }

    /// Unpublishes, disconnects and drops every transport resource. Idempotent.
    fn close(&self) -> BoxFuture<'_, ()>;

    /// Messages viewers sent on [`PLAYBACK_TOPIC`]. Taken once; `None` for a
    /// transport without a data channel.
    fn take_messages(&self) -> Option<mpsc::Receiver<ViewerMessage>> {
        None
    }

    /// Send a reliable data message on [`PLAYBACK_TOPIC`] to the room.
    fn send_data(&self, _payload: Vec<u8>) -> BoxFuture<'_, Result<(), PublishError>> {
        Box::pin(async { Ok(()) })
    }

    /// Publish an audio track beside the video, for the camera's decoded
    /// sound. `None` for a transport without audio.
    fn publish_audio(
        &self,
    ) -> BoxFuture<'_, Result<Option<Box<dyn PublishedAudioTrack>>, PublishError>> {
        Box::pin(async { Ok(None) })
    }
}

/// A published audio track, fed mono [`AUDIO_RATE`] Hz samples.
pub trait PublishedAudioTrack: Send + Sync {
    /// Queue `samples`; waits while the track's buffer is full (real time).
    fn capture<'a>(&'a self, samples: &'a [i16]) -> BoxFuture<'a, Result<(), PublishError>>;
}

/// The LiveKit data topic of playback control and state.
pub const PLAYBACK_TOPIC: &str = "nw.playback.v1";
/// Largest data message accepted from a viewer.
const MAX_VIEWER_MESSAGE_BYTES: usize = 1024;
/// Viewer messages waiting to be handled; more are dropped.
const VIEWER_MESSAGE_QUEUE: usize = 32;

/// A data message on the playback topic. `identity` is the sender's
/// participant identity when LiveKit resolved it, else empty.
///
/// Who may send is enforced by the LiveKit server, not by this node: a
/// playback room is one viewer's session, only tokens signed with the
/// LiveKit key (held by the control plane and this node) can join it, and
/// only participants granted `canPublishData` — the web portal grants it to
/// playback viewers only — can send data at all. The server does not always
/// attach the sender (this node holds no subscribe permission, so it does
/// not track other participants), so an unresolved sender is accepted and a
/// resolved one must be a viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerMessage {
    pub identity: String,
    pub payload: Vec<u8>,
}

/// May this sender steer playback? The web portal names every viewer token
/// `viewer:{user}:{viewer}`; an empty identity is an unresolved sender.
/// The name of a viewer's microphone track that talks through the camera.
pub const TALK_TRACK: &str = "talk";
/// Data topic telling a viewer why its talk track is not played:
/// `{"refused": "<reason>"}`, sent to that viewer only.
pub const TALK_TOPIC: &str = "nw.talk.v1";
/// Longest refusal reason sent to a viewer.
const MAX_TALK_REASON: usize = 300;
/// Talk renewals while sound keeps arriving (inside the talk lease).
const TALK_RENEW: std::time::Duration = std::time::Duration::from_secs(1);

/// Plays a viewer's voice on a camera's speaker (two-way audio).
pub trait TalkSink: Send + Sync {
    /// Start talking through `camera_id` as `speaker` (a viewer identity);
    /// the sender takes mono 16-bit PCM at [`AUDIO_RATE`] Hz.
    fn start(
        &self,
        camera_id: String,
        speaker: String,
    ) -> BoxFuture<'static, Result<std::sync::mpsc::Sender<Vec<i16>>, String>>;
    fn renew(&self, speaker: &str);
    fn stop(&self, speaker: &str);
}

/// The [`TALK_TOPIC`] message telling a viewer why it cannot talk.
fn talk_refusal(reason: &str) -> Vec<u8> {
    let reason: String = reason.chars().take(MAX_TALK_REASON).collect();
    serde_json::json!({ "refused": reason })
        .to_string()
        .into_bytes()
}

/// Feed a viewer's talk track to the camera's speaker until it ends.
async fn relay_talk(
    sink: std::sync::Arc<dyn TalkSink>,
    camera_id: String,
    speaker: String,
    track: livekit::prelude::RemoteAudioTrack,
    room: livekit::prelude::LocalParticipant,
) {
    use futures_util::StreamExt as _;
    let samples = match sink.start(camera_id.clone(), speaker.clone()).await {
        Ok(samples) => samples,
        Err(error) => {
            tracing::warn!(camera = %camera_id, %error, "talk refused");
            let _ = room
                .publish_data(DataPacket {
                    payload: talk_refusal(&error),
                    topic: Some(TALK_TOPIC.to_owned()),
                    reliable: true,
                    destination_identities: vec![speaker.clone().into()],
                })
                .await;
            return;
        }
    };
    tracing::info!(camera = %camera_id, "a viewer talks through the camera");
    let mut stream = NativeAudioStream::new(track.rtc_track(), AUDIO_RATE as i32, 1);
    let mut renewed = std::time::Instant::now();
    while let Some(frame) = stream.next().await {
        if samples.send(frame.data.to_vec()).is_err() {
            break;
        }
        if renewed.elapsed() >= TALK_RENEW {
            sink.renew(&speaker);
            renewed = std::time::Instant::now();
        }
    }
    drop(samples);
    sink.stop(&speaker);
    tracing::info!(camera = %camera_id, "the viewer stopped talking");
}

fn is_viewer(identity: &str) -> bool {
    identity.is_empty() || (identity.starts_with("viewer:") && identity.len() <= 200)
}

/// Handles a session's data channel: viewer messages, and a state report
/// sent to the room every second and after each message.
pub trait DataControl: Send + Sync {
    /// Act on a viewer's message.
    fn on_message<'a>(&'a self, identity: &'a str, payload: &'a [u8]) -> BoxFuture<'a, ()>;
    /// The state to report now, if any.
    fn report(&self) -> BoxFuture<'_, Option<Vec<u8>>>;
}

/// Opens the transport. A trait so the bridges' pumps can be exercised
/// against a fake that touches no network.
pub trait RoomConnector: Send + Sync {
    /// Publish a pre-encoded track that forwards `codec` access units as they are.
    fn connect_and_publish<'a>(
        &'a self,
        signaling_url: &'a str,
        participant_token: &'a str,
        grant: &'a MediaGrant,
        codec: VideoCodec,
        resolution: (u32, u32),
    ) -> BoxFuture<'a, Result<Box<dyn PublishedVideoTrack>, PublishError>>;
}

impl RoomConnector for std::sync::Arc<dyn RoomConnector> {
    fn connect_and_publish<'a>(
        &'a self,
        signaling_url: &'a str,
        participant_token: &'a str,
        grant: &'a MediaGrant,
        codec: VideoCodec,
        resolution: (u32, u32),
    ) -> BoxFuture<'a, Result<Box<dyn PublishedVideoTrack>, PublishError>> {
        (**self).connect_and_publish(signaling_url, participant_token, grant, codec, resolution)
    }
}

/// Opens a playback of local recordings that replays their compressed video
/// into `units`.
///
/// Implemented over the node's recordings (`playback::encoded_replay`);
/// injected so the publication logic can be tested without GStreamer.
pub trait PlaybackOpener: Send + Sync {
    fn open<'a>(
        &'a self,
        camera_id: &'a str,
        position: OffsetDateTime,
        speed_percent: u16,
        playing: bool,
        units: std::sync::Arc<EncodedSubmitter>,
    ) -> BoxFuture<'a, Result<Box<dyn PlaybackRun>, PublishError>>;
}

/// Opens a camera's own stream for live view, compressed: each publication
/// gets a dedicated encoded source, independent of the desktop view and of
/// recording, so neither can restart or stall it.
pub trait LiveSourceOpener: Send + Sync {
    /// Start feeding `units` from `camera_id`'s `representation` stream.
    /// Returns at once; connecting and reconnecting happen in the background.
    /// Fails at once for a stream the camera does not have
    /// ([`PublishError::StreamUnavailable`]) or one with no compressed video
    /// to forward ([`PublishError::UnsupportedSource`]).
    fn open(
        &self,
        camera_id: &str,
        representation: Representation,
        units: std::sync::Arc<EncodedSubmitter>,
    ) -> Result<Box<dyn LiveSourceRun>, PublishError>;
}

/// A running dedicated live source. Stopping it is idempotent.
pub trait LiveSourceRun: Send + Sync {
    fn stop(&self);
}

/// A running playback. Speed and pause apply in place; a seek reopens.
pub trait PlaybackRun: Send + Sync {
    fn set_speed_percent(&self, percent: u16);
    fn set_playing(&self, playing: bool);
    fn stop(&self);
    /// Recording time of the frame last sent, once one has been.
    fn position(&self) -> Option<OffsetDateTime> {
        None
    }
}

/// The control last applied to a playback publication, and the run it produced.
type AppliedPlayback = (Option<PlaybackControl>, Option<Box<dyn PlaybackRun>>);

/// The playback side of one playback publication: its track's unit queue,
/// the control last applied, and the run that feeds it.
struct PlaybackPublication {
    camera_id: String,
    units: std::sync::Arc<EncodedSubmitter>,
    opener: std::sync::Arc<dyn PlaybackOpener>,
    run: AsyncMutex<AppliedPlayback>,
    /// Data-channel state: the last control sequence applied, the last
    /// problem to report, and the message rate window.
    channel: Mutex<ChannelState>,
}

#[derive(Default)]
struct ChannelState {
    last_seq: u64,
    error: Option<String>,
    window: Option<(std::time::Instant, u32)>,
}

/// Most viewer messages handled per second; the rest are ignored.
const MAX_VIEWER_MESSAGES_PER_SECOND: u32 = 20;

/// A control from a viewer over the data channel. Fields left out keep
/// their current value; `position_at` present means a seek.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewerControl {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    seq: u64,
    #[serde(default)]
    position_at: Option<String>,
    #[serde(default)]
    speed_percent: Option<u16>,
    #[serde(default)]
    playing: Option<bool>,
}

/// Whole-second UTC in the canonical form playback controls use.
fn canonical_utc(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

impl PlaybackPublication {
    fn new(
        camera_id: String,
        units: EncodedSubmitter,
        opener: std::sync::Arc<dyn PlaybackOpener>,
    ) -> Self {
        Self {
            camera_id,
            units: std::sync::Arc::new(units),
            opener,
            run: AsyncMutex::new((None, None)),
            channel: Mutex::new(ChannelState::default()),
        }
    }

    fn set_error(&self, error: Option<String>) {
        if let Ok(mut channel) = self.channel.lock() {
            channel.error = error;
        }
    }

    /// Apply `control` unless a newer (or the same) revision already applied.
    /// This is the control plane's path (publish directives).
    ///
    /// Once the viewer steers over the data channel, the channel is the
    /// order of record: every change it sends is also saved with the cloud,
    /// but that copy arrives later and can be older than the viewer's next
    /// change (Play, then Pause before the Play's directive lands would
    /// resume playback). So only the revision is taken from it.
    async fn apply(&self, control: PlaybackControl) {
        let mut guard = self.run.lock().await;
        let steered = self.steered_by_channel();
        match guard.0.as_mut() {
            Some(applied) if applied.revision >= control.revision => {}
            Some(applied) if steered => applied.revision = control.revision,
            _ => self.apply_locked(&mut guard, control).await,
        }
    }

    /// Whether a viewer control has arrived over the data channel.
    fn steered_by_channel(&self) -> bool {
        self.channel
            .lock()
            .map(|channel| channel.last_seq > 0)
            .unwrap_or(false)
    }

    /// Apply a viewer's control from the data channel at once. It keeps the
    /// control-plane revision, so the same change arriving later as a
    /// directive is recognised as already applied.
    async fn apply_viewer(&self, message: ViewerControl) -> Result<(), String> {
        let mut guard = self.run.lock().await;
        let Some(current) = guard.0.clone() else {
            return Err("playback has not started yet".into());
        };
        let mut next = current.clone();
        if let Some(speed) = message.speed_percent {
            if !PLAYBACK_SPEEDS_PERCENT.contains(&speed) {
                return Err("unsupported playback speed".into());
            }
            next.speed_percent = speed;
        }
        if let Some(playing) = message.playing {
            next.playing = playing;
        }
        if let Some(position) = &message.position_at {
            let at =
                OffsetDateTime::parse(position, &time::format_description::well_known::Rfc3339)
                    .map_err(|_| "invalid position".to_owned())?;
            next.position_at_utc = canonical_utc(at);
        }
        self.apply_locked(&mut guard, next).await;
        Ok(())
    }

    async fn apply_locked(&self, guard: &mut AppliedPlayback, control: PlaybackControl) {
        let (applied, run) = guard;
        // Compared as instants: the same position may be written with or
        // without fractional seconds by the browser and the control plane.
        let seek = applied.as_ref().map_or(true, |applied| {
            applied.position().ok() != control.position().ok()
        });
        match (seek, run.as_ref(), applied.as_ref()) {
            (false, Some(current), Some(previous)) => {
                if previous.speed_percent != control.speed_percent {
                    current.set_speed_percent(control.speed_percent);
                }
                if previous.playing != control.playing {
                    current.set_playing(control.playing);
                }
            }
            _ => {
                let Ok(position) = control.position() else {
                    return;
                };
                if let Some(previous) = run.take() {
                    previous.stop();
                }
                match self
                    .opener
                    .open(
                        &self.camera_id,
                        position,
                        control.speed_percent,
                        control.playing,
                        std::sync::Arc::clone(&self.units),
                    )
                    .await
                {
                    Ok(opened) => {
                        *run = Some(opened);
                        self.set_error(None);
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, cam_id = %self.camera_id, "Playback could not be opened");
                        self.set_error(Some("There is no recording at that time.".into()));
                    }
                }
            }
        }
        *applied = Some(control);
    }

    async fn stop(&self) {
        if let Some(run) = self.run.lock().await.1.take() {
            run.stop();
        }
    }

    /// Accept a message under the rate limit and in sequence order.
    fn admit(&self, seq: u64) -> bool {
        let Ok(mut channel) = self.channel.lock() else {
            return false;
        };
        let now = std::time::Instant::now();
        let (since, count) = match channel.window {
            Some((since, count))
                if now.duration_since(since) < std::time::Duration::from_secs(1) =>
            {
                (since, count)
            }
            _ => (now, 0),
        };
        if count >= MAX_VIEWER_MESSAGES_PER_SECOND || seq <= channel.last_seq {
            return false;
        }
        channel.window = Some((since, count + 1));
        channel.last_seq = seq;
        true
    }
}

impl DataControl for PlaybackPublication {
    fn on_message<'a>(&'a self, identity: &'a str, payload: &'a [u8]) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if !is_viewer(identity) || payload.len() > MAX_VIEWER_MESSAGE_BYTES {
                return;
            }
            let Ok(message) = serde_json::from_slice::<ViewerControl>(payload) else {
                return;
            };
            if message.v != 1 || message.kind != "control" || !self.admit(message.seq) {
                return;
            }
            tracing::debug!(cam_id = %self.camera_id, seq = message.seq, "Playback control from the data channel");
            if let Err(error) = self.apply_viewer(message).await {
                self.set_error(Some(error));
            }
        })
    }

    fn report(&self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            let guard = self.run.lock().await;
            let (Some(applied), run) = (&guard.0, &guard.1) else {
                return None;
            };
            // What is on screen: the last frame sent, else where playback was asked to be.
            let position = run
                .as_ref()
                .and_then(|run| run.position())
                .map(canonical_utc)
                .unwrap_or_else(|| applied.position_at_utc.clone());
            let (seq, error) = self
                .channel
                .lock()
                .map(|channel| (channel.last_seq, channel.error.clone()))
                .unwrap_or((0, None));
            serde_json::to_vec(&serde_json::json!({
                "v": 1,
                "type": "state",
                "seq": seq,
                "position_at": position,
                "speed_percent": applied.speed_percent,
                "playing": applied.playing,
                "error": error,
            }))
            .ok()
        })
    }
}

/// Ceiling on a pass-through track's send rate, far above any camera stream.
///
/// The camera sets the bitrate and the node cannot change it. WebRTC drops
/// input frames whose output exceeds the target rate, and the SDK's default
/// target for a track is derived from its picture size; for a pass-through
/// that drops delta frames, which breaks every picture until the next
/// keyframe. So the ceiling is never what limits the stream: the network is.
const PASS_THROUGH_MAX_BITRATE: u64 = 50_000_000;

/// How a camera's own compressed stream is published: passed through as is,
/// with no simulcast (there is only the one encoding).
fn encoded_publish_options(codec: VideoCodec) -> TrackPublishOptions {
    TrackPublishOptions {
        video_codec: match codec {
            VideoCodec::H264 => TrackCodec::H264,
            VideoCodec::H265 => TrackCodec::H265,
        },
        video_encoder: VideoEncoderBackend::PreEncoded,
        video_encoding: Some(VideoEncoding {
            max_bitrate: PASS_THROUGH_MAX_BITRATE,
            max_framerate: 60.0,
        }),
        source: TrackSource::Camera,
        simulcast: false,
        ..Default::default()
    }
}

/// Real LiveKit transport; with a [`TalkSink`], live viewers may talk back.
#[derive(Default)]
pub struct LiveKitRoomConnector {
    talk: Option<std::sync::Arc<dyn TalkSink>>,
}

impl RoomConnector for LiveKitRoomConnector {
    fn connect_and_publish<'a>(
        &'a self,
        signaling_url: &'a str,
        participant_token: &'a str,
        grant: &'a MediaGrant,
        codec: VideoCodec,
        resolution: (u32, u32),
    ) -> BoxFuture<'a, Result<Box<dyn PublishedVideoTrack>, PublishError>> {
        Box::pin(async move {
            let (width, height) = resolution;
            if width == 0 || height == 0 {
                return Err(PublishError::InvalidFrame);
            }
            // No raw keepalive frames: they would start a real encoder on a
            // sender meant for pass-through.
            let source = NativeVideoSource::new_encoded(VideoResolution { width, height });
            let talk = self
                .talk
                .clone()
                .filter(|_| grant.scope.action == MediaAction::Live);
            connect_room(
                signaling_url,
                participant_token,
                grant,
                source,
                encoded_publish_options(codec),
                talk,
            )
            .await
        })
    }
}

async fn connect_room(
    signaling_url: &str,
    participant_token: &str,
    grant: &MediaGrant,
    source: NativeVideoSource,
    publish: TrackPublishOptions,
    talk: Option<std::sync::Arc<dyn TalkSink>>,
) -> Result<Box<dyn PublishedVideoTrack>, PublishError> {
    // `RoomOptions` is `#[non_exhaustive]`, so it can only be built by
    // mutating a default from outside the livekit crate.
    let mut options = RoomOptions::default();
    // Nothing is received unless asked for: only a viewer's talk track,
    // in a live room, ever is (the token of other rooms cannot subscribe).
    options.auto_subscribe = false;
    let (room, events) = Room::connect(signaling_url, participant_token, options)
        .await
        .map_err(|error| {
            tracing::warn!(session = %grant.session_id, %error, "livekit room connect failed");
            PublishError::Transport
        })?;

    let track = LocalVideoTrack::create_video_track(
        &format!("nw-{}", grant.scope.camera_id),
        RtcVideoSource::Native(source.clone()),
    );
    let codec = publish.video_codec.as_str();
    let publication = room
        .local_participant()
        .publish_track(LocalTrack::Video(track.clone()), publish)
        .await
        .map_err(|error| {
            tracing::warn!(session = %grant.session_id, %error, "livekit publish_track failed");
            PublishError::Transport
        })?;

    // The event receiver is unbounded, so it is drained rather than
    // parked: an undrained receiver grows without limit for the life of
    // the session. The task ends when the room closes the channel.
    // Viewer data on the playback topic goes to a bounded queue; the
    // rest is only logged.
    let (messages_tx, messages_rx) = mpsc::channel(VIEWER_MESSAGE_QUEUE);
    let viewed = std::sync::Arc::new(AtomicBool::new(false));
    let subscribed = std::sync::Arc::clone(&viewed);
    let camera_id = grant.scope.camera_id.clone();
    let local = room.local_participant();
    tokio::spawn(async move {
        let mut events = events;
        let mut talkers: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        while let Some(event) = events.recv().await {
            match event {
                RoomEvent::LocalTrackSubscribed { .. } => {
                    subscribed.store(true, Ordering::Relaxed);
                }
                RoomEvent::TrackPublished {
                    publication,
                    participant,
                } if talk.is_some()
                    && publication.kind() == TrackKind::Audio
                    && publication.name() == TALK_TRACK
                    && is_viewer(participant.identity().as_str()) =>
                {
                    publication.set_subscribed(true);
                }
                RoomEvent::TrackSubscribed {
                    track: RemoteTrack::Audio(track),
                    publication,
                    participant,
                } if publication.name() == TALK_TRACK
                    && is_viewer(participant.identity().as_str()) =>
                {
                    if let Some(sink) = talk.clone() {
                        let speaker = participant.identity().to_string();
                        if let Some(old) = talkers.remove(&speaker) {
                            old.abort();
                        }
                        let task = tokio::spawn(relay_talk(
                            sink,
                            camera_id.clone(),
                            speaker.clone(),
                            track,
                            local.clone(),
                        ));
                        talkers.insert(speaker, task);
                    }
                }
                RoomEvent::TrackUnpublished { participant, .. }
                | RoomEvent::TrackUnsubscribed { participant, .. } => {
                    stop_talker(
                        &mut talkers,
                        talk.as_deref(),
                        participant.identity().as_str(),
                    );
                }
                RoomEvent::ParticipantDisconnected(participant) => {
                    stop_talker(
                        &mut talkers,
                        talk.as_deref(),
                        participant.identity().as_str(),
                    );
                }
                RoomEvent::DataReceived {
                    payload,
                    topic,
                    participant,
                    ..
                } if topic.as_deref() == Some(PLAYBACK_TOPIC)
                    && payload.len() <= MAX_VIEWER_MESSAGE_BYTES =>
                {
                    let identity = participant
                        .map(|participant| participant.identity().to_string())
                        .unwrap_or_default();
                    if is_viewer(&identity) {
                        let _ = messages_tx.try_send(ViewerMessage {
                            identity,
                            payload: payload.to_vec(),
                        });
                    }
                }
                other => tracing::debug!(event = ?other, "livekit room event"),
            }
        }
        for speaker in talkers.keys().cloned().collect::<Vec<_>>() {
            stop_talker(&mut talkers, talk.as_deref(), &speaker);
        }
    });

    tracing::info!(
        session = %grant.session_id,
        camera = %grant.scope.camera_id,
        codec,
        "publishing camera to livekit"
    );
    Ok(Box::new(LiveKitPublishedTrack {
        session: Mutex::new(Some(LiveKitSession {
            room,
            publication,
            track,
            source,
        })),
        messages: Mutex::new(Some(messages_rx)),
        camera_id: grant.scope.camera_id.clone(),
        viewed,
    }) as Box<dyn PublishedVideoTrack>)
}

fn stop_talker(
    talkers: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    sink: Option<&dyn TalkSink>,
    speaker: &str,
) {
    if let Some(task) = talkers.remove(speaker) {
        task.abort();
        if let Some(sink) = sink {
            sink.stop(speaker);
        }
    }
}

/// A camera's sound in a LiveKit room; the SDK encodes it to Opus.
struct LiveKitAudioTrack {
    source: NativeAudioSource,
    /// Held so the published track lives as long as this does.
    _track: LocalAudioTrack,
}

impl PublishedAudioTrack for LiveKitAudioTrack {
    fn capture<'a>(&'a self, samples: &'a [i16]) -> BoxFuture<'a, Result<(), PublishError>> {
        Box::pin(async move {
            let frame = AudioFrame {
                data: samples.into(),
                sample_rate: AUDIO_RATE,
                num_channels: 1,
                samples_per_channel: samples.len() as u32,
            };
            self.source
                .capture_frame(&frame)
                .await
                .map_err(|_| PublishError::Transport)
        })
    }
}

struct LiveKitSession {
    room: Room,
    publication: livekit::prelude::LocalTrackPublication,
    /// Held so the published track outlives the publication handle.
    track: LocalVideoTrack,
    source: NativeVideoSource,
}

struct LiveKitPublishedTrack {
    /// `None` once closed. Taken out of the lock before any `.await`, so the
    /// lock is never held across one.
    session: Mutex<Option<LiveKitSession>>,
    messages: Mutex<Option<mpsc::Receiver<ViewerMessage>>>,
    camera_id: String,
    /// LiveKit has told this publisher a participant subscribed to its track.
    viewed: std::sync::Arc<AtomicBool>,
}

impl PublishedVideoTrack for LiveKitPublishedTrack {
    fn viewer_attached(&self) -> Option<bool> {
        Some(self.viewed.load(Ordering::Relaxed))
    }

    fn capture_encoded(&self, unit: &EncodedAccessUnit) -> Result<(), PublishError> {
        let guard = self.session.lock().expect("livekit session poisoned");
        let Some(session) = guard.as_ref() else {
            return Err(PublishError::Transport);
        };
        // `false` means the adapter dropped the unit (frame-rate adaptation);
        // the stream carries on, as with raw capture.
        session.source.capture_encoded_frame(&EncodedVideoFrame {
            codec: match unit.codec {
                VideoCodec::H264 => EncodedVideoCodec::H264,
                VideoCodec::H265 => EncodedVideoCodec::H265,
            },
            payload: &unit.payload,
            timestamp_us: unit.timestamp_us,
            frame_type: if unit.keyframe {
                EncodedFrameType::Key
            } else {
                EncodedFrameType::Delta
            },
            resolution: VideoResolution {
                width: unit.width,
                height: unit.height,
            },
            frame_metadata: None,
        });
        Ok(())
    }

    fn take_keyframe_request(&self) -> bool {
        self.session
            .lock()
            .ok()
            .and_then(|session| {
                session
                    .as_ref()
                    .map(|session| session.source.take_keyframe_request())
            })
            .unwrap_or(false)
    }

    fn negotiated_codec(&self) -> BoxFuture<'_, Option<String>> {
        // The publication's own mime type is not filled in for the publisher,
        // so the sender's RTP stats are asked which codec it sends in.
        let track = self
            .session
            .lock()
            .ok()
            .and_then(|session| session.as_ref().map(|session| session.track.clone()));
        Box::pin(async move {
            let stats = track?.get_stats().await.ok()?;
            let codec_id = stats.iter().find_map(|stat| match stat {
                livekit::webrtc::stats::RtcStats::OutboundRtp(rtp) => {
                    Some(rtp.stream.codec_id.clone()).filter(|id| !id.is_empty())
                }
                _ => None,
            })?;
            stats.iter().find_map(|stat| match stat {
                livekit::webrtc::stats::RtcStats::Codec(codec) if codec.rtc.id == codec_id => {
                    Some(codec.codec.mime_type.to_ascii_lowercase()).filter(|mime| !mime.is_empty())
                }
                _ => None,
            })
        })
    }

    fn take_messages(&self) -> Option<mpsc::Receiver<ViewerMessage>> {
        self.messages.lock().ok()?.take()
    }

    fn send_data(&self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), PublishError>> {
        // The participant handle is cloned out, so no lock is held across the send.
        let participant = self
            .session
            .lock()
            .ok()
            .and_then(|session| session.as_ref().map(|s| s.room.local_participant()));
        Box::pin(async move {
            let participant = participant.ok_or(PublishError::Transport)?;
            participant
                .publish_data(DataPacket {
                    payload,
                    topic: Some(PLAYBACK_TOPIC.to_owned()),
                    reliable: true,
                    ..Default::default()
                })
                .await
                .map_err(|_| PublishError::Transport)
        })
    }

    fn publish_audio(
        &self,
    ) -> BoxFuture<'_, Result<Option<Box<dyn PublishedAudioTrack>>, PublishError>> {
        // The participant handle is cloned out, so no lock is held across the publish.
        let participant = self
            .session
            .lock()
            .ok()
            .and_then(|session| session.as_ref().map(|s| s.room.local_participant()));
        Box::pin(async move {
            let participant = participant.ok_or(PublishError::Transport)?;
            // Camera sound as it is: no echo cancelling, noise suppression or gain.
            let source = NativeAudioSource::new(
                AudioSourceOptions::default(),
                AUDIO_RATE,
                1,
                AUDIO_SOURCE_QUEUE_MS,
            );
            let track = LocalAudioTrack::create_audio_track(
                &format!("nw-{}-audio", self.camera_id),
                RtcAudioSource::Native(source.clone()),
            );
            let options = TrackPublishOptions {
                source: TrackSource::Microphone,
                // Silence is sent as silence, not left out: a camera's sound is
                // not a conversation.
                dtx: false,
                ..Default::default()
            };
            participant
                .publish_track(LocalTrack::Audio(track.clone()), options)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "livekit audio publish_track failed");
                    PublishError::Transport
                })?;
            Ok(Some(Box::new(LiveKitAudioTrack {
                source,
                _track: track,
            }) as Box<dyn PublishedAudioTrack>))
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let session = self
                .session
                .lock()
                .expect("livekit session poisoned")
                .take();
            let Some(session) = session else {
                return;
            };
            let sid = session.publication.sid();
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                session.room.local_participant().unpublish_track(&sid),
            )
            .await
            {
                Ok(Err(error)) => tracing::debug!(%error, "unpublishing the livekit track failed"),
                Err(_) => tracing::warn!("unpublishing the livekit track timed out"),
                Ok(Ok(_)) => {}
            }
            match tokio::time::timeout(std::time::Duration::from_secs(5), session.room.close())
                .await
            {
                Ok(Err(error)) => tracing::debug!(%error, "closing the livekit room failed"),
                Err(_) => tracing::warn!("closing the livekit room timed out"),
                Ok(Ok(_)) => {}
            }
            // Dropping the session drops the room, the publication and with it
            // the track and its native source.
            drop(session);
        })
    }
}

// ---------------------------------------------------------------------------
// The bridge
// ---------------------------------------------------------------------------

/// Cancels a running publication. Dropping every handle also cancels.
#[derive(Clone)]
pub struct PublishCancel {
    sender: std::sync::Arc<watch::Sender<bool>>,
}

impl PublishCancel {
    /// Stops the pump and disconnects. Safe to call more than once, and from
    /// any thread.
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }
}

impl MediaBridge for Box<dyn MediaBridge> {
    fn publish<'a>(
        &'a self,
        config: &'a LiveKitPublisherConfig,
        grant: &'a MediaGrant,
        participant_token: &'a str,
    ) -> BoxFuture<'a, Result<(), PublishError>> {
        (**self).publish(config, grant, participant_token)
    }
}

/// Window over which a forwarded stream's bitrate is compared with its grant.
const BITRATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Most bytes and access units of one GOP kept to answer keyframe requests;
/// a longer GOP is not kept, and its viewers wait for the next keyframe.
const GOP_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;
const GOP_CACHE_MAX_UNITS: usize = 1024;
/// Keyframe requests are answered at most this often; one that comes sooner
/// is answered once the gap has passed.
const GOP_REPLAY_MIN_GAP: std::time::Duration = std::time::Duration::from_millis(500);
/// How often an idle stream (e.g. paused playback) looks for keyframe requests.
const KEYFRAME_REQUEST_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// Least spacing of capture timestamps: replayed pictures go out back to back,
/// and each must keep its own RTP timestamp (90 kHz).
const MIN_FRAME_SPACING_US: i64 = 1_000;
/// Longest a publication holds its pictures waiting for a viewer's first
/// keyframe request; past it, it sends anyway.
const AWAIT_VIEWER_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Least real time between two units handed to the track. WebRTC encodes
/// only the newest of frames that arrive while its encoder queue is busy, so
/// a burst (the GOP sent again, or units queued while the room was joined)
/// would lose its keyframe; spacing them lets every one through. A camera's
/// own pace is far slower and never waits.
const CAPTURE_SPACING: std::time::Duration = std::time::Duration::from_millis(3);

/// The access units sent since the latest keyframe.
///
/// A camera cannot be asked for a keyframe over RTSP, and a viewer that joins
/// after the publication's keyframe went out (the room is subscribed to only
/// once the track exists) would wait for the camera's next one: up to a whole
/// GOP, a few seconds on many cameras. Sending this GOP again, keyframe first,
/// gives that viewer a picture at once and keeps the reference chain whole.
/// Viewers already watching decode the same pictures again, briefly.
#[derive(Default)]
struct GopCache {
    units: Vec<EncodedAccessUnit>,
    bytes: usize,
    /// This GOP outgrew the cache; nothing is kept until the next keyframe.
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
        if self.units.len() >= GOP_CACHE_MAX_UNITS
            || self.bytes + unit.payload.len() > GOP_CACHE_MAX_BYTES
        {
            self.units.clear();
            self.bytes = 0;
            self.overflowed = true;
            return;
        }
        self.bytes += unit.payload.len();
        self.units.push(unit.clone());
    }
}

/// The live-view bridge: connect, publish a pre-encoded track, then forward
/// the camera's access units unchanged. Nothing here decodes or encodes.
///
/// The track needs the stream's codec and size, which come from its first
/// keyframe, so the room is joined only once that keyframe has arrived. As
/// with [`LiveKitMediaBridge`], this runs only after the grant was verified
/// and spent, and no unit reaches the transport before the track is published.
pub struct EncodedMediaBridge<C> {
    feed: EncodedFeed,
    connector: C,
    cancel: watch::Receiver<bool>,
    captured: AtomicU64,
    control: Option<std::sync::Arc<dyn DataControl>>,
}

impl<C: RoomConnector> EncodedMediaBridge<C> {
    pub fn new(feed: EncodedFeed, connector: C) -> (Self, PublishCancel) {
        let (sender, cancel) = watch::channel(false);
        (
            Self {
                feed,
                connector,
                cancel,
                captured: AtomicU64::new(0),
                control: None,
            },
            PublishCancel {
                sender: std::sync::Arc::new(sender),
            },
        )
    }

    /// Access units handed to the transport so far.
    #[cfg(test)]
    pub fn captured(&self) -> u64 {
        self.captured.load(Ordering::Relaxed)
    }

    /// Serve the session's data channel with `control` (recorded playback).
    pub fn with_control(mut self, control: std::sync::Arc<dyn DataControl>) -> Self {
        self.control = Some(control);
        self
    }

    async fn pump(
        &self,
        track: &dyn PublishedVideoTrack,
        first: EncodedAccessUnit,
        grant: &MediaGrant,
    ) -> Result<(), PublishError> {
        let codec = first.codec;
        let mut cancel = self.cancel.clone();
        let budget = u64::from(grant.max_bitrate_kbps) * 125 * BITRATE_WINDOW.as_secs();
        let (mut window_start, mut window_bytes, mut warned) =
            (std::time::Instant::now(), 0u64, false);
        let mut next = Some(first);
        let mut gop = GopCache::default();
        // Capture timestamps only move forward, replayed pictures included.
        let mut last_timestamp_us = i64::MIN;
        let mut keyframe_wanted = false;
        let mut last_replay: Option<std::time::Instant> = None;
        let mut last_capture: Option<tokio::time::Instant> = None;
        // A server that does not accept the camera's codec settles on another
        // one, and the camera's bytes would go out labelled as that. The
        // sender knows which once negotiation completes; until then it is
        // asked about once a second.
        let mut codec_confirmed = false;
        let mut next_codec_check = std::time::Instant::now();
        // Pictures sent before a viewer is attached are lost to it: it then
        // asks for a keyframe, and the GOP would go out again, back over
        // pictures it may already show (the picture jumps back, then races
        // forward). Held until the first request, the GOP goes out once, from
        // its keyframe, and becomes the viewer's start.
        let mut awaiting_viewer = track.viewer_attached() == Some(false);
        let viewer_deadline = std::time::Instant::now() + AWAIT_VIEWER_LIMIT;
        let expected_mime = match codec {
            VideoCodec::H264 => "video/h264",
            VideoCodec::H265 => "video/h265",
        };
        loop {
            if !codec_confirmed && std::time::Instant::now() >= next_codec_check {
                next_codec_check = std::time::Instant::now() + std::time::Duration::from_secs(1);
                if let Some(negotiated) = track.negotiated_codec().await {
                    if negotiated != expected_mime {
                        tracing::warn!(
                            session = %grant.session_id,
                            camera_codec = codec.code(),
                            %negotiated,
                            "the media server does not accept this camera's codec"
                        );
                        return Err(PublishError::CodecRefused);
                    }
                    codec_confirmed = true;
                }
            }
            if let Some(mut unit) = next.take() {
                if unit.codec != codec {
                    // The track was negotiated for one codec; a changed camera
                    // ends this publication, and the next grant starts a new one.
                    tracing::warn!(session = %grant.session_id, "camera stream changed codec");
                    return Err(PublishError::CodecChanged);
                }
                if awaiting_viewer {
                    // Held: no viewer could decode it yet (see `awaiting_viewer`).
                    gop.push(&unit);
                } else {
                    unit.timestamp_us = unit
                        .timestamp_us
                        .max(last_timestamp_us.saturating_add(MIN_FRAME_SPACING_US));
                    last_timestamp_us = unit.timestamp_us;
                    pace(&mut last_capture).await;
                    track.capture_encoded(&unit)?;
                    self.captured.fetch_add(1, Ordering::Relaxed);
                    window_bytes += unit.payload.len() as u64;
                    if window_start.elapsed() >= BITRATE_WINDOW {
                        if !warned && window_bytes > budget + budget / 4 {
                            warned = true;
                            tracing::warn!(
                                session = %grant.session_id,
                                kbps = window_bytes * 8 / 1000 / BITRATE_WINDOW.as_secs(),
                                granted_kbps = grant.max_bitrate_kbps,
                                "camera stream exceeds its grant's bitrate; it is forwarded unchanged"
                            );
                        }
                        (window_start, window_bytes) = (std::time::Instant::now(), 0);
                    }
                    // A keyframe just sent answers any request made before it.
                    if unit.keyframe {
                        keyframe_wanted = false;
                        let _ = track.take_keyframe_request();
                    }
                    gop.push(&unit);
                }
            }
            let requested = track.take_keyframe_request();
            if awaiting_viewer {
                // The first viewer is attached (or none came in time): its
                // start is the held GOP, from its keyframe.
                if requested
                    || track.viewer_attached() == Some(true)
                    || std::time::Instant::now() >= viewer_deadline
                {
                    awaiting_viewer = false;
                    keyframe_wanted = true;
                }
            } else {
                // A viewer that just joined (or lost data) asked for a keyframe:
                // send the current GOP again rather than make it wait for the camera's next.
                keyframe_wanted |= requested;
            }
            if !awaiting_viewer
                && keyframe_wanted
                && !gop.units.is_empty()
                && last_replay.map_or(true, |at| at.elapsed() >= GOP_REPLAY_MIN_GAP)
            {
                keyframe_wanted = false;
                last_replay = Some(std::time::Instant::now());
                tracing::debug!(session = %grant.session_id, units = gop.units.len(), "viewer keyframe request; resending the current GOP");
                // A catch-up: stamped [`MIN_FRAME_SPACING_US`] apart and ending
                // now, so the stream goes on from it without a jump in time,
                // and a player can tell it from pictures at the camera's pace.
                let count = gop.units.len() as i64;
                let mut stamp = last_timestamp_us
                    .saturating_add(MIN_FRAME_SPACING_US)
                    .max(now_us().saturating_sub(count * MIN_FRAME_SPACING_US));
                for cached in &gop.units {
                    let mut replayed = cached.clone();
                    replayed.timestamp_us = stamp;
                    last_timestamp_us = stamp;
                    stamp = stamp.saturating_add(MIN_FRAME_SPACING_US);
                    pace(&mut last_capture).await;
                    track.capture_encoded(&replayed)?;
                    self.captured.fetch_add(1, Ordering::Relaxed);
                }
            }
            next = tokio::select! {
                biased;
                _ = cancel.changed() => return Ok(()),
                unit = self.feed.next() => match unit {
                    Some(unit) => Some(unit),
                    None => return self.feed.failure().map_or(Ok(()), Err),
                },
                _ = tokio::time::sleep(KEYFRAME_REQUEST_POLL) => None,
            };
        }
    }
}

/// Wall-clock time in µs since the epoch (capture timestamps' clock).
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as i64)
}

/// Wait until [`CAPTURE_SPACING`] has passed since the previous capture.
async fn pace(last_capture: &mut Option<tokio::time::Instant>) {
    if let Some(at) = *last_capture {
        tokio::time::sleep_until(at + CAPTURE_SPACING).await;
    }
    *last_capture = Some(tokio::time::Instant::now());
}

impl<C: RoomConnector> MediaBridge for EncodedMediaBridge<C> {
    fn publish<'a>(
        &'a self,
        config: &'a LiveKitPublisherConfig,
        grant: &'a MediaGrant,
        participant_token: &'a str,
    ) -> BoxFuture<'a, Result<(), PublishError>> {
        Box::pin(async move {
            if !config.enabled {
                return Err(PublishError::Unconfigured);
            }
            let mut cancel = self.cancel.clone();
            if *cancel.borrow_and_update() {
                return Ok(());
            }
            // The feed starts at a keyframe; the camera may still be connecting.
            let first = tokio::select! {
                biased;
                _ = cancel.changed() => return Ok(()),
                unit = self.feed.next() => match unit {
                    Some(unit) => unit,
                    None => return self.feed.failure().map_or(Ok(()), Err),
                },
            };
            let connect = self.connector.connect_and_publish(
                &config.signaling_url,
                participant_token,
                grant,
                first.codec,
                (first.width, first.height),
            );
            let track = tokio::select! {
                biased;
                _ = cancel.changed() => return Ok(()),
                result = connect => result?,
            };
            let track: std::sync::Arc<dyn PublishedVideoTrack> = std::sync::Arc::from(track);
            let channel =
                self.control
                    .clone()
                    .zip(track.take_messages())
                    .map(|(control, messages)| {
                        spawn_data_channel(control, messages, std::sync::Arc::clone(&track))
                    });
            let audio = self.feed.take_audio().map(|samples| {
                spawn_audio(
                    samples,
                    std::sync::Arc::clone(&track),
                    grant.session_id.clone(),
                )
            });
            let result = self.pump(track.as_ref(), first, grant).await;
            if let Some(channel) = channel {
                channel.abort();
            }
            if let Some(audio) = audio {
                audio.abort();
            }
            track.close().await;
            result
        })
    }
}

/// Forward the camera's sound into an audio track, published when the first
/// of it arrives (a camera without audio never gets one).
fn spawn_audio(
    mut samples: mpsc::Receiver<Vec<i16>>,
    track: std::sync::Arc<dyn PublishedVideoTrack>,
    session: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(first) = samples.recv().await else {
            return;
        };
        let audio = match track.publish_audio().await {
            Ok(Some(audio)) => audio,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%session, %error, "Camera audio could not be published");
                return;
            }
        };
        tracing::info!(%session, "Publishing camera audio");
        let mut next = Some(first);
        while let Some(chunk) = next {
            if audio.capture(&chunk).await.is_err() {
                tracing::debug!(%session, "Camera audio track closed");
                return;
            }
            next = samples.recv().await;
        }
    })
}

/// Serve a session's data channel: hand each viewer message to `control`,
/// and send its state right after and once a second.
fn spawn_data_channel(
    control: std::sync::Arc<dyn DataControl>,
    mut messages: mpsc::Receiver<ViewerMessage>,
    track: std::sync::Arc<dyn PublishedVideoTrack>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                message = messages.recv() => {
                    let Some(message) = message else { break };
                    control.on_message(&message.identity, &message.payload).await;
                }
                _ = tick.tick() => {}
            }
            if let Some(state) = control.report().await {
                if let Err(error) = track.send_data(state).await {
                    tracing::debug!(%error, "Could not send playback state");
                }
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherState {
    Disconnected,
    Authorizing,
    Unavailable,
}

pub struct LiveKitPublisher<C, B> {
    config: LiveKitPublisherConfig,
    consumer: C,
    bridge: B,
    consumed: Mutex<HashSet<String>>,
    state: Mutex<PublisherState>,
}

impl<C: GrantConsumer, B: MediaBridge> LiveKitPublisher<C, B> {
    pub fn new(config: LiveKitPublisherConfig, consumer: C, bridge: B) -> Self {
        Self {
            config,
            consumer,
            bridge,
            consumed: Mutex::new(HashSet::new()),
            state: Mutex::new(PublisherState::Disconnected),
        }
    }

    pub fn state(&self) -> PublisherState {
        *self.state.lock().expect("publisher state poisoned")
    }

    pub async fn publish(
        &self,
        directive: PublishDirective,
        now: OffsetDateTime,
    ) -> Result<(), PublishError> {
        directive
            .validate_with(self.config.endpoint_policy)
            .map_err(|_| PublishError::InvalidDirective)?;
        if directive.sfu.region_id != self.config.region_id
            || directive.sfu.signaling_url != self.config.signaling_url
        {
            return Err(PublishError::InvalidDirective);
        }
        let grant = read_grant(&directive.grant)?;
        if grant.session_id != directive.session_id
            || grant.audience != directive.sfu.region_id
            || self.config.node_id.as_deref() != Some(grant.scope.site_id.as_str())
            || !matches!(
                grant.scope.action,
                MediaAction::Live | MediaAction::Playback
            )
        {
            return Err(PublishError::ScopeMismatch);
        }
        grant.is_usable_at(now).map_err(|_| PublishError::Expired)?;
        if self
            .consumed
            .lock()
            .expect("grant set poisoned")
            .contains(&grant.grant_id)
        {
            return Err(PublishError::Replay);
        }
        *self.state.lock().expect("publisher state poisoned") = PublisherState::Authorizing;
        self.consumer.consume(&directive.grant).await?;
        self.consumed
            .lock()
            .expect("grant set poisoned")
            .insert(grant.grant_id.clone());
        // The control plane minted this token for this room alone: publish,
        // data for the viewer, and (live only) the viewer's talk track.
        let result = self
            .bridge
            .publish(&self.config, &grant, &directive.publisher_token)
            .await;
        *self.state.lock().expect("publisher state poisoned") = match result {
            Err(PublishError::MediaBridgeUnavailable) => PublisherState::Unavailable,
            _ => PublisherState::Disconnected,
        };
        result
    }
}

/// The grant's scope, read from its compact token. The node does not check
/// the control plane's signature: a directive reaches it only through its own
/// signed, challenge-bound pull over TLS to the cloud it is linked to, which
/// is what the node trusts. What the node does check is that the grant names
/// this node, this session and this edge, and that it is current; the SFU
/// checks the publisher token.
fn read_grant(token: &str) -> Result<MediaGrant, PublishError> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || parts[0] != "nwg1" {
        return Err(PublishError::InvalidDirective);
    }
    let raw = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| PublishError::InvalidDirective)?;
    let raw = std::str::from_utf8(&raw).map_err(|_| PublishError::InvalidDirective)?;
    MediaGrant::from_canonical_json(raw).map_err(|_| PublishError::InvalidDirective)
}

struct ClaimedDirective;
impl GrantConsumer for ClaimedDirective {
    fn consume<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), PublishError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Owns the node's active room/track tasks. Directives arrive only from the
/// authenticated one-time cloud pull; this layer independently checks their
/// signature, expiry, endpoint, node, and action before creating a transport.
pub struct PublisherSupervisor {
    endpoint_policy: nw_cloud_protocol::media_session::EndpointPolicy,
    connector: std::sync::Arc<dyn RoomConnector>,
    playback: Option<std::sync::Arc<dyn PlaybackOpener>>,
    live_sources: Option<std::sync::Arc<dyn LiveSourceOpener>>,
    active: Mutex<HashMap<String, ActivePublication>>,
    status: Mutex<HashMap<String, String>>,
    inactive: tokio::sync::Notify,
}

struct ActivePublication {
    cancel: PublishCancel,
    camera_id: String,
    representation: Representation,
    expires_tx: watch::Sender<OffsetDateTime>,
    playback: Option<std::sync::Arc<PlaybackPublication>>,
}

fn spawn_publication_lease(
    mut expires_rx: watch::Receiver<OffsetDateTime>,
    cancel: PublishCancel,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let expires = *expires_rx.borrow_and_update();
            let remaining = if expires > OffsetDateTime::now_utc() {
                (expires - OffsetDateTime::now_utc()).unsigned_abs()
            } else {
                std::time::Duration::ZERO
            };
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {
                    cancel.cancel();
                    break;
                }
                changed = expires_rx.changed() => {
                    if changed.is_err() {
                        cancel.cancel();
                        break;
                    }
                }
            }
        }
    })
}

impl PublisherSupervisor {
    /// `endpoint_policy`: which signalling URLs a directive may name (the
    /// cloud link's policy). Nothing else is configured on the node.
    pub fn new(endpoint_policy: nw_cloud_protocol::media_session::EndpointPolicy) -> Self {
        Self {
            endpoint_policy,
            connector: std::sync::Arc::new(LiveKitRoomConnector::default()),
            playback: None,
            live_sources: None,
            active: Mutex::new(HashMap::new()),
            status: Mutex::new(HashMap::new()),
            inactive: tokio::sync::Notify::new(),
        }
    }

    /// Serve playback directives by playing local recordings.
    pub fn with_playback(mut self, opener: std::sync::Arc<dyn PlaybackOpener>) -> Self {
        self.playback = Some(opener);
        self
    }

    /// Serve live directives from the cameras' own compressed streams.
    pub fn with_live_sources(mut self, opener: std::sync::Arc<dyn LiveSourceOpener>) -> Self {
        self.live_sources = Some(opener);
        self
    }

    /// Let live viewers talk through the camera (two-way audio).
    pub fn with_talk(mut self, sink: std::sync::Arc<dyn TalkSink>) -> Self {
        self.connector = std::sync::Arc::new(LiveKitRoomConnector { talk: Some(sink) });
        self
    }

    /// Replace the LiveKit transport (tests use an in-memory one).
    #[cfg(test)]
    pub fn with_connector(mut self, connector: std::sync::Arc<dyn RoomConnector>) -> Self {
        self.connector = connector;
        self
    }

    /// Start (or extend) one publication. `linked_node_id` is the cloud node
    /// this installation is linked as; the directive arrived over that node's
    /// authenticated channel, and its grant must name that node.
    pub fn start(
        self: &std::sync::Arc<Self>,
        directive: PublishDirective,
        linked_node_id: Option<&str>,
    ) -> Result<(), PublishError> {
        let site = linked_node_id
            .map(str::to_owned)
            .ok_or(PublishError::ScopeMismatch)?;
        directive
            .validate_with(self.endpoint_policy)
            .map_err(|_| PublishError::InvalidDirective)?;
        let grant = read_grant(&directive.grant)?;
        // A live directive carries no playback control; a playback directive
        // must, and needs a playback engine on this node.
        let playback_control = match (grant.scope.action, directive.playback.clone()) {
            (MediaAction::Live, None) => None,
            (MediaAction::Playback, Some(control)) if self.playback.is_some() => Some(control),
            _ => return Err(PublishError::InvalidDirective),
        };
        if grant.session_id != directive.session_id
            || grant.audience != directive.sfu.region_id
            || grant.scope.site_id != site
        {
            return Err(PublishError::ScopeMismatch);
        }
        grant
            .is_usable_at(OffsetDateTime::now_utc())
            .map_err(|_| PublishError::Expired)?;
        let session_id = grant.session_id.clone();
        let expires = OffsetDateTime::parse(
            &grant.expires_at_utc,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| PublishError::InvalidDirective)?;
        {
            let active = self.active.lock().expect("publisher active map poisoned");
            if let Some(existing) = active.get(&session_id) {
                if existing.camera_id != grant.scope.camera_id
                    || existing.representation != grant.scope.representation
                {
                    return Err(PublishError::ScopeMismatch);
                }
                existing
                    .expires_tx
                    .send(expires)
                    .map_err(|_| PublishError::Transport)?;
                if let (Some(playback), Some(control)) = (&existing.playback, playback_control) {
                    let playback = std::sync::Arc::clone(playback);
                    tokio::spawn(async move { playback.apply(control).await });
                }
                return Ok(());
            }
        }
        // Live view forwards the camera's own compressed stream from a
        // dedicated source; playback replays a recording's compressed video.
        // Neither decodes on the node.
        let connector = std::sync::Arc::clone(&self.connector);
        let (bridge, cancel, playback, live_run): (Box<dyn MediaBridge>, _, _, _) =
            match (playback_control, &self.playback) {
                (Some(control), Some(opener)) => {
                    let (submitter, feed) = encoded_channel();
                    let playback = std::sync::Arc::new(PlaybackPublication::new(
                        grant.scope.camera_id.clone(),
                        submitter,
                        std::sync::Arc::clone(opener),
                    ));
                    let opening = std::sync::Arc::clone(&playback);
                    tokio::spawn(async move { opening.apply(control).await });
                    let (bridge, cancel) = EncodedMediaBridge::new(feed, connector);
                    // Viewers steer playback over the room's data channel.
                    let bridge = bridge.with_control(
                        std::sync::Arc::clone(&playback) as std::sync::Arc<dyn DataControl>
                    );
                    (Box::new(bridge), cancel, Some(playback), None)
                }
                _ => {
                    let opener = self
                        .live_sources
                        .as_ref()
                        .ok_or(PublishError::UnsupportedSource)?;
                    let (submitter, feed) = encoded_channel();
                    let run = opener.open(
                        &grant.scope.camera_id,
                        grant.scope.representation,
                        std::sync::Arc::new(submitter),
                    )?;
                    let (bridge, cancel) = EncodedMediaBridge::new(feed, connector);
                    (Box::new(bridge), cancel, None, Some(run))
                }
            };
        let (expires_tx, expires_rx) = watch::channel(expires);
        self.active
            .lock()
            .expect("publisher active map poisoned")
            .insert(
                session_id.clone(),
                ActivePublication {
                    cancel: cancel.clone(),
                    camera_id: grant.scope.camera_id.clone(),
                    representation: grant.scope.representation,
                    expires_tx,
                    playback: playback.clone(),
                },
            );
        self.status
            .lock()
            .expect("publisher status poisoned")
            .insert(session_id.clone(), "authorizing".into());
        let config = LiveKitPublisherConfig::for_directive(&directive, site, self.endpoint_policy);
        let publisher = LiveKitPublisher::new(config, ClaimedDirective, bridge);
        let supervisor = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let expiry_cancel = cancel.clone();
            let expiry = spawn_publication_lease(expires_rx, expiry_cancel);
            supervisor
                .status
                .lock()
                .expect("publisher status poisoned")
                .insert(session_id.clone(), "publishing".into());
            let result = publisher
                .publish(directive, OffsetDateTime::now_utc())
                .await;
            expiry.abort();
            if let Some(run) = live_run {
                run.stop();
            }
            if let Some(playback) = playback {
                playback.stop().await;
            }
            supervisor
                .active
                .lock()
                .expect("publisher active map poisoned")
                .remove(&session_id);
            supervisor
                .status
                .lock()
                .expect("publisher status poisoned")
                .insert(
                    session_id,
                    match result {
                        Ok(()) => "stopped",
                        Err(
                            PublishError::UnsupportedSource
                            | PublishError::CodecChanged
                            | PublishError::CodecRefused,
                        ) => "unsupported",
                        Err(_) => "failed",
                    }
                    .into(),
                );
            supervisor.inactive.notify_waiters();
        });
        Ok(())
    }

    pub async fn cancel_all(&self) {
        let sessions = {
            let active = self.active.lock().expect("publisher active map poisoned");
            for publication in active.values() {
                publication.cancel.cancel();
            }
            active.keys().cloned().collect::<HashSet<_>>()
        };
        self.wait_until_inactive(&sessions).await;
    }

    async fn wait_until_inactive(&self, sessions: &HashSet<String>) {
        loop {
            let notified = self.inactive.notified();
            if self
                .active
                .lock()
                .expect("publisher active map poisoned")
                .keys()
                .all(|session| !sessions.contains(session))
            {
                return;
            }
            notified.await;
        }
    }

    pub async fn reconcile(&self, active_session_ids: &[String]) {
        let authorized: HashSet<&str> = active_session_ids.iter().map(String::as_str).collect();
        let stopping = {
            let running = self.active.lock().expect("publisher active map poisoned");
            let mut stopping = HashSet::new();
            for (session_id, publication) in running.iter() {
                if !authorized.contains(session_id.as_str()) {
                    publication.cancel.cancel();
                    stopping.insert(session_id.clone());
                }
            }
            stopping
        };
        self.wait_until_inactive(&stopping).await;
    }

    pub fn status(&self) -> HashMap<String, String> {
        self.status
            .lock()
            .expect("publisher status poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_talk_refusal_is_a_short_json_reason() {
        let message: serde_json::Value =
            serde_json::from_slice(&talk_refusal("someone else is talking")).unwrap();
        assert_eq!(
            message,
            serde_json::json!({"refused": "someone else is talking"})
        );
        let long = talk_refusal(&"é".repeat(1000));
        let reason: serde_json::Value = serde_json::from_slice(&long).unwrap();
        assert_eq!(
            reason["refused"].as_str().unwrap().chars().count(),
            MAX_TALK_REASON
        );
    }
    use ed25519_dalek::{Signer as _, SigningKey};
    use nw_cloud_protocol::media_session::{
        EndpointPolicy, GrantScope, MediaAction, PlaybackControl, Representation, SfuEndpoint,
        PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountConsumer(AtomicUsize);
    impl GrantConsumer for CountConsumer {
        fn consume<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), PublishError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }
    struct CountBridge(AtomicUsize);
    impl MediaBridge for CountBridge {
        fn publish<'a>(
            &'a self,
            _: &'a LiveKitPublisherConfig,
            _: &'a MediaGrant,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), PublishError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(PublishError::MediaBridgeUnavailable) })
        }
    }
    struct CloseCountingTrack(std::sync::Arc<AtomicUsize>);
    impl PublishedVideoTrack for CloseCountingTrack {
        fn capture_encoded(&self, _: &EncodedAccessUnit) -> Result<(), PublishError> {
            Ok(())
        }

        fn close(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
            })
        }
    }
    struct ImmediateConnector(std::sync::Arc<AtomicUsize>);
    impl RoomConnector for ImmediateConnector {
        fn connect_and_publish<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a MediaGrant,
            _: VideoCodec,
            _: (u32, u32),
        ) -> BoxFuture<'a, Result<Box<dyn PublishedVideoTrack>, PublishError>> {
            let closes = self.0.clone();
            Box::pin(async move {
                Ok(Box::new(CloseCountingTrack(closes)) as Box<dyn PublishedVideoTrack>)
            })
        }
    }
    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }
    fn fixture() -> (LiveKitPublisherConfig, SigningKey, PublishDirective) {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let grant = MediaGrant {
            protocol_version: 1,
            grant_id: "grant-1".into(),
            session_id: "session-1".into(),
            audience: "region-1".into(),
            scope: GrantScope {
                tenant_id: "tenant-1".into(),
                site_id: "node-1".into(),
                camera_id: "camera-1".into(),
                action: MediaAction::Live,
                representation: Representation::Sub,
            },
            issued_at_utc: "2027-01-15T08:00:00Z".into(),
            not_before_utc: "2027-01-15T08:00:00Z".into(),
            expires_at_utc: "2027-01-15T08:00:30Z".into(),
            max_bitrate_kbps: 2000,
        };
        let payload = URL_SAFE_NO_PAD.encode(grant.canonical_json().unwrap());
        let signature = URL_SAFE_NO_PAD.encode(
            signing
                .sign(format!("nwg1.{payload}").as_bytes())
                .to_bytes(),
        );
        let directive = PublishDirective {
            protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
            directive_id: "directive-1".into(),
            session_id: grant.session_id.clone(),
            grant: format!("nwg1.{payload}.{signature}"),
            sfu: SfuEndpoint {
                region_id: "region-1".into(),
                signaling_url: "wss://livekit.example.test".into(),
                ice_servers: vec![],
            },
            publisher_token: PUBLISHER_TOKEN.into(),
            playback: None,
        };
        let config = LiveKitPublisherConfig::for_directive(
            &directive,
            "node-1".into(),
            EndpointPolicy::PUBLIC,
        );
        (config, signing, directive)
    }

    /// The shape of the SFU token the control plane mints; never a real one.
    const PUBLISHER_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJub2RlIn0.c2lnbmF0dXJl";

    #[tokio::test]
    async fn directives_are_published_only_for_the_linked_node_and_a_trusted_sfu() {
        let (_, signing, _) = fixture();
        let supervisor = std::sync::Arc::new(PublisherSupervisor::new(EndpointPolicy::PUBLIC));
        let directive = live_directive(&signing, "session-main", Representation::Main);
        // Not linked, or linked as another node: the grant names node-1.
        assert_eq!(
            supervisor.start(directive.clone(), None),
            Err(PublishError::ScopeMismatch)
        );
        assert_eq!(
            supervisor.start(directive.clone(), Some("node-2")),
            Err(PublishError::ScopeMismatch)
        );
        // The grant is for one edge; the directive must name that edge.
        let mut elsewhere = directive.clone();
        elsewhere.sfu.region_id = "region-2".into();
        assert_eq!(
            supervisor.start(elsewhere, Some("node-1")),
            Err(PublishError::ScopeMismatch)
        );
        // A loopback SFU only under the development opt-in.
        let mut loopback = directive;
        loopback.sfu.signaling_url = "ws://127.0.0.1:7880".into();
        assert_eq!(
            supervisor.start(loopback.clone(), Some("node-1")),
            Err(PublishError::InvalidDirective)
        );
        let development = std::sync::Arc::new(PublisherSupervisor::new(
            EndpointPolicy::DEVELOPMENT_LOOPBACK,
        ));
        assert_eq!(
            development.start(loopback, Some("node-1")),
            Err(PublishError::UnsupportedSource),
            "past every check: only the missing camera source stops it"
        );
    }

    #[test]
    fn a_publication_goes_where_its_directive_says_with_the_token_it_carries() {
        let (config, _, directive) = fixture();
        assert_eq!(config.node_id.as_deref(), Some("node-1"));
        assert_eq!(config.region_id, directive.sfu.region_id);
        assert_eq!(config.signaling_url, directive.sfu.signaling_url);
        assert_eq!(directive.publisher_token, PUBLISHER_TOKEN);
    }

    #[test]
    fn a_grant_signed_by_the_web_portal_reads_byte_for_byte() {
        // Produced by the NeuroWatch web portal's grant signer from a fixed
        // test seed; both repositories carry the same file.
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/media-grant-v1.signed.json")).unwrap();
        let token = fixture["token"].as_str().unwrap();

        let grant = read_grant(token).expect("portal grant reads");
        let expected: MediaGrant = serde_json::from_value(fixture["grant"].clone()).unwrap();
        assert_eq!(grant, expected);

        let garbled = token.replacen("nwg1.e", "nwg1.f", 1);
        assert!(read_grant(&garbled).is_err());
        assert!(read_grant(&token.replacen("nwg1.", "nwg2.", 1)).is_err());
    }

    // -- encoded live view ---------------------------------------------------------

    fn unit(index: u8, keyframe: bool) -> EncodedAccessUnit {
        EncodedAccessUnit {
            codec: VideoCodec::H264,
            payload: vec![0, 0, 0, 1, index],
            keyframe,
            width: 1280,
            height: 720,
            timestamp_us: i64::from(index),
            pts_us: None,
        }
    }

    async fn drain(feed: &EncodedFeed) -> Vec<u8> {
        let mut seen = Vec::new();
        while let Ok(Some(unit)) =
            tokio::time::timeout(std::time::Duration::from_millis(10), feed.next()).await
        {
            seen.push(unit.payload[4]);
        }
        seen
    }

    #[tokio::test]
    async fn the_encoded_feed_starts_at_a_keyframe() {
        let (submitter, feed) = encoded_channel();
        assert_eq!(
            submitter.submit(unit(1, false)),
            SubmitOutcome::DroppedQueueFull
        );
        assert_eq!(submitter.submit(unit(2, true)), SubmitOutcome::Queued);
        assert_eq!(submitter.submit(unit(3, false)), SubmitOutcome::Queued);
        assert_eq!(drain(&feed).await, vec![2, 3]);
    }

    #[tokio::test]
    async fn a_full_encoded_queue_drops_whole_gops_never_a_dependent_frame() {
        let (submitter, feed) = encoded_channel();
        assert_eq!(submitter.submit(unit(0, true)), SubmitOutcome::Queued);
        for index in 1..ENCODED_QUEUE_UNITS as u8 {
            assert_eq!(submitter.submit(unit(index, false)), SubmitOutcome::Queued);
        }
        // Full: this delta and every delta after it, until a keyframe, are dropped.
        assert_eq!(
            submitter.submit(unit(200, false)),
            SubmitOutcome::DroppedQueueFull
        );
        let queued = drain(&feed).await;
        assert_eq!(queued.len(), ENCODED_QUEUE_UNITS);
        assert_eq!(
            submitter.submit(unit(201, false)),
            SubmitOutcome::DroppedQueueFull
        );
        assert_eq!(submitter.submit(unit(202, true)), SubmitOutcome::Queued);
        assert_eq!(submitter.submit(unit(203, false)), SubmitOutcome::Queued);
        assert_eq!(drain(&feed).await, vec![202, 203]);
        assert_eq!(submitter.dropped(), 2);
    }

    #[tokio::test]
    async fn a_rebuilt_camera_connection_resumes_at_its_next_keyframe() {
        let (submitter, feed) = encoded_channel();
        submitter.submit(unit(1, true));
        submitter.resync();
        assert_eq!(
            submitter.submit(unit(2, false)),
            SubmitOutcome::DroppedQueueFull
        );
        submitter.submit(unit(3, true));
        assert_eq!(drain(&feed).await, vec![1, 3]);
    }

    #[tokio::test]
    async fn a_failed_source_ends_the_feed_with_its_reason() {
        let (submitter, feed) = encoded_channel();
        submitter.fail(PublishError::UnsupportedSource);
        assert!(feed.next().await.is_none());
        assert_eq!(feed.failure(), Some(PublishError::UnsupportedSource));
    }

    #[derive(Default)]
    struct EncodedLog {
        published: Mutex<Vec<(VideoCodec, (u32, u32))>>,
        units: Mutex<Vec<u8>>,
        timestamps: Mutex<Vec<i64>>,
        keyframe_request: AtomicBool,
        closed: AtomicUsize,
        audio_tracks: AtomicUsize,
        audio: Mutex<Vec<usize>>,
    }
    struct EncodedAudio(std::sync::Arc<EncodedLog>);
    impl PublishedAudioTrack for EncodedAudio {
        fn capture<'a>(&'a self, samples: &'a [i16]) -> BoxFuture<'a, Result<(), PublishError>> {
            self.0.audio.lock().unwrap().push(samples.len());
            Box::pin(async { Ok(()) })
        }
    }
    struct EncodedTrack(std::sync::Arc<EncodedLog>);
    impl PublishedVideoTrack for EncodedTrack {
        fn capture_encoded(&self, unit: &EncodedAccessUnit) -> Result<(), PublishError> {
            self.0.units.lock().unwrap().push(unit.payload[4]);
            self.0.timestamps.lock().unwrap().push(unit.timestamp_us);
            Ok(())
        }
        fn take_keyframe_request(&self) -> bool {
            self.0.keyframe_request.swap(false, Ordering::SeqCst)
        }
        fn close(&self) -> BoxFuture<'_, ()> {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn publish_audio(
            &self,
        ) -> BoxFuture<'_, Result<Option<Box<dyn PublishedAudioTrack>>, PublishError>> {
            self.0.audio_tracks.fetch_add(1, Ordering::SeqCst);
            let log = std::sync::Arc::clone(&self.0);
            Box::pin(async move {
                Ok(Some(
                    Box::new(EncodedAudio(log)) as Box<dyn PublishedAudioTrack>
                ))
            })
        }
    }
    struct EncodedConnector(std::sync::Arc<EncodedLog>);
    impl RoomConnector for EncodedConnector {
        fn connect_and_publish<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a MediaGrant,
            codec: VideoCodec,
            resolution: (u32, u32),
        ) -> BoxFuture<'a, Result<Box<dyn PublishedVideoTrack>, PublishError>> {
            self.0.published.lock().unwrap().push((codec, resolution));
            let log = std::sync::Arc::clone(&self.0);
            Box::pin(async move { Ok(Box::new(EncodedTrack(log)) as Box<dyn PublishedVideoTrack>) })
        }
    }

    fn live_grant() -> MediaGrant {
        let (_, _, directive) = fixture();
        read_grant(&directive.grant).unwrap()
    }

    #[tokio::test]
    async fn the_encoded_bridge_publishes_the_stream_as_it_is_from_its_first_keyframe() {
        let (config, _, _) = fixture();
        let log = std::sync::Arc::new(EncodedLog::default());
        let (submitter, feed) = encoded_channel();
        let (bridge, cancel) =
            EncodedMediaBridge::new(feed, EncodedConnector(std::sync::Arc::clone(&log)));
        let grant = live_grant();
        let publishing = tokio::spawn(async move {
            let result = bridge.publish(&config, &grant, "token").await;
            (result, bridge.captured())
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            log.published.lock().unwrap().is_empty(),
            "the room is joined only once the stream's codec and size are known"
        );
        for (index, keyframe) in [(1, false), (2, true), (3, false), (4, false)] {
            submitter.submit(unit(index, keyframe));
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while log.units.lock().unwrap().len() < 3 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        cancel.cancel();
        let (result, captured) = publishing.await.unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(
            log.published.lock().unwrap().as_slice(),
            &[(VideoCodec::H264, (1280, 720))]
        );
        assert_eq!(log.units.lock().unwrap().as_slice(), &[2, 3, 4]);
        assert_eq!(captured, 3);
        assert_eq!(log.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn camera_sound_gets_an_audio_track_once_it_arrives() {
        let (config, _, _) = fixture();
        let log = std::sync::Arc::new(EncodedLog::default());
        let (submitter, feed) = encoded_channel();
        let (bridge, cancel) =
            EncodedMediaBridge::new(feed, EncodedConnector(std::sync::Arc::clone(&log)));
        let grant = live_grant();
        let publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, "token").await });
        submitter.submit(unit(1, true));
        assert_eq!(captured(&log, 1).await, [1]);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            log.audio_tracks.load(Ordering::SeqCst),
            0,
            "no sound yet: no audio track"
        );
        submitter.submit_audio(vec![0; 480]);
        submitter.submit_audio(vec![1; 960]);
        submitter.submit_audio(Vec::new());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while log.audio.lock().unwrap().len() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        cancel.cancel();
        assert_eq!(publishing.await.unwrap(), Ok(()));
        assert_eq!(log.audio_tracks.load(Ordering::SeqCst), 1);
        assert_eq!(log.audio.lock().unwrap().as_slice(), &[480, 960]);
    }

    /// Wait until the track has been handed `count` units.
    async fn captured(log: &EncodedLog, count: usize) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while log.units.lock().unwrap().len() < count && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        log.units.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_viewer_keyframe_request_is_answered_with_the_current_gop() {
        let (config, _, _) = fixture();
        let log = std::sync::Arc::new(EncodedLog::default());
        let (submitter, feed) = encoded_channel();
        let (bridge, cancel) =
            EncodedMediaBridge::new(feed, EncodedConnector(std::sync::Arc::clone(&log)));
        let grant = live_grant();
        let publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, "token").await });
        for (index, keyframe) in [(1, true), (2, false), (3, true), (4, false), (5, false)] {
            submitter.submit(unit(index, keyframe));
        }
        assert_eq!(captured(&log, 5).await, [1, 2, 3, 4, 5]);

        // A viewer joins after the keyframe went out: it is sent again with
        // what followed it, not the older GOP. No new unit is needed (paused
        // playback has none).
        log.keyframe_request.store(true, Ordering::SeqCst);
        assert_eq!(captured(&log, 8).await[5..], [3, 4, 5]);

        // Then the stream carries on; a request answered by the camera's own
        // keyframe sends nothing again.
        log.keyframe_request.store(true, Ordering::SeqCst);
        submitter.submit(unit(6, true));
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert_eq!(log.units.lock().unwrap()[8..], [6]);

        let timestamps = log.timestamps.lock().unwrap().clone();
        assert!(
            timestamps
                .windows(2)
                .all(|pair| pair[1] >= pair[0] + MIN_FRAME_SPACING_US),
            "capture timestamps only move forward, by at least 1 ms: {timestamps:?}"
        );
        cancel.cancel();
        assert_eq!(publishing.await.unwrap(), Ok(()));
    }

    #[test]
    fn the_gop_cache_keeps_one_whole_gop_or_none() {
        let mut gop = GopCache::default();
        gop.push(&unit(1, false));
        assert!(gop.units.is_empty(), "a GOP is kept from its keyframe only");
        gop.push(&unit(2, true));
        gop.push(&unit(3, false));
        assert_eq!(
            gop.units.iter().map(|u| u.payload[4]).collect::<Vec<_>>(),
            [2, 3]
        );

        let mut big = unit(4, true);
        big.payload = vec![0; GOP_CACHE_MAX_BYTES];
        gop.push(&big);
        assert_eq!(gop.units.len(), 1);
        gop.push(&unit(5, false));
        assert!(
            gop.units.is_empty() && gop.overflowed,
            "an outsized GOP is dropped whole"
        );
        gop.push(&unit(6, false));
        assert!(gop.units.is_empty());
        gop.push(&unit(7, true));
        assert_eq!(gop.units.len(), 1);
    }

    #[tokio::test]
    async fn a_codec_change_ends_the_publication() {
        let (config, _, _) = fixture();
        let log = std::sync::Arc::new(EncodedLog::default());
        let (submitter, feed) = encoded_channel();
        let (bridge, _cancel) =
            EncodedMediaBridge::new(feed, EncodedConnector(std::sync::Arc::clone(&log)));
        submitter.submit(unit(1, true));
        submitter.submit(EncodedAccessUnit {
            codec: VideoCodec::H265,
            ..unit(2, true)
        });
        assert_eq!(
            bridge.publish(&config, &live_grant(), "token").await,
            Err(PublishError::CodecChanged)
        );
        assert_eq!(log.units.lock().unwrap().as_slice(), &[1]);
        assert_eq!(log.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stream_that_cannot_be_forwarded_fails_before_any_room_is_joined() {
        let (config, _, _) = fixture();
        let log = std::sync::Arc::new(EncodedLog::default());
        let (submitter, feed) = encoded_channel();
        let (bridge, _cancel) =
            EncodedMediaBridge::new(feed, EncodedConnector(std::sync::Arc::clone(&log)));
        submitter.fail(PublishError::UnsupportedSource);
        assert_eq!(
            bridge.publish(&config, &live_grant(), "token").await,
            Err(PublishError::UnsupportedSource)
        );
        assert!(log.published.lock().unwrap().is_empty());
    }

    /// A camera stand-in (test pictures, encoded and sent as RTP) through the
    /// production encoded pipeline and bridge to a real LiveKit server, and a
    /// subscriber that checks what it receives. Only the stand-in encodes;
    /// the subscriber decodes to count pictures, as a browser would.
    /// Time to first picture for a viewer already in the room (as a browser
    /// is) when live view starts just after one of the camera's keyframes,
    /// and for a second viewer joining late in a GOP. `NW_E2E_GOP_FRAMES`
    /// (30 FPS, default 30) sets the keyframe interval. Neither waits for the
    /// camera's next keyframe: the current GOP is sent again on request.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a LiveKit server: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn live_view_start_up_timing() {
        use futures_util::StreamExt as _;
        use gstreamer::prelude::*;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let (url, key, secret) = (
            var("NW_LIVEKIT_E2E_URL"),
            var("NW_LIVEKIT_E2E_KEY"),
            var("NW_LIVEKIT_E2E_SECRET"),
        );
        let gop: u32 = std::env::var("NW_E2E_GOP_FRAMES")
            .ok()
            .and_then(|frames| frames.parse().ok())
            .unwrap_or(30);
        let room = format!("e2e-{}", uuid::Uuid::new_v4().simple());
        let token = |identity: &str, publish: bool| {
            livekit_api::access_token::AccessToken::with_api_key(&key, &secret)
                .with_identity(identity)
                .with_grants(livekit_api::access_token::VideoGrants {
                    room_join: true,
                    room: room.clone(),
                    can_publish: Some(publish),
                    can_subscribe: Some(!publish),
                    ..Default::default()
                })
                .to_jwt()
                .unwrap()
        };
        let (viewer, mut events) =
            Room::connect(&url, &token("viewer:e2e", false), RoomOptions::default())
                .await
                .expect("viewer joins");

        gstreamer::init().unwrap();
        let (submitter, feed) = encoded_channel();
        let submitter = std::sync::Arc::new(submitter);
        let units = std::sync::Arc::clone(&submitter);
        // Armed, the gate opens just after the next keyframe (that keyframe is
        // not sent): live view asked for at the worst moment of the GOP.
        let armed = std::sync::Arc::new(AtomicBool::new(false));
        let opened_at = std::sync::Arc::new(std::sync::OnceLock::<std::time::Instant>::new());
        let (gate_armed, gate_opened) = (
            std::sync::Arc::clone(&armed),
            std::sync::Arc::clone(&opened_at),
        );
        let source = format!(
            "videotestsrc is-live=true pattern=ball ! video/x-raw,format=I420,width=640,height=360,framerate=30/1 ! \
             x264enc tune=zerolatency key-int-max={gop} bframes=0 bitrate=1500 ! rtph264pay ! \
             application/x-rtp,media=video ! parsebin name=encoded_parse"
        );
        let pipeline = crate::pipeline::encoded::build(
            &source,
            true,
            crate::pipeline::encoded::EncodedOutputs {
                unit: std::sync::Arc::new(move |unit| {
                    if gate_opened.get().is_some() {
                        units.submit(unit);
                    } else if gate_armed.load(Ordering::Relaxed) && unit.keyframe {
                        let _ = gate_opened.set(std::time::Instant::now());
                    }
                }),
                codec: std::sync::Arc::new(|_| {}),
            },
        )
        .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        // The camera has been running a while.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let (mut config, _, _) = fixture();
        config.signaling_url = url.clone();
        let mut grant = live_grant();
        grant.session_id = room.clone();
        let (bridge, cancel) = EncodedMediaBridge::new(feed, LiveKitRoomConnector::default());
        let publisher_token = token("node:e2e", true);
        let publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, &publisher_token).await });
        armed.store(true, Ordering::Relaxed);
        let track = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while let Some(event) = events.recv().await {
                if let RoomEvent::TrackSubscribed {
                    track: livekit::track::RemoteTrack::Video(track),
                    ..
                } = event
                {
                    return track;
                }
            }
            panic!("the viewer's room closed")
        })
        .await
        .expect("subscribed");
        let t0 = *opened_at.get().expect("the gate opened");
        let subscribed = t0.elapsed();
        let mut stream =
            livekit::webrtc::video_stream::native::NativeVideoStream::new(track.rtc_track());
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), stream.next())
            .await
            .ok()
            .flatten()
            .map(|_| t0.elapsed());
        eprintln!(
            "start-up timing: gop={gop} frames ({} ms), subscribed after {} ms, first picture after {:?} ms",
            u64::from(gop) * 1000 / 30,
            subscribed.as_millis(),
            first.map(|at| at.as_millis())
        );
        // A second viewer joins late in a GOP; both must keep decoding.
        let first_viewer = tokio::spawn(async move {
            let mut pictures = 0u32;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(4), async {
                while stream.next().await.is_some() {
                    pictures += 1;
                }
            })
            .await;
            pictures
        });
        tokio::time::sleep(std::time::Duration::from_millis(
            u64::from(gop) * 1000 / 30 * 3 / 4,
        ))
        .await;
        let (late, mut late_events) =
            Room::connect(&url, &token("viewer:late", false), RoomOptions::default())
                .await
                .expect("late viewer joins");
        let joined = std::time::Instant::now();
        let late_track = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = late_events.recv().await {
                if let RoomEvent::TrackSubscribed {
                    track: livekit::track::RemoteTrack::Video(track),
                    ..
                } = event
                {
                    return track;
                }
            }
            panic!("the late viewer's room closed")
        })
        .await
        .expect("late viewer subscribed");
        let mut late_stream =
            livekit::webrtc::video_stream::native::NativeVideoStream::new(late_track.rtc_track());
        let late_first =
            tokio::time::timeout(std::time::Duration::from_secs(10), late_stream.next())
                .await
                .ok()
                .flatten()
                .map(|_| joined.elapsed());
        let mut late_pictures = 0u32;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while late_stream.next().await.is_some() {
                late_pictures += 1;
            }
        })
        .await;
        let first_viewer_pictures = first_viewer.await.unwrap();
        eprintln!(
            "late viewer: first picture {:?} ms after joining, then {late_pictures} pictures in 1 s; first viewer: {first_viewer_pictures} pictures in 4 s",
            late_first.map(|at| at.as_millis())
        );
        let _ = late.close().await;
        cancel.cancel();
        let _ = publishing.await;
        pipeline.set_state(gstreamer::State::Null).unwrap();
        let _ = viewer.close().await;
        let first = first.expect("no picture reached the viewer");
        assert!(
            first < subscribed + std::time::Duration::from_secs(1),
            "the first viewer waited for the camera's next keyframe"
        );
        assert!(
            late_first.is_some_and(|at| at < std::time::Duration::from_millis(1500)),
            "the late viewer waited for the camera's next keyframe: {late_first:?}"
        );
        assert!(
            late_pictures >= 15 && first_viewer_pictures >= 60,
            "pictures stopped"
        );
    }

    /// The cadence of the first seconds a viewer sees: when each decoded
    /// picture reaches a viewer who joins `NW_E2E_JOIN_MS` (default 1500)
    /// into a GOP of `NW_E2E_GOP_FRAMES` (default 60) at 30 FPS. Reports
    /// bursts (pictures closer than 10 ms) and stalls (gaps over 100 ms):
    /// a smooth start shows neither. Measurement only; prints a summary.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a LiveKit server: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn live_view_first_seconds_cadence() {
        use futures_util::StreamExt as _;
        use gstreamer::prelude::*;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let (url, key, secret) = (
            var("NW_LIVEKIT_E2E_URL"),
            var("NW_LIVEKIT_E2E_KEY"),
            var("NW_LIVEKIT_E2E_SECRET"),
        );
        let number = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let (gop, join_ms) = (
            number("NW_E2E_GOP_FRAMES", 60),
            number("NW_E2E_JOIN_MS", 1500),
        );
        let room = format!("e2e-{}", uuid::Uuid::new_v4().simple());
        let token = |identity: &str, publish: bool| {
            livekit_api::access_token::AccessToken::with_api_key(&key, &secret)
                .with_identity(identity)
                .with_grants(livekit_api::access_token::VideoGrants {
                    room_join: true,
                    room: room.clone(),
                    can_publish: Some(publish),
                    can_subscribe: Some(!publish),
                    ..Default::default()
                })
                .to_jwt()
                .unwrap()
        };
        gstreamer::init().unwrap();
        let (submitter, feed) = encoded_channel();
        let submitter = std::sync::Arc::new(submitter);
        let units = std::sync::Arc::clone(&submitter);
        let keyframe_at = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let marks = std::sync::Arc::clone(&keyframe_at);
        let pipeline = crate::pipeline::encoded::build(
            &format!(
                "videotestsrc is-live=true pattern=ball ! video/x-raw,format=I420,width=640,height=360,framerate=30/1 ! \
                 x264enc tune=zerolatency key-int-max={gop} bframes=0 bitrate=1500 ! rtph264pay ! \
                 application/x-rtp,media=video ! parsebin name=encoded_parse"
            ),
            true,
            crate::pipeline::encoded::EncodedOutputs {
                unit: std::sync::Arc::new(move |unit| {
                    if unit.keyframe {
                        *marks.lock().unwrap() = Some(std::time::Instant::now());
                    }
                    units.submit(unit);
                }),
                codec: std::sync::Arc::new(|_| {}),
            },
        )
        .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        // NW_E2E_JOIN_MS=0: the viewer is in the room before publishing starts
        // (Start live view); otherwise it joins that long after a keyframe.
        let early = if join_ms == 0 {
            Some(
                Room::connect(&url, &token("viewer:e2e", false), RoomOptions::default())
                    .await
                    .expect("viewer joins"),
            )
        } else {
            None
        };
        let (mut config, _, _) = fixture();
        config.signaling_url = url.clone();
        let mut grant = live_grant();
        grant.session_id = room.clone();
        let (bridge, cancel) = EncodedMediaBridge::new(feed, LiveKitRoomConnector::default());
        let publisher_token = token("node:e2e", true);
        let publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, &publisher_token).await });
        let joined = std::time::Instant::now();
        let (viewer, mut events) = match early {
            Some(joined) => joined,
            None => {
                // Join `join_ms` after one of the camera's keyframes.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                loop {
                    let since = keyframe_at.lock().unwrap().map(|at| at.elapsed());
                    if since.is_some_and(|since| {
                        since.as_millis() as u64 >= join_ms
                            && (since.as_millis() as u64) < join_ms + 40
                    }) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Room::connect(&url, &token("viewer:e2e", false), RoomOptions::default())
                    .await
                    .expect("viewer joins")
            }
        };
        let joined = if join_ms == 0 {
            joined
        } else {
            std::time::Instant::now()
        };
        let track = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while let Some(event) = events.recv().await {
                if let RoomEvent::TrackSubscribed {
                    track: livekit::track::RemoteTrack::Video(track),
                    ..
                } = event
                {
                    return track;
                }
            }
            panic!("the viewer's room closed")
        })
        .await
        .expect("subscribed");
        let mut stream =
            livekit::webrtc::video_stream::native::NativeVideoStream::new(track.rtc_track());
        let mut arrivals: Vec<(std::time::Duration, i64)> = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(frame) = stream.next().await {
                arrivals.push((joined.elapsed(), frame.timestamp_us));
            }
        })
        .await;
        cancel.cancel();
        let _ = publishing.await;
        pipeline.set_state(gstreamer::State::Null).unwrap();
        let _ = viewer.close().await;

        let first = arrivals
            .first()
            .map(|(at, _)| *at)
            .expect("pictures arrived");
        let gaps: Vec<(std::time::Duration, u128)> = arrivals
            .windows(2)
            .map(|pair| (pair[1].0 - first, (pair[1].0 - pair[0].0).as_millis()))
            .collect();
        let bursts = gaps.iter().filter(|(_, gap)| *gap < 10).count();
        let stalls: Vec<String> = gaps
            .iter()
            .filter(|(_, gap)| *gap > 100)
            .map(|(at, gap)| format!("{gap} ms at +{} ms", at.as_millis()))
            .collect();
        let per_half_second: Vec<usize> = (0..8)
            .map(|half| {
                let (from, to) = (
                    first + std::time::Duration::from_millis(half * 500),
                    first + std::time::Duration::from_millis(half * 500 + 500),
                );
                arrivals
                    .iter()
                    .filter(|(at, _)| *at >= from && *at < to)
                    .count()
            })
            .collect();
        eprintln!(
            "cadence: gop={gop} join={join_ms} ms: first picture {} ms after joining; {} pictures in 5 s; \
             {bursts} burst gaps (<10 ms); stalls (>100 ms): {stalls:?}; pictures per 0.5 s: {per_half_second:?}",
            first.as_millis(),
            arrivals.len()
        );
    }

    #[tokio::test]
    #[ignore = "requires a LiveKit server: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn camera_sound_reaches_a_subscriber_as_an_audio_track() {
        use futures_util::StreamExt as _;
        use gstreamer::prelude::*;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let (url, key, secret) = (
            var("NW_LIVEKIT_E2E_URL"),
            var("NW_LIVEKIT_E2E_KEY"),
            var("NW_LIVEKIT_E2E_SECRET"),
        );
        let room = format!("e2e-{}", uuid::Uuid::new_v4().simple());
        let token = |identity: &str, publish: bool| {
            livekit_api::access_token::AccessToken::with_api_key(&key, &secret)
                .with_identity(identity)
                .with_grants(livekit_api::access_token::VideoGrants {
                    room_join: true,
                    room: room.clone(),
                    can_publish: Some(publish),
                    can_subscribe: Some(!publish),
                    ..Default::default()
                })
                .to_jwt()
                .unwrap()
        };
        let (viewer, mut events) =
            Room::connect(&url, &token("viewer:e2e", false), RoomOptions::default())
                .await
                .expect("viewer joins");

        gstreamer::init().unwrap();
        let (submitter, feed) = encoded_channel();
        let submitter = std::sync::Arc::new(submitter);
        let units = std::sync::Arc::clone(&submitter);
        let video = crate::pipeline::encoded::build(
            "videotestsrc is-live=true ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! \
             x264enc tune=zerolatency key-int-max=25 bframes=0 ! rtph264pay ! \
             application/x-rtp,media=video ! parsebin name=encoded_parse",
            true,
            crate::pipeline::encoded::EncodedOutputs {
                unit: std::sync::Arc::new(move |unit| {
                    units.submit(unit);
                }),
                codec: std::sync::Arc::new(|_| {}),
            },
        )
        .unwrap();
        // A 440 Hz tone, as the live audio connection hands it over.
        let sound = crate::pipeline::builder::build(&format!(
            "audiotestsrc is-live=true freq=440 volume=0.5 samplesperbuffer=480 ! \
             audio/x-raw,format=S16LE,layout=interleaved,rate={AUDIO_RATE},channels=1 ! appsink name=tone sync=false"
        ))
        .unwrap();
        let tone = sound
            .by_name("tone")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let samples = std::sync::Arc::clone(&submitter);
        tone.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink
                        .pull_sample()
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    if let Some(pcm) = crate::pipeline::audio::pcm_samples(&sample) {
                        samples.submit_audio(pcm);
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );
        video.set_state(gstreamer::State::Playing).unwrap();
        sound.set_state(gstreamer::State::Playing).unwrap();

        let (mut config, _, _) = fixture();
        config.signaling_url = url.clone();
        let mut grant = live_grant();
        grant.session_id = room.clone();
        let (bridge, cancel) = EncodedMediaBridge::new(feed, LiveKitRoomConnector::default());
        let publisher_token = token("node:e2e", true);
        let publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, &publisher_token).await });
        let (mut audio_track, mut video_track) = (None, None);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while let Some(event) = events.recv().await {
                if let RoomEvent::TrackSubscribed { track, .. } = event {
                    match track {
                        livekit::track::RemoteTrack::Audio(track) => audio_track = Some(track),
                        livekit::track::RemoteTrack::Video(track) => video_track = Some(track),
                    }
                }
                if audio_track.is_some() && video_track.is_some() {
                    return;
                }
            }
        })
        .await;
        assert!(video_track.is_some(), "the video track was published");
        let audio_track = audio_track.expect("the camera's sound was published as an audio track");
        let mut stream = livekit::webrtc::audio_stream::native::NativeAudioStream::new(
            audio_track.rtc_track(),
            AUDIO_RATE as i32,
            1,
        );
        let (mut received, mut loud) = (0usize, 0usize);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(frame) = stream.next().await {
                received += frame.data.len();
                loud += frame
                    .data
                    .iter()
                    .filter(|sample| sample.abs() > 2_000)
                    .count();
            }
        })
        .await;
        cancel.cancel();
        let _ = publishing.await;
        video.set_state(gstreamer::State::Null).unwrap();
        sound.set_state(gstreamer::State::Null).unwrap();
        let _ = viewer.close().await;
        eprintln!("sound: {received} samples received in 2 s, {loud} of them loud");
        assert!(
            received >= AUDIO_RATE as usize,
            "about 2 s of sound: {received}"
        );
        assert!(
            loud > received / 4,
            "the tone, not silence: {loud} of {received}"
        );
    }

    async fn encoded_live_reaches_a_subscriber(codec: VideoCodec, encoder: &str, payloader: &str) {
        let rate = std::env::var("NW_E2E_BITRATE_KBPS")
            .ok()
            .map(|kbps| format!("bitrate={kbps} "))
            .unwrap_or_default();
        use futures_util::StreamExt as _;
        use gstreamer::prelude::*;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let (url, key, secret) = (
            var("NW_LIVEKIT_E2E_URL"),
            var("NW_LIVEKIT_E2E_KEY"),
            var("NW_LIVEKIT_E2E_SECRET"),
        );
        let room = format!("e2e-{}", uuid::Uuid::new_v4().simple());
        let token = |identity: &str, publish: bool| {
            livekit_api::access_token::AccessToken::with_api_key(&key, &secret)
                .with_identity(identity)
                .with_grants(livekit_api::access_token::VideoGrants {
                    room_join: true,
                    room: room.clone(),
                    can_publish: Some(publish),
                    can_subscribe: Some(!publish),
                    ..Default::default()
                })
                .to_jwt()
                .unwrap()
        };
        let (viewer, mut events) =
            Room::connect(&url, &token("viewer:e2e", false), RoomOptions::default())
                .await
                .expect("viewer joins");

        gstreamer::init().unwrap();
        let (submitter, feed) = encoded_channel();
        let submitter = std::sync::Arc::new(submitter);
        let units = std::sync::Arc::clone(&submitter);
        let source = format!(
            "videotestsrc name=camera_source is-live=true num-buffers=600 ! \
             video/x-raw,width=320,height=240,framerate=30/1 ! \
             {encoder} name=camera_encoder tune=zerolatency key-int-max=30 {rate}! {payloader} name=camera_pay ! \
             application/x-rtp,media=video ! parsebin name=encoded_parse"
        );
        let pipeline = crate::pipeline::encoded::build(
            &source,
            true,
            crate::pipeline::encoded::EncodedOutputs {
                unit: std::sync::Arc::new(move |unit| {
                    units.submit(unit);
                }),
                codec: std::sync::Arc::new(|_| {}),
            },
        )
        .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();

        let (mut config, _, _) = fixture();
        config.signaling_url = url.clone();
        let mut grant = live_grant();
        grant.session_id = room.clone();
        let (bridge, cancel) = EncodedMediaBridge::new(feed, LiveKitRoomConnector::default());
        let publisher_token = token("node:e2e", true);
        let mut publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, &publisher_token).await });

        let (track, mime) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let event = tokio::select! {
                    event = events.recv() => event,
                    ended = &mut publishing => panic!("the publication ended early: {ended:?}"),
                };
                let Some(event) = event else { break };
                if let RoomEvent::TrackSubscribed {
                    track: livekit::track::RemoteTrack::Video(track),
                    publication,
                    ..
                } = event
                {
                    return (track, publication.mime_type());
                }
            }
            panic!("the viewer's room closed")
        })
        .await
        .expect("the viewer is sent the camera track");
        let expected = match codec {
            VideoCodec::H264 => "video/h264",
            VideoCodec::H265 => "video/h265",
        };
        assert_eq!(
            mime.to_lowercase(),
            expected,
            "the camera's own codec reaches the viewer"
        );

        let mut stream =
            livekit::webrtc::video_stream::native::NativeVideoStream::new(track.rtc_track());
        let mut pictures = 0;
        let mut size = (0, 0);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while let Some(frame) = stream.next().await {
                pictures += 1;
                size = (frame.buffer.width(), frame.buffer.height());
                if pictures >= 60 {
                    break;
                }
            }
        })
        .await;
        cancel.cancel();
        assert_eq!(publishing.await.unwrap(), Ok(()));
        pipeline.set_state(gstreamer::State::Null).unwrap();
        let _ = viewer.close().await;
        assert!(
            pictures >= 60,
            "the viewer decoded only {pictures} pictures"
        );
        assert_eq!(size, (320, 240));
        assert_eq!(submitter.dropped(), 0, "nothing was dropped on the way");
    }

    /// Browser live view on the camera's shared connection (shared-streams
    /// stage 2): a viewer joining 1.5 s into a running connection (the
    /// recorder keeps it open) starts from the hub's cached GOP, its units
    /// rewritten to Annex-B, and a real LiveKit subscriber decodes them.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a LiveKit server: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn shared_connection_live_reaches_a_livekit_subscriber() {
        use crate::streams::{Delivery, Media, StreamHubs, SubscriberPolicy};
        use futures_util::StreamExt as _;
        use gstreamer::prelude::*;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let (url, key, secret) = (
            var("NW_LIVEKIT_E2E_URL"),
            var("NW_LIVEKIT_E2E_KEY"),
            var("NW_LIVEKIT_E2E_SECRET"),
        );
        gstreamer::init().unwrap();
        // The camera stand-in: 20 s of H.264 (a keyframe every second).
        let path =
            std::env::temp_dir().join(format!("nw-e2e-hub-{}.mkv", uuid::Uuid::new_v4().simple()));
        let writer = crate::pipeline::builder::build(&format!(
            "videotestsrc num-buffers=600 ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 ! \
             x264enc key-int-max=30 bframes=0 tune=zerolatency ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        writer.set_state(gstreamer::State::Playing).unwrap();
        writer
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(60),
                &[gstreamer::MessageType::Eos],
            )
            .unwrap();
        writer.set_state(gstreamer::State::Null).unwrap();

        let hubs = std::sync::Arc::new(StreamHubs::default());
        let camera = nw_core::camera::CameraConfig::new("Cam", path.to_str().unwrap());
        let recorder = hubs
            .subscribe(&camera, SubscriberPolicy::recorder(), None)
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let room = format!("e2e-{}", uuid::Uuid::new_v4().simple());
        let token = |identity: &str, publish: bool| {
            livekit_api::access_token::AccessToken::with_api_key(&key, &secret)
                .with_identity(identity)
                .with_grants(livekit_api::access_token::VideoGrants {
                    room_join: true,
                    room: room.clone(),
                    can_publish: Some(publish),
                    can_subscribe: Some(!publish),
                    ..Default::default()
                })
                .to_jwt()
                .unwrap()
        };
        let (viewer, mut events) =
            Room::connect(&url, &token("viewer:e2e", false), RoomOptions::default())
                .await
                .expect("viewer joins");
        let (submitter, feed) = encoded_channel();
        let submitter = std::sync::Arc::new(submitter);
        let units = std::sync::Arc::clone(&submitter);
        let subscription = hubs
            .subscribe(&camera, SubscriberPolicy::live_video(), None)
            .unwrap();
        assert_eq!(
            hubs.open_count(),
            1,
            "the viewer shares the recorder's connection"
        );
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let forwarding = {
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut converter = None;
                let mut first = true;
                while !stop.load(Ordering::Relaxed) {
                    let Some(Delivery::Unit { unit, .. }) =
                        subscription.recv(std::time::Duration::from_millis(100))
                    else {
                        continue;
                    };
                    if unit.media != Media::Video {
                        continue;
                    }
                    let convert = converter.get_or_insert_with(|| {
                        crate::streams::annexb::AnnexB::from_caps(&unit.caps).unwrap()
                    });
                    let access_unit =
                        crate::cloud::live_publish::access_unit(convert, &unit).unwrap();
                    if first {
                        assert!(
                            access_unit.keyframe,
                            "the viewer starts at the cached keyframe"
                        );
                        assert!(
                            access_unit
                                .payload
                                .windows(5)
                                .any(|w| w == [0, 0, 0, 1, 0x67]),
                            "with its SPS in Annex-B"
                        );
                        first = false;
                    }
                    units.submit(access_unit);
                }
            })
        };

        let (mut config, _, _) = fixture();
        config.signaling_url = url.clone();
        let mut grant = live_grant();
        grant.session_id = room.clone();
        let (bridge, cancel) = EncodedMediaBridge::new(feed, LiveKitRoomConnector::default());
        let publisher_token = token("node:e2e", true);
        let started = std::time::Instant::now();
        let mut publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, &publisher_token).await });
        let track = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let event = tokio::select! {
                    event = events.recv() => event,
                    ended = &mut publishing => panic!("the publication ended early: {ended:?}"),
                };
                let Some(event) = event else { break };
                if let RoomEvent::TrackSubscribed {
                    track: livekit::track::RemoteTrack::Video(track),
                    ..
                } = event
                {
                    return track;
                }
            }
            panic!("the viewer's room closed")
        })
        .await
        .expect("the viewer is sent the camera track");
        let mut stream =
            livekit::webrtc::video_stream::native::NativeVideoStream::new(track.rtc_track());
        let mut pictures = 0;
        let mut first_picture = None;
        let mut size = (0, 0);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while let Some(frame) = stream.next().await {
                first_picture.get_or_insert_with(|| started.elapsed());
                pictures += 1;
                size = (frame.buffer.width(), frame.buffer.height());
                if pictures >= 60 {
                    break;
                }
            }
        })
        .await;
        cancel.cancel();
        assert_eq!(publishing.await.unwrap(), Ok(()));
        stop.store(true, Ordering::Relaxed);
        forwarding.join().unwrap();
        drop(recorder);
        let _ = viewer.close().await;
        let _ = std::fs::remove_file(&path);
        eprintln!("first decoded picture {first_picture:?} after publishing started");
        assert!(
            pictures >= 60,
            "the viewer decoded only {pictures} pictures"
        );
        assert_eq!(size, (320, 240));
        assert_eq!(submitter.dropped(), 0, "nothing was dropped on the way");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a LiveKit server: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn h264_live_reaches_a_livekit_subscriber_without_transcoding() {
        encoded_live_reaches_a_subscriber(VideoCodec::H264, "x264enc", "rtph264pay").await;
    }

    /// A recording replayed, still compressed, through the bridge to a real
    /// LiveKit server: the subscriber decodes it, as a browser would.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a LiveKit server: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn a_recording_replays_to_a_livekit_subscriber_without_transcoding() {
        use futures_util::StreamExt as _;
        use gstreamer::prelude::*;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let (url, key, secret) = (
            var("NW_LIVEKIT_E2E_URL"),
            var("NW_LIVEKIT_E2E_KEY"),
            var("NW_LIVEKIT_E2E_SECRET"),
        );
        gstreamer::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("nw-e2e-rec-{}.mkv", uuid::Uuid::new_v4().simple()));
        let writer = crate::pipeline::builder::build(&format!(
            "videotestsrc num-buffers=250 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! \
             x264enc key-int-max=25 bframes=0 bitrate=1500 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        writer.set_state(gstreamer::State::Playing).unwrap();
        writer
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(30),
                &[gstreamer::MessageType::Eos],
            )
            .unwrap();
        writer.set_state(gstreamer::State::Null).unwrap();

        let room = format!("e2e-{}", uuid::Uuid::new_v4().simple());
        let token = |identity: &str, publish: bool| {
            livekit_api::access_token::AccessToken::with_api_key(&key, &secret)
                .with_identity(identity)
                .with_grants(livekit_api::access_token::VideoGrants {
                    room_join: true,
                    room: room.clone(),
                    can_publish: Some(publish),
                    can_subscribe: Some(!publish),
                    ..Default::default()
                })
                .to_jwt()
                .unwrap()
        };
        let (viewer, mut events) =
            Room::connect(&url, &token("viewer:e2e", false), RoomOptions::default())
                .await
                .unwrap();
        // 4:2:0 like a camera: unconstrained, x264enc picks High 4:4:4, which
        // no browser decodes (and the SDK's own subscriber cannot map).
        let (submitter, feed) = encoded_channel();
        // Two seconds in: the keyframe there, then paced playback.
        let replay = crate::cloud::encoded_replay::EncodedReplay::spawn(
            vec![crate::playback::session::PlaybackSegment {
                path: path.to_string_lossy().into_owned(),
                start_ts: 1_000_000,
                end_ts: 1_010_000,
            }],
            2_000,
            100,
            true,
            std::sync::Arc::new(submitter),
        );
        let (mut config, _, _) = fixture();
        config.signaling_url = url.clone();
        let mut grant = live_grant();
        grant.session_id = room.clone();
        let (bridge, cancel) = EncodedMediaBridge::new(feed, LiveKitRoomConnector::default());
        let publisher_token = token("node:e2e", true);
        let opened = std::time::Instant::now();
        let publishing =
            tokio::spawn(async move { bridge.publish(&config, &grant, &publisher_token).await });
        let (track, mime) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while let Some(event) = events.recv().await {
                if let RoomEvent::TrackSubscribed {
                    track: livekit::track::RemoteTrack::Video(track),
                    publication,
                    ..
                } = event
                {
                    return (track, publication.mime_type());
                }
            }
            panic!("the viewer's room closed")
        })
        .await
        .expect("the viewer is sent the recording");
        assert_eq!(mime.to_lowercase(), "video/h264");
        let mut stream =
            livekit::webrtc::video_stream::native::NativeVideoStream::new(track.rtc_track());
        let mut pictures = 0;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(6), async {
            while stream.next().await.is_some() {
                if pictures == 0 {
                    eprintln!(
                        "playback: first picture {} ms after opening",
                        opened.elapsed().as_millis()
                    );
                }
                pictures += 1;
                if pictures >= 50 {
                    break;
                }
            }
        })
        .await;
        let position = replay.position_ms();
        replay.stop();
        cancel.cancel();
        assert_eq!(publishing.await.unwrap(), Ok(()));
        let _ = viewer.close().await;
        std::fs::remove_file(path).unwrap();
        assert!(
            pictures >= 50,
            "the viewer decoded only {pictures} pictures"
        );
        assert!(position.is_some_and(|ms| ms >= 1_002_000), "{position:?}");
    }

    /// Needs a server that enables H.265 (`room.enabled_codecs` with `video/h265`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a LiveKit server with H.265 enabled: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn h265_live_reaches_a_livekit_subscriber_without_transcoding() {
        encoded_live_reaches_a_subscriber(VideoCodec::H265, "x265enc", "rtph265pay").await;
    }

    /// Needs a server that does not enable H.265 (the LiveKit default).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a LiveKit server without H.265: NW_LIVEKIT_E2E_URL, NW_LIVEKIT_E2E_KEY, NW_LIVEKIT_E2E_SECRET"]
    async fn h265_is_refused_by_a_server_that_does_not_accept_it() {
        use gstreamer::prelude::*;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let (key, secret) = (var("NW_LIVEKIT_E2E_KEY"), var("NW_LIVEKIT_E2E_SECRET"));
        let mut grant = live_grant();
        grant.session_id = format!("e2e-{}", uuid::Uuid::new_v4().simple());
        let token = livekit_api::access_token::AccessToken::with_api_key(&key, &secret)
            .with_identity("node:e2e")
            .with_grants(livekit_api::access_token::VideoGrants {
                room_join: true,
                room: grant.session_id.clone(),
                can_publish: Some(true),
                can_subscribe: Some(false),
                ..Default::default()
            })
            .to_jwt()
            .unwrap();
        gstreamer::init().unwrap();
        let (submitter, feed) = encoded_channel();
        let pipeline = crate::pipeline::encoded::build(
            "videotestsrc is-live=true num-buffers=600 ! video/x-raw,width=320,height=240,framerate=30/1 ! \
             x265enc name=camera_encoder tune=zerolatency key-int-max=30 ! rtph265pay name=camera_pay ! \
             application/x-rtp,media=video ! parsebin name=encoded_parse",
            true,
            crate::pipeline::encoded::EncodedOutputs {
                unit: std::sync::Arc::new(move |unit| {
                    submitter.submit(unit);
                }),
                codec: std::sync::Arc::new(|_| {}),
            },
        )
        .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let (mut config, _, _) = fixture();
        config.signaling_url = var("NW_LIVEKIT_E2E_URL");
        let (bridge, _cancel) = EncodedMediaBridge::new(feed, LiveKitRoomConnector::default());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            bridge.publish(&config, &grant, &token),
        )
        .await
        .expect("refused within the negotiation time");
        pipeline.set_state(gstreamer::State::Null).unwrap();
        assert_eq!(result, Err(PublishError::CodecRefused));
    }

    #[test]
    fn encoded_tracks_pass_the_camera_stream_through_without_simulcast() {
        let options = encoded_publish_options(VideoCodec::H265);
        assert_eq!(options.video_encoder, VideoEncoderBackend::PreEncoded);
        assert_eq!(options.video_codec, TrackCodec::H265);
        assert!(!options.simulcast);
        let encoding = options.video_encoding.expect("an explicit ceiling");
        assert!(
            encoding.max_bitrate >= 20_000_000,
            "a ceiling below the camera's own rate would make WebRTC drop delta frames"
        );
    }

    // -- recorded playback publication ------------------------------------------

    #[derive(Default)]
    struct PlaybackLog(Mutex<Vec<String>>);
    impl PlaybackLog {
        fn push(&self, entry: String) {
            self.0.lock().unwrap().push(entry);
        }
        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }
    struct FakeRun(std::sync::Arc<PlaybackLog>);
    impl PlaybackRun for FakeRun {
        fn set_speed_percent(&self, percent: u16) {
            self.0.push(format!("speed {percent}"));
        }
        fn set_playing(&self, playing: bool) {
            self.0.push(format!("playing {playing}"));
        }
        fn stop(&self) {
            self.0.push("stop".into());
        }
    }
    struct FakeOpener(std::sync::Arc<PlaybackLog>);
    impl PlaybackOpener for FakeOpener {
        fn open<'a>(
            &'a self,
            camera_id: &'a str,
            position: OffsetDateTime,
            speed_percent: u16,
            playing: bool,
            units: std::sync::Arc<EncodedSubmitter>,
        ) -> BoxFuture<'a, Result<Box<dyn PlaybackRun>, PublishError>> {
            // As a replay does: the recording's first keyframe.
            units.submit(unit(1, true));
            Box::pin(async move {
                self.0.push(format!(
                    "open {camera_id} {} {speed_percent} {playing}",
                    position.unix_timestamp()
                ));
                Ok(Box::new(FakeRun(std::sync::Arc::clone(&self.0))) as Box<dyn PlaybackRun>)
            })
        }
    }

    fn utc(at: OffsetDateTime) -> String {
        at.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    fn signed_directive(
        signing: &SigningKey,
        directive_id: &str,
        action: MediaAction,
        playback: Option<PlaybackControl>,
    ) -> PublishDirective {
        signed_directive_for(
            signing,
            directive_id,
            "session-pb",
            action,
            Representation::Sub,
            playback,
        )
    }

    fn signed_directive_for(
        signing: &SigningKey,
        directive_id: &str,
        session_id: &str,
        action: MediaAction,
        representation: Representation,
        playback: Option<PlaybackControl>,
    ) -> PublishDirective {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
        let grant = MediaGrant {
            protocol_version: 1,
            grant_id: format!("grant-{directive_id}"),
            session_id: session_id.into(),
            audience: "region-1".into(),
            scope: GrantScope {
                tenant_id: "tenant-1".into(),
                site_id: "node-1".into(),
                camera_id: "camera-1".into(),
                action,
                representation,
            },
            issued_at_utc: utc(now),
            not_before_utc: utc(now),
            expires_at_utc: utc(now + time::Duration::seconds(50)),
            max_bitrate_kbps: 1500,
        };
        let payload = URL_SAFE_NO_PAD.encode(grant.canonical_json().unwrap());
        let signature = URL_SAFE_NO_PAD.encode(
            signing
                .sign(format!("nwg1.{payload}").as_bytes())
                .to_bytes(),
        );
        PublishDirective {
            protocol_version: PUBLISH_DIRECTIVE_PROTOCOL_VERSION,
            directive_id: directive_id.into(),
            session_id: session_id.into(),
            grant: format!("nwg1.{payload}.{signature}"),
            sfu: SfuEndpoint {
                region_id: "region-1".into(),
                signaling_url: "wss://livekit.example.test".into(),
                ice_servers: vec![],
            },
            publisher_token: PUBLISHER_TOKEN.into(),
            playback,
        }
    }

    fn control(position: i64, speed_percent: u16, playing: bool, revision: u32) -> PlaybackControl {
        PlaybackControl {
            position_at_utc: utc(OffsetDateTime::from_unix_timestamp(position).unwrap()),
            speed_percent,
            playing,
            revision,
        }
    }

    async fn settle(log: &PlaybackLog, entries: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while log.entries().len() < entries && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        log.entries()
    }

    fn playback_supervisor() -> (
        std::sync::Arc<PublisherSupervisor>,
        std::sync::Arc<PlaybackLog>,
        SigningKey,
    ) {
        let (_, signing, _) = fixture();
        let log = std::sync::Arc::new(PlaybackLog::default());
        let supervisor = std::sync::Arc::new(
            PublisherSupervisor::new(EndpointPolicy::PUBLIC)
                .with_connector(std::sync::Arc::new(ImmediateConnector(
                    std::sync::Arc::new(AtomicUsize::new(0)),
                )))
                .with_playback(std::sync::Arc::new(FakeOpener(std::sync::Arc::clone(&log)))),
        );
        (supervisor, log, signing)
    }

    // -- encoded live sources ----------------------------------------------------------

    struct FakeLiveRun(std::sync::Arc<PlaybackLog>);
    impl LiveSourceRun for FakeLiveRun {
        fn stop(&self) {
            self.0.push("stop".into());
        }
    }
    struct FakeLiveOpener {
        log: std::sync::Arc<PlaybackLog>,
        refuse: Option<PublishError>,
        units: Mutex<Vec<std::sync::Arc<EncodedSubmitter>>>,
    }
    impl LiveSourceOpener for FakeLiveOpener {
        fn open(
            &self,
            camera_id: &str,
            representation: Representation,
            units: std::sync::Arc<EncodedSubmitter>,
        ) -> Result<Box<dyn LiveSourceRun>, PublishError> {
            if let Some(error) = &self.refuse {
                return Err(error.clone());
            }
            self.log
                .push(format!("open {camera_id} {}", representation.code()));
            self.units.lock().unwrap().push(units);
            Ok(Box::new(FakeLiveRun(std::sync::Arc::clone(&self.log))))
        }
    }

    fn live_supervisor(
        refuse: Option<PublishError>,
    ) -> (
        std::sync::Arc<PublisherSupervisor>,
        std::sync::Arc<FakeLiveOpener>,
        std::sync::Arc<PlaybackLog>,
        SigningKey,
    ) {
        let (_, signing, _) = fixture();
        let log = std::sync::Arc::new(PlaybackLog::default());
        let opener = std::sync::Arc::new(FakeLiveOpener {
            log: std::sync::Arc::clone(&log),
            refuse,
            units: Mutex::new(Vec::new()),
        });
        let supervisor = PublisherSupervisor::new(EndpointPolicy::PUBLIC)
            .with_connector(std::sync::Arc::new(EncodedConnector(
                std::sync::Arc::default(),
            )))
            .with_live_sources(
                std::sync::Arc::clone(&opener) as std::sync::Arc<dyn LiveSourceOpener>
            );
        (std::sync::Arc::new(supervisor), opener, log, signing)
    }

    fn live_directive(
        signing: &SigningKey,
        session: &str,
        representation: Representation,
    ) -> PublishDirective {
        signed_directive_for(
            signing,
            &format!("d-{session}"),
            session,
            MediaAction::Live,
            representation,
            None,
        )
    }

    #[tokio::test]
    async fn each_live_stream_opens_its_own_encoded_source() {
        let (supervisor, _opener, log, signing) = live_supervisor(None);

        supervisor
            .start(
                live_directive(&signing, "session-main", Representation::Main),
                Some("node-1"),
            )
            .expect("main accepted");
        supervisor
            .start(
                live_directive(&signing, "session-sub", Representation::Sub),
                Some("node-1"),
            )
            .expect("sub accepted alongside main");

        assert_eq!(
            log.entries(),
            vec!["open camera-1 main", "open camera-1 sub"]
        );
        supervisor.cancel_all().await;
        assert_eq!(
            settle(&log, 4).await,
            vec!["open camera-1 main", "open camera-1 sub", "stop", "stop"]
        );
    }

    #[tokio::test]
    async fn a_missing_stream_is_refused_at_once() {
        let (supervisor, _opener, _log, signing) =
            live_supervisor(Some(PublishError::StreamUnavailable));
        assert_eq!(
            supervisor.start(
                live_directive(&signing, "session-sub", Representation::Sub),
                Some("node-1")
            ),
            Err(PublishError::StreamUnavailable)
        );
        assert!(supervisor.status().is_empty());
    }

    #[tokio::test]
    async fn a_stream_found_unforwardable_is_reported_unsupported() {
        let (supervisor, opener, _log, signing) = live_supervisor(None);
        supervisor
            .start(
                live_directive(&signing, "session-main", Representation::Main),
                Some("node-1"),
            )
            .expect("accepted before the camera answers");
        let units = std::sync::Arc::clone(&opener.units.lock().unwrap()[0]);
        units.fail(PublishError::UnsupportedSource);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while supervisor.status().get("session-main").map(String::as_str) != Some("unsupported")
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            supervisor.status().get("session-main").map(String::as_str),
            Some("unsupported")
        );
    }

    #[tokio::test]
    async fn without_encoded_sources_live_view_is_unsupported() {
        let (_, signing, _) = fixture();
        let supervisor = std::sync::Arc::new(PublisherSupervisor::new(EndpointPolicy::PUBLIC));
        assert_eq!(
            supervisor.start(
                live_directive(&signing, "session-main", Representation::Main),
                Some("node-1")
            ),
            Err(PublishError::UnsupportedSource)
        );
    }

    #[tokio::test]
    async fn a_playback_directive_plays_recordings_from_its_position() {
        let (supervisor, log, signing) = playback_supervisor();
        let directive = signed_directive(
            &signing,
            "d1",
            MediaAction::Playback,
            Some(control(1_790_000_000, 100, true, 1)),
        );

        supervisor
            .start(directive, Some("node-1"))
            .expect("playback accepted");

        assert_eq!(
            settle(&log, 1).await,
            vec!["open camera-1 1790000000 100 true"]
        );
        supervisor.cancel_all().await;
        assert_eq!(settle(&log, 2).await.last().unwrap(), "stop");
    }

    #[tokio::test]
    async fn newer_controls_change_speed_and_pause_in_place_and_reopen_on_seek() {
        let (supervisor, log, signing) = playback_supervisor();
        let first = control(1_790_000_000, 100, true, 1);
        supervisor
            .start(
                signed_directive(&signing, "d1", MediaAction::Playback, Some(first.clone())),
                Some("node-1"),
            )
            .unwrap();
        settle(&log, 1).await;

        supervisor
            .start(
                signed_directive(
                    &signing,
                    "d2",
                    MediaAction::Playback,
                    Some(control(1_790_000_000, 400, false, 2)),
                ),
                Some("node-1"),
            )
            .unwrap();
        settle(&log, 3).await;
        // A redelivered older revision changes nothing.
        supervisor
            .start(
                signed_directive(&signing, "d1b", MediaAction::Playback, Some(first)),
                Some("node-1"),
            )
            .unwrap();
        supervisor
            .start(
                signed_directive(
                    &signing,
                    "d3",
                    MediaAction::Playback,
                    Some(control(1_790_000_600, 400, false, 3)),
                ),
                Some("node-1"),
            )
            .unwrap();

        assert_eq!(
            settle(&log, 5).await,
            vec![
                "open camera-1 1790000000 100 true",
                "speed 400",
                "playing false",
                "stop",
                "open camera-1 1790000600 400 false",
            ]
        );
        supervisor.cancel_all().await;
    }

    // -- the playback data channel ---------------------------------------------

    /// A track with a data channel: the test injects viewer messages and
    /// reads what the node sent back.
    struct DataTrack {
        messages: Mutex<Option<mpsc::Receiver<ViewerMessage>>>,
        sent: std::sync::Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl PublishedVideoTrack for DataTrack {
        fn capture_encoded(&self, _: &EncodedAccessUnit) -> Result<(), PublishError> {
            Ok(())
        }
        fn close(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
        fn take_messages(&self) -> Option<mpsc::Receiver<ViewerMessage>> {
            self.messages.lock().unwrap().take()
        }
        fn send_data(&self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), PublishError>> {
            self.sent
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&payload).unwrap());
            Box::pin(async { Ok(()) })
        }
    }
    struct DataConnector {
        messages: Mutex<Option<mpsc::Receiver<ViewerMessage>>>,
        sent: std::sync::Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl RoomConnector for DataConnector {
        fn connect_and_publish<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a MediaGrant,
            _: VideoCodec,
            _: (u32, u32),
        ) -> BoxFuture<'a, Result<Box<dyn PublishedVideoTrack>, PublishError>> {
            let messages = self.messages.lock().unwrap().take();
            let sent = std::sync::Arc::clone(&self.sent);
            Box::pin(async move {
                Ok(Box::new(DataTrack {
                    messages: Mutex::new(messages),
                    sent,
                }) as Box<dyn PublishedVideoTrack>)
            })
        }
    }

    struct DataHarness {
        supervisor: std::sync::Arc<PublisherSupervisor>,
        log: std::sync::Arc<PlaybackLog>,
        signing: SigningKey,
        viewer: mpsc::Sender<ViewerMessage>,
        sent: std::sync::Arc<Mutex<Vec<serde_json::Value>>>,
    }

    fn data_harness() -> DataHarness {
        let (_, signing, _) = fixture();
        let log = std::sync::Arc::new(PlaybackLog::default());
        let (viewer, messages) = mpsc::channel(8);
        let sent = std::sync::Arc::new(Mutex::new(Vec::new()));
        let supervisor = std::sync::Arc::new(
            PublisherSupervisor::new(EndpointPolicy::PUBLIC)
                .with_connector(std::sync::Arc::new(DataConnector {
                    messages: Mutex::new(Some(messages)),
                    sent: std::sync::Arc::clone(&sent),
                }))
                .with_playback(std::sync::Arc::new(FakeOpener(std::sync::Arc::clone(&log)))),
        );
        DataHarness {
            supervisor,
            log,
            signing,
            viewer,
            sent,
        }
    }

    async fn from_viewer(harness: &DataHarness, identity: &str, message: serde_json::Value) {
        harness
            .viewer
            .send(ViewerMessage {
                identity: identity.into(),
                payload: message.to_string().into_bytes(),
            })
            .await
            .unwrap();
    }

    /// The node's latest state report that has applied at least `seq`.
    async fn state_after(harness: &DataHarness, seq: u64) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let latest = harness.sent.lock().unwrap().last().cloned();
            if let Some(state) = latest.filter(|state| state["seq"].as_u64() >= Some(seq)) {
                return state;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no state report for seq {seq}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn viewer_controls_over_the_data_channel_apply_at_once_and_state_goes_back() {
        let harness = data_harness();
        harness
            .supervisor
            .start(
                signed_directive(
                    &harness.signing,
                    "d1",
                    MediaAction::Playback,
                    Some(control(1_790_000_000, 100, true, 1)),
                ),
                Some("node-1"),
            )
            .unwrap();
        settle(&harness.log, 1).await;

        // Pause: applied in place, no control-plane round trip.
        from_viewer(
            &harness,
            "viewer:user-1:v1",
            serde_json::json!({"v": 1, "type": "control", "seq": 1, "playing": false}),
        )
        .await;
        assert_eq!(
            settle(&harness.log, 2).await.last().unwrap(),
            "playing false"
        );
        let state = state_after(&harness, 1).await;
        assert_eq!(state["type"], "state");
        assert_eq!(state["playing"], false);
        assert_eq!(state["error"], serde_json::Value::Null);

        // Seek, written as the browser writes it (fractional seconds).
        from_viewer(&harness, "viewer:user-1:v1", serde_json::json!({"v": 1, "type": "control", "seq": 2, "position_at": "2026-09-21T13:47:40.000Z"})).await;
        let seek_to = OffsetDateTime::parse(
            "2026-09-21T13:47:40Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(
            settle(&harness.log, 4).await[2..],
            [
                "stop".to_owned(),
                format!("open camera-1 {} 100 false", seek_to.unix_timestamp())
            ]
        );
        assert_eq!(
            state_after(&harness, 2).await["position_at"],
            "2026-09-21T13:47:40Z"
        );

        // The control plane's copy of the same change arrives later: nothing reopens.
        harness
            .supervisor
            .start(
                signed_directive(
                    &harness.signing,
                    "d2",
                    MediaAction::Playback,
                    Some(control(seek_to.unix_timestamp(), 100, false, 2)),
                ),
                Some("node-1"),
            )
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(harness.log.entries().len(), 4);
        harness.supervisor.cancel_all().await;
    }

    #[tokio::test]
    async fn a_late_cloud_copy_never_undoes_a_newer_viewer_pause() {
        let harness = data_harness();
        let start = |id: &str, control: PlaybackControl| {
            harness
                .supervisor
                .start(
                    signed_directive(&harness.signing, id, MediaAction::Playback, Some(control)),
                    Some("node-1"),
                )
                .unwrap()
        };
        start("d1", control(1_790_000_000, 100, false, 1));
        settle(&harness.log, 1).await;

        // Play, then Pause, over the channel; each is also saved with the cloud.
        for (seq, playing) in [(1, true), (2, false)] {
            from_viewer(
                &harness,
                "viewer:user-1:v1",
                serde_json::json!({"v": 1, "type": "control", "seq": seq, "playing": playing}),
            )
            .await;
        }
        assert_eq!(
            settle(&harness.log, 3).await[1..],
            ["playing true".to_owned(), "playing false".to_owned()]
        );

        // The cloud's copy of the Play lands after the Pause: still paused.
        start("d2", control(1_790_000_000, 100, true, 2));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(harness.log.entries().len(), 3);
        assert_eq!(state_after(&harness, 2).await["playing"], false);

        // Its revision is kept, so the copy of the Pause is recognised too.
        start("d3", control(1_790_000_000, 100, false, 3));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(harness.log.entries().len(), 3);
        harness.supervisor.cancel_all().await;
    }

    #[tokio::test]
    async fn only_viewers_in_order_and_valid_controls_are_applied() {
        let harness = data_harness();
        harness
            .supervisor
            .start(
                signed_directive(
                    &harness.signing,
                    "d1",
                    MediaAction::Playback,
                    Some(control(1_790_000_000, 100, true, 1)),
                ),
                Some("node-1"),
            )
            .unwrap();
        settle(&harness.log, 1).await;

        // A sender LiveKit identified as another node is not a viewer.
        from_viewer(
            &harness,
            "node:other:session",
            serde_json::json!({"v": 1, "type": "control", "seq": 1, "playing": false}),
        )
        .await;
        // Malformed and unknown messages are dropped.
        from_viewer(
            &harness,
            "viewer:u:v",
            serde_json::json!({"v": 1, "type": "control", "seq": 2, "playing": false, "url": "x"}),
        )
        .await;
        from_viewer(
            &harness,
            "viewer:u:v",
            serde_json::json!({"v": 2, "type": "control", "seq": 3, "playing": false}),
        )
        .await;
        // A valid one, then a replay of it and an older one: applied once.
        from_viewer(
            &harness,
            "viewer:u:v",
            serde_json::json!({"v": 1, "type": "control", "seq": 5, "speed_percent": 200}),
        )
        .await;
        from_viewer(
            &harness,
            "viewer:u:v",
            serde_json::json!({"v": 1, "type": "control", "seq": 5, "speed_percent": 400}),
        )
        .await;
        from_viewer(
            &harness,
            "viewer:u:v",
            serde_json::json!({"v": 1, "type": "control", "seq": 4, "speed_percent": 400}),
        )
        .await;
        // An unsupported speed is refused and reported.
        from_viewer(
            &harness,
            "viewer:u:v",
            serde_json::json!({"v": 1, "type": "control", "seq": 6, "speed_percent": 333}),
        )
        .await;

        let state = state_after(&harness, 6).await;
        assert_eq!(state["error"], "unsupported playback speed");
        assert_eq!(state["speed_percent"], 200);
        assert_eq!(
            harness.log.entries(),
            ["open camera-1 1790000000 100 true", "speed 200"]
        );
        // A sender LiveKit did not resolve is the session's viewer (see `ViewerMessage`).
        from_viewer(
            &harness,
            "",
            serde_json::json!({"v": 1, "type": "control", "seq": 7, "playing": false}),
        )
        .await;
        assert_eq!(
            settle(&harness.log, 3).await.last().unwrap(),
            "playing false"
        );
        harness.supervisor.cancel_all().await;
    }

    #[tokio::test]
    async fn live_and_playback_directives_must_carry_matching_controls() {
        let (supervisor, _log, signing) = playback_supervisor();
        let live_with_control = signed_directive(
            &signing,
            "d1",
            MediaAction::Live,
            Some(control(1_790_000_000, 100, true, 1)),
        );
        let playback_without = signed_directive(&signing, "d2", MediaAction::Playback, None);
        assert_eq!(
            supervisor.start(live_with_control, Some("node-1")),
            Err(PublishError::InvalidDirective)
        );
        assert_eq!(
            supervisor.start(playback_without, Some("node-1")),
            Err(PublishError::InvalidDirective)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn publication_lease_renews_then_fails_closed_at_the_new_deadline() {
        let now = OffsetDateTime::now_utc();
        let (expires_tx, expires_rx) = watch::channel(now + time::Duration::seconds(30));
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let lease = spawn_publication_lease(
            expires_rx,
            PublishCancel {
                sender: std::sync::Arc::new(cancel_tx),
            },
        );
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(20)).await;
        expires_tx
            .send(now + time::Duration::seconds(60))
            .expect("lease receiver remains alive");
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(20)).await;
        assert!(
            !*cancel_rx.borrow(),
            "renewed publication stopped at old deadline"
        );
        tokio::time::advance(std::time::Duration::from_secs(41)).await;
        cancel_rx.changed().await.expect("cancellation delivered");
        assert!(
            *cancel_rx.borrow(),
            "publication survived renewal failure/expiry"
        );
        lease.await.unwrap();
    }

    #[tokio::test]
    async fn bridge_cancellation_always_closes_the_published_track() {
        let (submitter, feed) = encoded_channel();
        submitter.submit(unit(1, true));
        let closes = std::sync::Arc::new(AtomicUsize::new(0));
        let (bridge, cancel) = EncodedMediaBridge::new(feed, ImmediateConnector(closes.clone()));
        let (config, _, directive) = fixture();
        let grant = read_grant(&directive.grant).unwrap();

        let task = tokio::spawn(async move { bridge.publish(&config, &grant, "token").await });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert_eq!(task.await.unwrap(), Ok(()));
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_scope_and_expiry_are_rejected_before_any_network_boundary() {
        let (config, _, mut directive) = fixture();
        directive.session_id = "another-session".into();
        let publisher = LiveKitPublisher::new(
            config,
            CountConsumer(AtomicUsize::new(0)),
            CountBridge(AtomicUsize::new(0)),
        );
        assert_eq!(
            publisher.publish(directive, now()).await,
            Err(PublishError::ScopeMismatch)
        );
        assert_eq!(publisher.consumer.0.load(Ordering::SeqCst), 0);
        assert_eq!(publisher.bridge.0.load(Ordering::SeqCst), 0);

        let (config, _, directive) = fixture();
        let publisher = LiveKitPublisher::new(
            config,
            CountConsumer(AtomicUsize::new(0)),
            CountBridge(AtomicUsize::new(0)),
        );
        assert_eq!(
            publisher
                .publish(directive, now() + time::Duration::hours(1))
                .await,
            Err(PublishError::Expired)
        );
        assert_eq!(publisher.consumer.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn consumed_grant_replay_is_rejected_before_network_and_bridge_is_explicitly_unavailable()
    {
        let (config, _, directive) = fixture();
        let publisher = LiveKitPublisher::new(
            config,
            CountConsumer(AtomicUsize::new(0)),
            CountBridge(AtomicUsize::new(0)),
        );
        assert_eq!(publisher.state(), PublisherState::Disconnected);
        assert_eq!(
            publisher.publish(directive.clone(), now()).await,
            Err(PublishError::MediaBridgeUnavailable)
        );
        assert_eq!(publisher.state(), PublisherState::Unavailable);
        assert_eq!(
            publisher.publish(directive, now()).await,
            Err(PublishError::Replay)
        );
        assert_eq!(publisher.consumer.0.load(Ordering::SeqCst), 1);
        assert_eq!(publisher.bridge.0.load(Ordering::SeqCst), 1);
    }
}
