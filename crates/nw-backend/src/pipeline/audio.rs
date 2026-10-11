//! Camera audio: what a camera sends, what can be recorded, and listening live.
//!
//! - **Recording** keeps a camera's audio as it arrives, next to its video in
//!   the Matroska segments, when the codec is one Matroska stores without
//!   transcoding ([`recordable`]); nothing is decoded.
//! - **Listening live** ([`LiveAudio`]) is a separate, audio-only connection
//!   to one camera, decoded to the computer's audio output, run only while a
//!   desktop UI's view lease asks for it (`view.update` `audio_camera_id`).
//!   It never touches the camera's video decoder or its recording.
//! - **Sound for an extension** uses the same connection
//!   ([`AudioOutput::Samples`]), decoded to mono samples instead of speakers.
//!
//! The connection asks the camera for its audio track only (`select-stream`),
//! so no copy of the video is received.

use std::sync::Arc;
use std::time::Duration;

use gstreamer::prelude::*;
use nw_core::camera::{CameraConfig, Transport};
use tokio::sync::watch;

use super::builder;

/// Pauses between attempts to (re)connect live audio.
const RETRY: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// How often a running live-audio pipeline checks for a stop.
const POLL: Duration = Duration::from_millis(100);

/// A readable name for an RTP audio encoding (`encoding-name` of an RTSP
/// stream's caps), e.g. `MPEG4-GENERIC` → `AAC`.
pub fn audio_label(encoding: &str) -> String {
    match encoding.to_ascii_uppercase().as_str() {
        "MPEG4-GENERIC" | "MP4A-LATM" => "AAC".into(),
        "PCMA" => "G.711 A-law".into(),
        "PCMU" => "G.711 µ-law".into(),
        "OPUS" => "Opus".into(),
        "L16" | "L24" => "PCM".into(),
        other if other.starts_with("G726") => "G.726".into(),
        other => other.to_owned(),
    }
}

/// Whether audio labelled `label` can be recorded as it arrives (Matroska
/// stores these without transcoding).
pub fn recordable(label: &str) -> bool {
    matches!(
        label,
        "AAC" | "G.711 A-law" | "G.711 µ-law" | "Opus" | "PCM"
    )
}

/// The audio track's label in an RTSP stream description, if `structure`
/// (one stream's RTP caps) is an audio stream.
pub fn audio_from_rtp(structure: &gstreamer::StructureRef) -> Option<String> {
    if structure.get::<String>("media").ok().as_deref() != Some("audio") {
        return None;
    }
    let encoding = structure.get::<String>("encoding-name").ok()?;
    Some(audio_label(&encoding))
}

/// Where live audio goes.
#[derive(Clone)]
pub enum AudioOutput {
    /// This computer's audio output (desktop listening).
    Speakers,
    /// Mono 16-bit samples at `rate` Hz (a browser's audio track).
    Samples {
        rate: u32,
        sink: Arc<dyn Fn(Vec<i16>) + Send + Sync>,
    },
}

/// Name of the audio-only `rtspsrc`, whose `select-stream` keeps the video out.
const AUDIO_SOURCE: &str = "audio_src";
/// Name of the appsink of [`AudioOutput::Samples`].
const AUDIO_SINK: &str = "audio_out";

/// Listening live needs an RTSP camera: audio comes on its own track there.
pub fn live_audio_supported(camera: &CameraConfig) -> bool {
    camera.url.starts_with("rtsp://") || camera.url.starts_with("rtsps://")
}

/// The pipeline that takes `camera`'s audio to `output`.
pub fn live_audio_pipeline_str(camera: &CameraConfig, output: &AudioOutput) -> String {
    let location = builder::gst_quote(&builder::url_with_credentials_str(
        &camera.url,
        camera.username.as_deref(),
        camera.password_b64.as_deref(),
    ));
    let protocols = match camera.transport {
        Transport::Tcp => "tcp",
        Transport::Udp => "udp",
    };
    let tail = match output {
        AudioOutput::Speakers => "autoaudiosink".to_owned(),
        AudioOutput::Samples { rate, .. } => format!(
            "audio/x-raw,format=S16LE,layout=interleaved,rate={rate},channels=1 \
             ! appsink name={AUDIO_SINK} sync=false max-buffers=50 drop=true"
        ),
    };
    format!(
        "rtspsrc name={AUDIO_SOURCE} location=\"{location}\" latency=200 protocols={protocols} \
           do-rtsp-keep-alive=true ! application/x-rtp,media=audio ! decodebin \
         ! audioconvert ! audioresample ! {tail}"
    )
}

