//! A stream the recorder keeps decoding (`CameraConfig::always_decode`), so
//! analytics plugins use its pictures instead of decoding the camera again,
//! whether or not a desktop shows it.
//!
//! The user's explicit choice per camera stream: the one place the recorder
//! decodes video that no desktop view asked for. It reads the stream's shared
//! connection (no camera connection of its own), decodes into system-memory
//! NV12 at the stream's own size (no scaling) and publishes into the stream's
//! decoded-picture channel, where its pictures win over a desktop view's
//! (`decoded::Publisher::always_on`). Nothing is written to a desktop ring.

use super::{
    decoded, feed::VideoFeed, frames, Delivery, Media, Phase, StreamHubs, SubscriberPolicy,
};
use crate::pipeline::decoder_health::{self, DecoderHealth, Verdict};
use crate::pipeline::hwaccel;
use gstreamer::prelude::*;
use nw_core::camera::CameraConfig;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const PIPELINE: &str =
    "appsrc name=src format=time is-live=true do-timestamp=false block=false max-bytes=0 ! \
     decodebin name=decode ! videoconvert ! video/x-raw,format=NV12 ! \
     appsink name=sink max-buffers=1 drop=true sync=false emit-signals=true";

/// Waits before trying again after a failure: 2, 5, 10, then 30 s.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

