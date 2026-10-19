//! Two-way audio: talking through a camera's speaker over its ONVIF audio
//! backchannel (an RTSP session opened with `Require: www.onvif.org/ver20/backchannel`).
//!
//! The camera describes a send-only audio stream (`a=sendonly`); only that
//! stream is set up, so no video or sound is received. Sound from the chosen
//! source — this computer's microphone, a test tone, or PCM samples a client
//! pushes (the web portal's microphone) — is encoded to the codec the camera
//! asked for (G.711 µ-law or A-law), packed as RTP and pushed into the
//! backchannel (`push-backchannel-sample`).
//!
//! Push-to-talk: one talker at a time per recorder, holding a lease it renews
//! while the button is held ([`LEASE`]); a lapsed lease, `talk.stop` or a
//! failure ends the session and frees the camera's speaker.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer::prelude::*;
use nw_core::camera::CameraConfig;

use crate::pipeline::builder;

/// A talk session ends this long after its last renewal.
pub const LEASE: Duration = Duration::from_secs(3);
/// How long starting may take (connect, describe, set up, play).
const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);
/// Name of the backchannel `rtspsrc`.
const TALK_SOURCE: &str = "talk_src";
/// Rate of PCM samples a client pushes (`TalkSource::Samples`).
pub const SAMPLE_RATE: u32 = 48_000;

/// Where the sound comes from.
pub enum TalkSource {
    /// This computer's default microphone (desktop mode only).
    Microphone,
    /// A 440 Hz tone (to try a camera's speaker without a microphone).
    Tone,
    /// Mono 16-bit PCM at [`SAMPLE_RATE`], pushed by the caller.
    Samples(std::sync::mpsc::Receiver<Vec<i16>>),
}

impl TalkSource {
    fn launch(&self) -> &'static str {
        match self {
            Self::Microphone => "autoaudiosrc",
            Self::Tone => "audiotestsrc is-live=true wave=sine freq=440 volume=0.3",
            Self::Samples(_) => "appsrc name=talk_in is-live=true format=time do-timestamp=true \
                                 caps=audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=1",
        }
    }
}

/// The camera's send-only stream: its index and codec.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Backchannel {
    stream: u32,
    encoding: String,
    payload: u32,
    rate: i32,
}

/// The encoder and payloader for a backchannel's codec, or `None`.
fn encoder(back: &Backchannel) -> Option<String> {
    match back.encoding.to_ascii_uppercase().as_str() {
        "PCMU" => Some(format!("mulawenc ! rtppcmupay pt={}", back.payload)),
        "PCMA" => Some(format!("alawenc ! rtppcmapay pt={}", back.payload)),
        _ => None,
    }
}

fn label(encoding: &str) -> &'static str {
    match encoding.to_ascii_uppercase().as_str() {
        "PCMU" => "G.711 µ-law",
        "PCMA" => "G.711 A-law",
        _ => "unsupported",
    }
}

fn sound_pipeline(source: &TalkSource, back: &Backchannel) -> Result<String, String> {
    let encode = encoder(back).ok_or_else(|| {
        format!(
            "this camera expects {} sound, which NeuroWatch cannot send yet (G.711 only)",
            back.encoding
        )
    })?;
    Ok(format!(
        "{} ! audioconvert ! audioresample ! audio/x-raw,rate={},channels=1 ! {encode} \
         ! appsink name=talk_out sync=false max-buffers=50 drop=true",
        source.launch(),
        back.rate.max(8000)
    ))
}

struct Active {
    camera_id: String,
    client_id: String,
    until: Instant,
    stop: Arc<AtomicBool>,
}

/// Who talks through which camera, if anyone.
#[derive(Default)]
pub struct TalkService {
    active: Mutex<Option<Active>>,
}

impl TalkService {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The current talker: (camera, client), if their lease holds.
    pub fn status(&self) -> Option<(String, String)> {
        let mut active = self.lock();
        if active
            .as_ref()
            .is_some_and(|a| a.until < Instant::now() || a.stop.load(Ordering::SeqCst))
        {
            if let Some(old) = active.take() {
                old.stop.store(true, Ordering::SeqCst);
            }
        }
        active
            .as_ref()
            .map(|a| (a.camera_id.clone(), a.client_id.clone()))
    }

