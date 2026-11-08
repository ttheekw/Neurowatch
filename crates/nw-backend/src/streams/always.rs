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
use gstreamer::prelude::*;
use nw_core::camera::CameraConfig;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const PIPELINE: &str =
    "appsrc name=src format=time is-live=true do-timestamp=false block=false max-bytes=0 ! \
     decodebin ! videoconvert ! video/x-raw,format=NV12 ! \
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
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        match decode(hubs, source, stop) {
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
fn decode(hubs: &StreamHubs, source: &CameraConfig, stop: &AtomicBool) -> Result<(), String> {
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
    let frame_ids: Arc<Mutex<frames::DecoderFrames>> = Arc::default();
    let publisher =
        decoded::Publisher::with_frames(subscription.decoded(), &pipeline, Arc::clone(&frame_ids))
            .always_on();
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                // A reference only; the copy for a plugin is made elsewhere.
                if let Ok(sample) = sink.pull_sample() {
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
    while !stop.load(Ordering::Acquire) {
        if let Some(message) =
            bus.pop_filtered(&[gstreamer::MessageType::Error, gstreamer::MessageType::Eos])
        {
            return Err(match message.view() {
                gstreamer::MessageView::Error(error) => error.error().to_string(),
                _ => "the decoder stopped".into(),
            });
        }
        match subscription.recv(Duration::from_millis(100)) {
            Some(Delivery::Unit { unit, after_gap }) if unit.media == Media::Video => {
                // A changed format goes on with the new caps: decodebin follows.
                feed.push(&unit, after_gap, true)
                    .map_err(|flow| format!("the decoder refused input: {flow:?}"))?;
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