pub struct AlwaysDecoder {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AlwaysDecoder {
    /// Decode `source` (a camera whose `url` is the chosen stream) until dropped.
    pub fn start(hubs: StreamHubs, source: CameraConfig, label: String) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(format!("always-decode-{}", source.id))
                .spawn(move || run(&hubs, &source, &label, &stop))?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for AlwaysDecoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(hubs: &StreamHubs, source: &CameraConfig, label: &str, stop: &AtomicBool) {
    let mut failures = 0usize;
    // Lives as long as the decoder: a hardware decoder given up stays given up.
    let mut health = DecoderHealth::default();
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        match decode(hubs, source, stop, &mut health) {
            Ok(()) => return,
            Err(reason) => {
                tracing::warn!(cam_id = %source.id, stream = label, %reason, "Always-on decoder stopped; retrying");
            }
        }
        // A run that lasted a while starts the backoff again.
        if started.elapsed() > Duration::from_secs(60) {
            failures = 0;
        }
        let wait = BACKOFF[failures.min(BACKOFF.len() - 1)];
        failures += 1;
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if stop.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// One decoder run: `Ok` when asked to stop, `Err` on a failure to retry.
fn decode(
    hubs: &StreamHubs,
    source: &CameraConfig,
    stop: &AtomicBool,
    health: &mut DecoderHealth,
) -> Result<(), String> {
    let subscription = hubs.subscribe(source, SubscriberPolicy::always_decode(), None)?;
    let pipeline = gstreamer::parse::launch(PIPELINE)
        .map_err(|error| error.to_string())?
        .downcast::<gstreamer::Pipeline>()
        .map_err(|_| "not a pipeline".to_owned())?;
    let running = Running(pipeline.clone());
    let input = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
        .ok_or("the decoder has no input")?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or("the decoder has no output")?;
    let decodebin = pipeline
        .by_name("decode")
        .ok_or("the decoder has no decodebin")?;
    health.run_started();
    if health.software() {
        hwaccel::software_only(&decodebin);
    }
    let decodebin = decodebin
        .downcast::<gstreamer::Bin>()
        .map_err(|_| "decodebin is not a bin".to_owned())?;
    let label: Arc<decoded::DecoderLabel> = Arc::default();
    let pictures = Arc::new(AtomicU64::new(0));
    let frame_ids: Arc<Mutex<frames::DecoderFrames>> = Arc::default();
    let publisher =
        decoded::Publisher::with_frames(subscription.decoded(), &pipeline, Arc::clone(&frame_ids))
            .with_decoder(Arc::clone(&label))
            .always_on();
    let counted = Arc::clone(&pictures);
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                // A reference only; the copy for a plugin is made elsewhere.
                if let Ok(sample) = sink.pull_sample() {
                    counted.fetch_add(1, Ordering::Relaxed);
                    publisher.publish(&sample);
                }
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );
    let mut feed = VideoFeed::new(input).with_frames(frame_ids);
    let bus = pipeline.bus().ok_or("the decoder has no bus")?;
    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|error| error.to_string())?;
    tracing::info!(cam_id = %source.id, "Always-on decoder running");
    let mut watch = StallWatch::new();
    while !stop.load(Ordering::Acquire) {
        watch.name_decoder(&decodebin, &label);
        if let Some(message) =
            bus.pop_filtered(&[gstreamer::MessageType::Error, gstreamer::MessageType::Eos])
        {
            let reason = match message.view() {
                gstreamer::MessageView::Error(error) => error.error().to_string(),
                _ => "the decoder stopped".into(),
            };
            if matches!(message.view(), gstreamer::MessageView::Error(_)) {
                blame(
                    health,
                    &decodebin,
                    &subscription,
                    pictures.load(Ordering::Relaxed),
                    Some(&message),
                    &source.id,
                    "always-on decoder",
                );
            }
            return Err(reason);
        }
        if watch.stalled(pictures.load(Ordering::Relaxed), &subscription)
            && blame(
                health,
                &decodebin,
                &subscription,
                pictures.load(Ordering::Relaxed),
                None,
                &source.id,
                "always-on decoder",
            ) == Verdict::Software
        {
            return Err("the hardware decoder stalled".into());
        }
        match subscription.recv(Duration::from_millis(100)) {
            Some(Delivery::Unit { unit, after_gap }) if unit.media == Media::Video => {
                // A changed format goes on with the new caps: decodebin follows.
                feed.push(&unit, after_gap, true)
                    .map_err(|flow| format!("the decoder refused input: {flow:?}"))?;
                watch.pushed();
            }
            Some(Delivery::Unit { .. }) => {}
            Some(Delivery::Discontinuity { .. }) => feed.restart(),
            None if subscription.state().phase == Phase::Closed => {
                return Err("the camera connection closed".into());
            }
            None => {}
        }
    }
    drop(running);
    Ok(())
}

/// Counts a failed run against the hardware decoder (see
/// `pipeline::decoder_health`): before the run's first picture any pipeline
/// error counts, afterwards only one raised by the decoder or a stall. Not
/// while the camera connection is down.
pub(super) fn blame(
    health: &mut DecoderHealth,
    decodebin: &gstreamer::Bin,
    subscription: &super::Subscription,
    pictures: u64,
    message: Option<&gstreamer::Message>,
    cam_id: &str,
    owner: &str,
) -> Verdict {
    if health.software() {
        return Verdict::Ignore;
    }
    if pictures > 0 {
        health.picture();
    }
    if subscription.state().phase != Phase::Streaming {
        return Verdict::Ignore;
    }
    let Some(decoder) = hwaccel::hardware_decoder_element_in(decodebin) else {
        return Verdict::Ignore;
    };
    let caused = message.is_none_or(|message| decoder_health::raised_by(message, &decoder))
        || !health.pictures_seen();
    let verdict = health.failed(Instant::now(), caused);
    let name = decoder.factory().map(|factory| factory.name().to_string());
    let decoder = name.as_deref().unwrap_or("unknown");
    let reason = decoder_health::describe(message.is_none());
    match verdict {
        Verdict::Software => {
            tracing::warn!(
                cam_id,
                decoder,
                reason,
                owner,
                "The hardware decoder failed on this stream; it decodes on the CPU from now on"
            );
        }
        Verdict::Retry => {
            tracing::warn!(cam_id, decoder, reason, owner, "The hardware decoder failed on this stream; trying it again (a second failure within 10 minutes moves to the CPU)");
        }
        Verdict::Ignore => {}
    }
    verdict
}

/// A run's bookkeeping beside its loop: when the decoder was last fed and
/// last delivered, and naming it.
pub(super) struct StallWatch {
    pictures: u64,
    delivered: Instant,
    fed: Option<Instant>,
    counted: bool,
    named: Instant,
}

impl StallWatch {
    pub(super) fn new() -> Self {
        Self {
            pictures: 0,
            delivered: Instant::now(),
            fed: None,
            counted: false,
            named: Instant::now(),
        }
    }