    /// Start talking through `camera`'s speaker; the codec once the camera
    /// accepted the backchannel.
    pub async fn start(
        self: &Arc<Self>,
        camera: &CameraConfig,
        client_id: &str,
        source: TalkSource,
    ) -> Result<String, String> {
        if let Some((camera_id, client)) = self.status() {
            if client != client_id {
                return Err(format!(
                    "someone else is talking through {}; try again when they stop",
                    if camera_id == camera.id {
                        "this camera"
                    } else {
                        "a camera"
                    }
                ));
            }
            self.stop(client_id);
        }
        if !(camera.url.starts_with("rtsp://") || camera.url.starts_with("rtsps://")) {
            return Err("talking needs an RTSP camera".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        {
            let camera = camera.clone();
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("talk".into())
                .spawn(move || run(camera, Some(source), stop, ready_tx))
                .map_err(|e| e.to_string())?;
        }
        let codec =
            match tokio::time::timeout(START_TIMEOUT + Duration::from_secs(1), ready_rx).await {
                Ok(Ok(Ok(codec))) => codec,
                Ok(Ok(Err(error))) => {
                    stop.store(true, Ordering::SeqCst);
                    return Err(error);
                }
                _ => {
                    stop.store(true, Ordering::SeqCst);
                    return Err("the camera did not answer in time".into());
                }
            };
        *self.lock() = Some(Active {
            camera_id: camera.id.clone(),
            client_id: client_id.to_owned(),
            until: Instant::now() + LEASE,
            stop: Arc::clone(&stop),
        });
        // End the session when its lease lapses.
        let service = Arc::clone(self);
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = service.status();
            }
        });
        tracing::info!(cam_id = %camera.id, codec = %codec, "Talking through the camera");
        Ok(codec)
    }

    /// Whether `camera` has a speaker NeuroWatch can talk to (its codec),
    /// asked without sending any sound.
    pub async fn probe(camera: &CameraConfig) -> Result<String, String> {
        if !(camera.url.starts_with("rtsp://") || camera.url.starts_with("rtsps://")) {
            return Err("talking needs an RTSP camera".into());
        }
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let camera = camera.clone();
        std::thread::Builder::new()
            .name("talk-probe".into())
            .spawn(move || run(camera, None, Arc::new(AtomicBool::new(false)), ready_tx))
            .map_err(|e| e.to_string())?;
        match tokio::time::timeout(START_TIMEOUT + Duration::from_secs(1), ready_rx).await {
            Ok(Ok(result)) => result,
            _ => Err("the camera did not answer in time".into()),
        }
    }

    pub fn renew(&self, client_id: &str) -> Result<(), String> {
        let mut active = self.lock();
        match active.as_mut() {
            Some(talk) if talk.client_id == client_id && !talk.stop.load(Ordering::SeqCst) => {
                talk.until = Instant::now() + LEASE;
                Ok(())
            }
            _ => Err("not talking (the session ended)".into()),
        }
    }

    /// Stop `client_id`'s session (another client's is left alone).
    pub fn stop(&self, client_id: &str) {
        let mut active = self.lock();
        if active.as_ref().is_some_and(|a| a.client_id == client_id) {
            if let Some(talk) = active.take() {
                talk.stop.store(true, Ordering::SeqCst);
                tracing::info!(cam_id = %talk.camera_id, "Stopped talking");
            }
        }
    }
}

fn caps_backchannel(stream: u32, caps: &gstreamer::Caps) -> Option<Backchannel> {
    let s = caps.structure(0)?;
    if !s.has_field("a-sendonly") || s.get::<String>("media").ok()? != "audio" {
        return None;
    }
    Some(Backchannel {
        stream,
        encoding: s.get::<String>("encoding-name").unwrap_or_default(),
        payload: s.get::<i32>("payload").map_or(0, |p| p.max(0) as u32),
        rate: s.get::<i32>("clock-rate").unwrap_or(8000),
    })
}

/// The talk session's thread: connect, push sound until stopped or failed.
/// Without a source it only checks the camera's speaker (`talk.probe`).
fn run(
    camera: CameraConfig,
    source: Option<TalkSource>,
    stop: Arc<AtomicBool>,
    ready: tokio::sync::oneshot::Sender<Result<String, String>>,
) {
    let mut ready = Some(ready);
    let mut answer = |result: Result<String, String>| {
        if let Some(ready) = ready.take() {
            let _ = ready.send(result);
        }
    };
    let outcome = talk(&camera, source, &stop, &mut answer);
    if let Err(error) = &outcome {
        let error = crate::pipeline::sanitize_credentials(error);
        tracing::warn!(cam_id = %camera.id, error = %error, "Talking failed");
        answer(Err(error));
    }
    stop.store(true, Ordering::SeqCst);
}