/// Ask the camera for its audio track only: the video is not set up, so
/// none of it is received.
fn select_audio_only(pipeline: &gstreamer::Pipeline) {
    let Some(source) = pipeline.by_name(AUDIO_SOURCE) else {
        return;
    };
    source.connect("select-stream", false, |args| {
        let audio = args
            .get(2)
            .and_then(|caps| caps.get::<gstreamer::Caps>().ok())
            .and_then(|caps| {
                caps.structure(0)
                    .and_then(|s| s.get::<String>("media").ok())
            })
            .is_some_and(|media| media == "audio");
        Some(audio.to_value())
    });
}

/// Hand each decoded chunk of [`AudioOutput::Samples`] to its sink.
fn connect_samples(pipeline: &gstreamer::Pipeline, output: &AudioOutput) {
    let AudioOutput::Samples { sink, .. } = output else {
        return;
    };
    let Some(appsink) = pipeline
        .by_name(AUDIO_SINK)
        .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
    else {
        return;
    };
    let sink = Arc::clone(sink);
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink
                    .pull_sample()
                    .map_err(|_| gstreamer::FlowError::Error)?;
                if let Some(samples) = pcm_samples(&sample) {
                    sink(samples);
                }
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );
}

/// The 16-bit samples of an `S16LE` sample.
pub fn pcm_samples(sample: &gstreamer::Sample) -> Option<Vec<i16>> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    Some(
        map.chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect(),
    )
}

/// One camera's audio on the computer's output, until stopped.
pub struct LiveAudio {
    camera: CameraConfig,
    stop_rx: watch::Receiver<bool>,
    output: AudioOutput,
}

impl LiveAudio {
    pub fn new(camera: CameraConfig, stop_rx: watch::Receiver<bool>) -> Self {
        Self {
            camera,
            stop_rx,
            output: AudioOutput::Speakers,
        }
    }

    /// Send the audio to `output` instead of this computer's speakers.
    pub fn with_output(mut self, output: AudioOutput) -> Self {
        self.output = output;
        self
    }

    /// Runs on a blocking thread: plays, and reconnects after a failure
    /// with a growing pause, until the stop channel says so.
    pub fn run(self) {
        let cam_id = self.camera.id.clone();
        if !live_audio_supported(&self.camera) {
            tracing::info!(cam_id = %cam_id, "Live audio needs an RTSP camera; not listening");
            return;
        }
        tracing::info!(cam_id = %cam_id, "Live audio started");
        let mut failures = 0usize;
        while !self.stopped() {
            match self.play_once() {
                Ok(()) => break,
                Err(reason) => {
                    let pause = RETRY[failures.min(RETRY.len() - 1)];
                    failures += 1;
                    tracing::warn!(
                        cam_id = %cam_id,
                        reason = %super::sanitize_credentials(&reason),
                        retry_in_s = pause.as_secs(),
                        "Live audio failed"
                    );
                    if !self.rest(pause) {
                        break;
                    }
                }
            }
        }
        tracing::info!(cam_id = %cam_id, "Live audio stopped");
    }

    fn stopped(&self) -> bool {
        *self.stop_rx.borrow()
    }

    fn rest(&self, pause: Duration) -> bool {
        let mut waited = Duration::ZERO;
        while waited < pause {
            if self.stopped() {
                return false;
            }
            std::thread::sleep(POLL);
            waited += POLL;
        }
        !self.stopped()
    }