    pub(super) fn pushed(&mut self) {
        self.fed = Some(Instant::now());
    }

    /// Name the decoder once decodebin has plugged it (every so often, off the
    /// picture path).
    pub(super) fn name_decoder(
        &mut self,
        decodebin: &gstreamer::Bin,
        label: &decoded::DecoderLabel,
    ) {
        if label.get().is_some() || self.named.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.named = Instant::now();
        if let Some(name) = hwaccel::decoder_label_in(decodebin) {
            label.set(name);
        }
    }

    /// The decoder stalled: pictures came before, none for
    /// `decoder_health::STALL`, the camera streams and units went in just now.
    /// Once per stall.
    pub(super) fn stalled(&mut self, pictures: u64, subscription: &super::Subscription) -> bool {
        if pictures != self.pictures {
            self.pictures = pictures;
            self.delivered = Instant::now();
            self.counted = false;
            return false;
        }
        if subscription.state().phase != Phase::Streaming {
            self.delivered = Instant::now();
            return false;
        }
        let flowing = self
            .fed
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        if pictures > 0
            && !self.counted
            && flowing
            && self.delivered.elapsed() >= decoder_health::STALL
        {
            self.counted = true;
            return true;
        }
        false
    }
}

/// Stops the pipeline however the run ends.
struct Running(gstreamer::Pipeline);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.set_state(gstreamer::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_always_on_decoder_shares_identified_pictures_with_nobody_watching() {
        let fixture = super::super::testing::CameraFile::new(300);
        let hubs = StreamHubs::default();
        // A plugin's view of the stream: its decoded-picture channel.
        let watcher = hubs
            .subscribe(&fixture.camera, SubscriberPolicy::plugin(), None)
            .unwrap();
        let mut receiver = watcher.decoded().subscribe();
        let decoder =
            AlwaysDecoder::start(hubs.clone(), fixture.camera.clone(), "main".into()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut seen = None;
        while Instant::now() < deadline && seen.is_none() {
            seen = receiver.take();
            std::thread::sleep(Duration::from_millis(20));
        }
        let (frame, _) = seen.expect("decoded pictures reach the stream's channel");
        let caps = frame
            .sample
            .caps()
            .unwrap()
            .structure(0)
            .unwrap()
            .to_owned();
        assert_eq!(caps.get::<&str>("format").unwrap(), "NV12");
        assert!(
            frame.identity.is_some(),
            "pictures keep their frame identity"
        );
        // It says who decoded them and on what once the decoder is plugged.
        let deadline = Instant::now() + Duration::from_secs(5);
        while frame.source().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let source = frame.source().expect("the decoder is named");
        assert_eq!(source.by, "always_on");
        assert!(
            [
                "VA-API",
                "NVDEC",
                "Direct3D 11",
                "Direct3D 12",
                "Quick Sync",
                "V4L2",
                "CPU"
            ]
            .contains(&source.decoder),
            "{source:?}"
        );
        // It is a subscription of its own role: diagnostics flags the stream.
        let rows = hubs.diagnostics(std::slice::from_ref(&fixture.camera));
        assert!(rows[0].always_decoded, "{rows:?}");
        assert!(!rows[0]
            .roles
            .contains_key(&super::super::queue::ConsumerKind::AlwaysDecode));
        drop(decoder);
        let rows = hubs.diagnostics(std::slice::from_ref(&fixture.camera));
        assert!(!rows[0].always_decoded, "stopping it removes it");
    }
}