fn talk(
    camera: &CameraConfig,
    source: Option<TalkSource>,
    stop: &AtomicBool,
    answer: &mut impl FnMut(Result<String, String>),
) -> Result<(), String> {
    let location = builder::url_with_credentials_str(
        &camera.url,
        camera.username.as_deref(),
        camera.password_b64.as_deref(),
    );
    let connection = gstreamer::Pipeline::with_name("talk");
    let rtsp = gstreamer::ElementFactory::make("rtspsrc")
        .name(TALK_SOURCE)
        .property("location", &location)
        .property_from_str("backchannel", "onvif")
        .property_from_str("protocols", "tcp")
        .property("latency", 100u32)
        .build()
        .map_err(|_| "the RTSP source is missing".to_owned())?;
    let found: Arc<Mutex<Option<Backchannel>>> = Arc::default();
    // When the camera last offered a stream: its offer is complete a moment
    // after the last one (the source plays at once, long before that).
    let offered: Arc<Mutex<Option<Instant>>> = Arc::default();
    {
        let found = Arc::clone(&found);
        let offered = Arc::clone(&offered);
        // Only the send-only stream: nothing is received from the camera.
        rtsp.connect("select-stream", false, move |args| {
            *offered.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            let stream = args.get(1).and_then(|v| v.get::<u32>().ok()).unwrap_or(0);
            let caps = args.get(2).and_then(|v| v.get::<gstreamer::Caps>().ok());
            tracing::debug!(stream, caps = ?caps, "Camera offers a stream");
            let back = caps.and_then(|caps| caps_backchannel(stream, &caps));
            let take = back.is_some();
            if let Some(back) = back {
                *found.lock().unwrap_or_else(|p| p.into_inner()) = Some(back);
            }
            Some(take.to_value())
        });
    }
    connection.add(&rtsp).map_err(|e| e.to_string())?;
    let result = (|| {
        connection
            .set_state(gstreamer::State::Playing)
            .map_err(|_| "could not connect to the camera".to_owned())?;
        let bus = connection.bus().ok_or("no bus")?;
        let deadline = Instant::now() + START_TIMEOUT;
        // Wait until the session plays with its backchannel set up.
        let back = loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(message) =
                bus.timed_pop(gstreamer::ClockTime::from_mseconds(POLL.as_millis() as u64))
            {
                if let gstreamer::MessageView::Error(error) = message.view() {
                    return Err(error.error().to_string());
                }
            }
            let back = found.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(back) = back {
                break back;
            }
            let last_offer = *offered.lock().unwrap_or_else(|p| p.into_inner());
            if last_offer.is_some_and(|at| at.elapsed() > Duration::from_millis(800)) {
                return Err(
                    "this camera does not accept sound (it offers no ONVIF audio backchannel)"
                        .into(),
                );
            }
            if Instant::now() >= deadline {
                return Err(match back {
                    Some(_) => "the camera did not start the audio session".into(),
                    None => "the camera did not answer in time".into(),
                });
            }
        };
        let Some(source) = source else {
            // A probe: the camera has a speaker NeuroWatch can talk to.
            sound_pipeline(&TalkSource::Tone, &back)?;
            answer(Ok(label(&back.encoding).to_owned()));
            return Ok(());
        };
        let sound = builder::build(&sound_pipeline(&source, &back)?).map_err(|e| e.to_string())?;
        let appsink = sound
            .by_name("talk_out")
            .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
            .ok_or("no sound output")?;
        let rtsp_for_sink = rtsp.clone();
        let stream = back.stream;
        // GStreamer 1.24 added `push-backchannel-sample`, which borrows the
        // sample; the older `push-backchannel-buffer` takes it over, so it is
        // handed a reference of its own (else it is freed twice).
        let borrows = gstreamer::version() >= (1, 24, 0, 0);
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                    if borrows {
                        let _ = rtsp_for_sink.emit_by_name::<gstreamer::FlowReturn>(
                            "push-backchannel-sample",
                            &[&stream, &sample],
                        );
                    } else {
                        let handed_over = sample.clone();
                        let _ = rtsp_for_sink.emit_by_name::<gstreamer::FlowReturn>(
                            "push-backchannel-buffer",
                            &[&stream, &handed_over],
                        );
                        std::mem::forget(handed_over);
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );
        // Samples a client pushes go into the appsrc on their own thread.
        let feeder = if let TalkSource::Samples(samples) = source {
            let appsrc = sound
                .by_name("talk_in")
                .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
                .ok_or("no sound input")?;
            Some(std::thread::spawn(move || {
                while let Ok(chunk) = samples.recv() {
                    let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                    if appsrc
                        .push_buffer(gstreamer::Buffer::from_mut_slice(bytes))
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = appsrc.end_of_stream();
            }))
        } else {
            None
        };
        sound
            .set_state(gstreamer::State::Playing)
            .map_err(|_| "the microphone could not start".to_owned())?;
        answer(Ok(label(&back.encoding).to_owned()));
        let sound_bus = sound.bus().ok_or("no bus")?;
        let outcome = 'session: loop {
            if stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            for bus in [&bus, &sound_bus] {
                while let Some(message) = bus.pop() {
                    match message.view() {
                        gstreamer::MessageView::Error(error) => {
                            break 'session Err(error.error().to_string());
                        }
                        // The pushed samples ended, or the camera closed.
                        gstreamer::MessageView::Eos(_) => break 'session Ok(()),
                        _ => {}
                    }
                }
            }
            std::thread::sleep(POLL);
        };
        let _ = sound.set_state(gstreamer::State::Null);
        drop(feeder);
        outcome
    })();
    let _ = connection.set_state(gstreamer::State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_send_only_audio_stream_is_a_backchannel_and_g711_is_encoded() {
        let _ = gstreamer::init();
        let back = gstreamer::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "PCMU")
            .field("payload", 0i32)
            .field("clock-rate", 8000i32)
            .field("a-sendonly", "")
            .build();
        let video = gstreamer::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "H264")
            .build();
        let found = caps_backchannel(2, &back).unwrap();
        assert_eq!(
            found,
            Backchannel {
                stream: 2,
                encoding: "PCMU".into(),
                payload: 0,
                rate: 8000
            }
        );
        assert!(caps_backchannel(0, &video).is_none());
        let receive = gstreamer::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .build();
        assert!(
            caps_backchannel(1, &receive).is_none(),
            "a camera's own sound is not the speaker"
        );
        let pipeline = sound_pipeline(&TalkSource::Tone, &found).unwrap();
        assert!(pipeline.contains("mulawenc ! rtppcmupay pt=0") && pipeline.contains("rate=8000"));
        let aac = Backchannel {
            encoding: "MPEG4-GENERIC".into(),
            ..found
        };
        assert!(sound_pipeline(&TalkSource::Tone, &aac)
            .unwrap_err()
            .contains("G.711"));
        assert_eq!(label("pcma"), "G.711 A-law");
    }

    #[tokio::test]
    async fn one_talker_at_a_time_holding_a_lease() {
        let service = Arc::new(TalkService::default());
        let stop = Arc::new(AtomicBool::new(false));
        *service.lock() = Some(Active {
            camera_id: "cam".into(),
            client_id: "desk".into(),
            until: Instant::now() + LEASE,
            stop: Arc::clone(&stop),
        });
        assert_eq!(service.status(), Some(("cam".into(), "desk".into())));
        let camera = CameraConfig::new("Gate", "rtsp://127.0.0.1:1/x");
        let busy = service
            .start(&camera, "web-1", TalkSource::Tone)
            .await
            .unwrap_err();
        assert!(busy.contains("someone else"), "{busy}");
        assert!(service.renew("web-1").is_err());
        assert!(service.renew("desk").is_ok());
        service.stop("web-1");
        assert!(service.status().is_some(), "another client's session stays");
        service.stop("desk");
        assert!(stop.load(Ordering::SeqCst) && service.status().is_none());
        // A lapsed lease frees the speaker.
        let lapsed = Arc::new(AtomicBool::new(false));
        *service.lock() = Some(Active {
            camera_id: "cam".into(),
            client_id: "desk".into(),
            until: Instant::now() - Duration::from_millis(1),
            stop: Arc::clone(&lapsed),
        });
        assert!(service.status().is_none() && lapsed.load(Ordering::SeqCst));
    }
}