    /// Play until stopped (`Ok`) or the pipeline fails (`Err`).
    fn play_once(&self) -> Result<(), String> {
        let pipeline = builder::build(&live_audio_pipeline_str(&self.camera, &self.output))
            .map_err(|error| error.to_string())?;
        select_audio_only(&pipeline);
        connect_samples(&pipeline, &self.output);
        let outcome = (|| {
            pipeline
                .set_state(gstreamer::State::Playing)
                .map_err(|_| "audio pipeline would not start".to_owned())?;
            let bus = pipeline.bus().ok_or("audio pipeline has no bus")?;
            loop {
                if self.stopped() {
                    return Ok(());
                }
                let Some(message) =
                    bus.timed_pop(gstreamer::ClockTime::from_mseconds(POLL.as_millis() as u64))
                else {
                    continue;
                };
                match message.view() {
                    gstreamer::MessageView::Error(error) => {
                        return Err(error.error().to_string());
                    }
                    gstreamer::MessageView::Eos(_) => return Err("audio stream ended".into()),
                    _ => {}
                }
            }
        })();
        let _ = pipeline.set_state(gstreamer::State::Null);
        outcome
    }
}

/// The installed decoder of the highest rank for `caps`: `Some(None)` for raw
/// PCM, which needs none, and `None` when no decoder reads it. Picked here
/// rather than by `decodebin`, which would hold a pipeline's preroll until
/// the first sound arrives, after the first picture.
pub fn audio_decoder(caps: &gstreamer::Caps) -> Option<Option<gstreamer::ElementFactory>> {
    if caps.structure(0).is_some_and(|s| s.name() == "audio/x-raw") {
        return Some(None);
    }
    // Listed by decreasing rank.
    gstreamer::ElementFactory::factories_with_type(
        gstreamer::ElementFactoryType::DECODER | gstreamer::ElementFactoryType::MEDIA_AUDIO,
        gstreamer::Rank::MARGINAL,
    )
    .into_iter()
    .find(|factory| factory.can_sink_any_caps(caps))
    .map(Some)
}

/// A sample's place in its file (stream time), in µs.
pub fn stream_time_us(sample: &gstreamer::Sample) -> Option<i64> {
    let pts = sample.buffer()?.pts()?;
    let time = sample
        .segment()
        .and_then(|segment| {
            segment
                .downcast_ref::<gstreamer::ClockTime>()?
                .to_stream_time(pts)
        })
        .unwrap_or(pts);
    i64::try_from(time.useconds()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_audio_codecs_have_readable_names() {
        assert_eq!(audio_label("MPEG4-GENERIC"), "AAC");
        assert_eq!(audio_label("mp4a-latm"), "AAC");
        assert_eq!(audio_label("PCMA"), "G.711 A-law");
        assert_eq!(audio_label("PCMU"), "G.711 µ-law");
        assert_eq!(audio_label("OPUS"), "Opus");
        assert_eq!(audio_label("G726-32"), "G.726");
    }

    #[test]
    fn only_codecs_matroska_stores_as_they_are_are_recorded() {
        for label in ["AAC", "G.711 A-law", "G.711 µ-law", "Opus", "PCM"] {
            assert!(recordable(label), "{label}");
        }
        assert!(!recordable("G.726"));
        assert!(!recordable("UNKNOWN"));
    }

    #[test]
    fn the_audio_track_of_a_stream_description() {
        let _ = gstreamer::init();
        let audio = gstreamer::Structure::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "PCMU")
            .build();
        let video = gstreamer::Structure::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "H264")
            .build();
        assert_eq!(audio_from_rtp(&audio).as_deref(), Some("G.711 µ-law"));
        assert_eq!(audio_from_rtp(&video), None);
    }

    #[test]
    fn live_audio_listens_to_the_audio_track_only() {
        let camera = CameraConfig::new("Cam", "rtsp://10.0.0.5/main");
        let pipeline = live_audio_pipeline_str(&camera, &AudioOutput::Speakers);
        assert!(pipeline.contains("application/x-rtp,media=audio ! decodebin"));
        assert!(pipeline.ends_with("autoaudiosink"));
        let browser = live_audio_pipeline_str(
            &camera,
            &AudioOutput::Samples {
                rate: 48_000,
                sink: Arc::new(|_| {}),
            },
        );
        assert!(browser.contains("rate=48000,channels=1"));
        assert!(browser.contains("appsink name=audio_out"));
        assert!(live_audio_supported(&camera));
        assert!(!live_audio_supported(&CameraConfig::new(
            "File",
            "/videos/a.mkv"
        )));
    }
}
