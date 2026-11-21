//! Portal thumbnails without a running decoder.
//!
//! A thumbnail request never starts a decoder that keeps running. For a
//! camera the desktop is not showing, a picture comes from, in order:
//!
//! 1. **The recording's latest keyframe.** The ingest that records a camera
//!    keeps a reference to the last keyframe it passed on
//!    ([`KeyframeStore::record`]; no copy), and that one picture is decoded
//!    on request ([`decode_keyframe`]).
//! 2. **The camera's own JPEG snapshot** over ONVIF ([`CameraSnapshots`]).
//! 3. **One keyframe from the camera's stream** (its sub stream when it has
//!    one): connect, take the first keyframe, disconnect
//!    ([`grab_keyframe`]). Only for cameras nobody records, at most
//!    [`MAX_CONCURRENT_GRABS`] at a time.
//!
//! Each decodes a single picture and ends; nothing stays connected.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gstreamer::prelude::*;
use nw_core::camera::CameraConfig;

use crate::cloud::snapshots::CameraSnapshots;
use crate::pipeline::builder;

/// A recording keyframe older than this is not used (the camera stalled).
const KEYFRAME_MAX_AGE: Duration = Duration::from_secs(30);
/// Longest wait for one keyframe to decode.
const DECODE_TIMEOUT: gstreamer::ClockTime = gstreamer::ClockTime::from_seconds(3);
/// Longest wait for a camera's stream to deliver its first keyframe.
const GRAB_TIMEOUT: Duration = Duration::from_secs(8);
/// Camera connections opened for thumbnails at the same time.
pub const MAX_CONCURRENT_GRABS: usize = 4;
/// Keyframes decoded for thumbnails at the same time (each is a decoder and
/// a blocking thread); other requests wait their turn.
pub const MAX_CONCURRENT_DECODES: usize = 2;
/// Longest a thumbnail request waits for its turn to decode.
const DECODE_QUEUE_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct Keyframe {
    caps: gstreamer::Caps,
    buffer: gstreamer::Buffer,
    at: Instant,
}

/// The latest keyframe each recording ingest passed on, with its caps.
#[derive(Default)]
pub struct KeyframeStore {
    latest: Mutex<HashMap<String, Keyframe>>,
}

impl KeyframeStore {
    /// Keep `buffer` (a keyframe) as the camera's latest. Holds a reference,
    /// not a copy; called from the ingest's streaming thread.
    pub fn record(&self, camera_id: &str, caps: gstreamer::Caps, buffer: gstreamer::Buffer) {
        if let Ok(mut latest) = self.latest.lock() {
            latest.insert(
                camera_id.to_owned(),
                Keyframe {
                    caps,
                    buffer,
                    at: Instant::now(),
                },
            );
        }
    }

    pub fn forget(&self, camera_id: &str) {
        if let Ok(mut latest) = self.latest.lock() {
            latest.remove(camera_id);
        }
    }

    pub(crate) fn latest(&self, camera_id: &str) -> Option<(gstreamer::Caps, gstreamer::Buffer)> {
        let latest = self.latest.lock().ok()?;
        let keyframe = latest.get(camera_id)?;
        (keyframe.at.elapsed() < KEYFRAME_MAX_AGE)
            .then(|| (keyframe.caps.clone(), keyframe.buffer.clone()))
    }
}

/// Where a portal thumbnail comes from when no desktop view is decoding the camera.
pub struct Thumbnails {
    keyframes: std::sync::Arc<KeyframeStore>,
    snapshots: CameraSnapshots,
    grabs: tokio::sync::Semaphore,
    decodes: tokio::sync::Semaphore,
}

impl Thumbnails {
    pub fn new(keyframes: std::sync::Arc<KeyframeStore>) -> Self {
        Self {
            keyframes,
            snapshots: CameraSnapshots::default(),
            grabs: tokio::sync::Semaphore::new(MAX_CONCURRENT_GRABS),
            decodes: tokio::sync::Semaphore::new(MAX_CONCURRENT_DECODES),
        }
    }

    /// Run `work` (a keyframe decode) on a blocking thread, at most
    /// [`MAX_CONCURRENT_DECODES`] at a time; `None` when its turn does not
    /// come within [`DECODE_QUEUE_LIMIT`].
    async fn decode_capped<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Option<T> + Send + 'static,
    ) -> Option<T> {
        let _turn = tokio::time::timeout(DECODE_QUEUE_LIMIT, self.decodes.acquire())
            .await
            .ok()?
            .ok()?;
        tokio::task::spawn_blocking(work).await.ok().flatten()
    }

    /// A JPEG thumbnail (base64) for `camera`, or `None`: its recording's
    /// latest keyframe, else one from a stream of it already open (no new
    /// connection), else its own JPEG snapshot, else one keyframe from a
    /// connection made for it.
    pub async fn fetch(
        &self,
        camera: &CameraConfig,
        streams: &crate::streams::StreamHubs,
    ) -> Option<String> {
        if let Some((caps, buffer)) = self.keyframes.latest(&camera.id) {
            let decoded = self
                .decode_capped(move || decode_keyframe(&caps, &buffer))
                .await;
            if decoded.is_some() {
                return decoded;
            }
        }
        let open = (streams.clone(), camera.clone());
        let decoded = self
            .decode_capped(move || {
                let (caps, buffer) = open.0.open_keyframe(&open.1, Duration::from_secs(2))?;
                decode_keyframe(&caps, &buffer)
            })
            .await;
        if decoded.is_some() {
            return decoded;
        }
        if let Some(jpeg) = self.snapshots.fetch(camera).await {
            return Some(jpeg);
        }
        let _permit = self.grabs.acquire().await.ok()?;
        let stream = camera
            .sub_url
            .as_deref()
            .filter(|sub| !sub.is_empty())
            .unwrap_or(&camera.url);
        let url = builder::url_with_credentials_str(
            stream,
            camera.username.as_deref(),
            camera.password_b64.as_deref(),
        );
        self.decode_capped(move || {
            let (caps, buffer) = grab_keyframe(&url)?;
            decode_keyframe(&caps, &buffer)
        })
        .await
    }
}

/// Decode one keyframe into a JPEG thumbnail (base64). Blocking.
pub fn decode_keyframe(caps: &gstreamer::Caps, buffer: &gstreamer::Buffer) -> Option<String> {
    let pipeline = builder::build(
        "appsrc name=keyframe_src format=time ! decodebin ! videoconvert ! video/x-raw \
         ! appsink name=keyframe_sink max-buffers=1 sync=false",
    )
    .ok()?;
    let source = pipeline
        .by_name("keyframe_src")?
        .downcast::<gstreamer_app::AppSrc>()
        .ok()?;
    let sink = pipeline
        .by_name("keyframe_sink")?
        .downcast::<gstreamer_app::AppSink>()
        .ok()?;
    crate::pipeline::hwaccel::software_only_in(pipeline.upcast_ref());
    source.set_caps(Some(caps));
    // A shallow copy: the stream's timestamps are not this picture's.
    let mut picture = buffer.copy();
    {
        let picture = picture.make_mut();
        picture.set_pts(gstreamer::ClockTime::ZERO);
        picture.set_dts(gstreamer::ClockTime::ZERO);
    }
    let sample = (|| {
        pipeline.set_state(gstreamer::State::Playing).ok()?;
        source.push_buffer(picture).ok()?;
        source.end_of_stream().ok()?;
        sink.try_pull_sample(DECODE_TIMEOUT)
    })();
    let _ = pipeline.set_state(gstreamer::State::Null);
    crate::cloud::previews::encode_sample(sample?)
}

/// Connect to `url`, take its first keyframe and disconnect. Blocking.
pub fn grab_keyframe(url: &str) -> Option<(gstreamer::Caps, gstreamer::Buffer)> {
    let pipeline = builder::build(&crate::pipeline::encoded_probe_pipeline_str(url)).ok()?;
    let sink = pipeline
        .by_name("probe_sink")?
        .downcast::<gstreamer_app::AppSink>()
        .ok()?;
    let found = (|| {
        pipeline.set_state(gstreamer::State::Playing).ok()?;
        let deadline = Instant::now() + GRAB_TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let sample = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(
                left.as_millis().min(1000) as u64,
            ));
            let Some(sample) = sample else {
                if sink.is_eos() {
                    return None;
                }
                continue;
            };
            let (Some(caps), Some(buffer)) = (sample.caps(), sample.buffer()) else {
                continue;
            };
            if !buffer.flags().contains(gstreamer::BufferFlags::DELTA_UNIT) {
                return Some((caps.to_owned(), buffer.to_owned()));
            }
        }
        None
    })();
    let _ = pipeline.set_state(gstreamer::State::Null);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One H.264 keyframe (and the delta after it) as a camera's parser passes them on.
    fn h264_units() -> Vec<(gstreamer::Caps, gstreamer::Buffer)> {
        gstreamer::init().unwrap();
        let pipeline = builder::build(
            "videotestsrc num-buffers=2 ! video/x-raw,format=I420,width=640,height=360,framerate=15/1 \
             ! x264enc key-int-max=15 bframes=0 ! h264parse ! video/x-h264,stream-format=avc,alignment=au \
             ! appsink name=units sync=false",
        )
        .unwrap();
        let sink = pipeline
            .by_name("units")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let mut units = Vec::new();
        while let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_seconds(5)) {
            units.push((
                sample.caps().unwrap().to_owned(),
                sample.buffer().unwrap().to_owned(),
            ));
        }
        pipeline.set_state(gstreamer::State::Null).unwrap();
        units
    }

    #[tokio::test]
    async fn thumbnail_decodes_run_a_few_at_a_time() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let thumbnails = std::sync::Arc::new(Thumbnails::new(std::sync::Arc::new(
            KeyframeStore::default(),
        )));
        let now = std::sync::Arc::new(AtomicUsize::new(0));
        let most = std::sync::Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let (thumbnails, now, most) = (thumbnails.clone(), now.clone(), most.clone());
            tasks.push(tokio::spawn(async move {
                thumbnails
                    .decode_capped(move || {
                        let running = now.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(running, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(30));
                        now.fetch_sub(1, Ordering::SeqCst);
                        Some(())
                    })
                    .await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_some());
        }
        assert!(most.load(Ordering::SeqCst) <= 2, "at most two at once");
        assert!(most.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn a_recording_keyframe_decodes_into_a_jpeg_thumbnail() {
        let units = h264_units();
        let (caps, keyframe) = &units[0];
        assert!(!keyframe
            .flags()
            .contains(gstreamer::BufferFlags::DELTA_UNIT));
        let jpeg = decode_keyframe(caps, keyframe).expect("a thumbnail");
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, jpeg).unwrap();
        let image = image::load_from_memory(&bytes).unwrap();
        assert_eq!((image.width(), image.height()), (320, 180));
    }

    #[test]
    fn the_store_keeps_each_cameras_latest_keyframe_until_it_is_old_or_forgotten() {
        let units = h264_units();
        let store = KeyframeStore::default();
        assert!(store.latest("cam").is_none());
        store.record("cam", units[0].0.clone(), units[0].1.clone());
        assert!(store.latest("cam").is_some());
        assert!(store.latest("other").is_none());
        store.latest.lock().unwrap().get_mut("cam").unwrap().at -= KEYFRAME_MAX_AGE;
        assert!(
            store.latest("cam").is_none(),
            "a stalled camera's picture is not shown"
        );
        store.record("cam", units[0].0.clone(), units[0].1.clone());
        store.forget("cam");
        assert!(store.latest("cam").is_none());
    }

    /// A camera nobody records: one keyframe from its RTSP stream.
    #[test]
    #[ignore = "requires an RTSP H.264 stream at NW_RTSP_TEST_URL"]
    fn an_rtsp_camera_gives_one_keyframe_for_a_thumbnail() {
        gstreamer::init().unwrap();
        let url = std::env::var("NW_RTSP_TEST_URL").expect("NW_RTSP_TEST_URL");
        let started = Instant::now();
        let (caps, keyframe) = grab_keyframe(&url).expect("a keyframe");
        assert!(decode_keyframe(&caps, &keyframe).is_some());
        eprintln!("rtsp thumbnail in {} ms", started.elapsed().as_millis());
    }

    /// A camera not recording, with no JPEG snapshot of its own, whose
    /// other stream is open (a plugin watches it): the portal's thumbnail
    /// comes from that stream's buffer, with no connection made for it.
    #[tokio::test]
    async fn a_portal_thumbnail_comes_from_a_stream_already_open() {
        use crate::streams::{Delivery, StreamHubs, SubscriberPolicy};
        // 30 s of video: a shorter one could end before the picture is asked for
        // on a loaded machine (coverage builds), leaving no open stream.
        let fixture = crate::streams::testing::CameraFile::new(750);
        let hubs = StreamHubs::default();
        let mut camera = fixture.camera.clone();
        let open = camera.url.clone();
        // Its main stream answers nowhere: a connection made for the
        // thumbnail would fail, so a picture proves the open stream was used.
        camera.url = "rtsp://127.0.0.1:9/main".into();
        camera.sub_url = None;
        let mut watched = camera.clone();
        watched.url = open;
        let watcher = hubs
            .subscribe(&watched, SubscriberPolicy::plugin(), None)
            .unwrap();
        let watcher = tokio::task::spawn_blocking(move || {
            assert!(matches!(
                watcher.recv(Duration::from_secs(3)),
                Some(Delivery::Unit { .. })
            ));
            watcher
        })
        .await
        .unwrap();
        // The stream is marked streaming by its poll after its first unit:
        // wait for that, not only for a unit, or this races it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while hubs.streaming_source(&camera).is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let thumbnails = Thumbnails::new(std::sync::Arc::new(KeyframeStore::default()));
        let started = Instant::now();
        let jpeg = thumbnails.fetch(&camera, &hubs).await;
        assert!(
            jpeg.is_some_and(|jpeg| !jpeg.is_empty()),
            "a thumbnail from the open stream"
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(hubs.open_count(), 1, "no connection was opened for it");
        drop(watcher);
    }

    #[test]
    fn a_stream_gives_its_first_keyframe_and_is_closed() {
        gstreamer::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("nw-thumb-{}.mkv", uuid::Uuid::new_v4().simple()));
        let writer = builder::build(&format!(
            "videotestsrc num-buffers=30 ! video/x-raw,format=I420,width=320,height=240,framerate=15/1 \
             ! x264enc key-int-max=15 bframes=0 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        ))
        .unwrap();
        writer.set_state(gstreamer::State::Playing).unwrap();
        writer
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(10),
                &[gstreamer::MessageType::Eos],
            )
            .unwrap();
        writer.set_state(gstreamer::State::Null).unwrap();

        let (caps, keyframe) = grab_keyframe(path.to_str().unwrap()).expect("a keyframe");
        assert!(!keyframe
            .flags()
            .contains(gstreamer::BufferFlags::DELTA_UNIT));
        assert!(decode_keyframe(&caps, &keyframe).is_some());
        std::fs::remove_file(path).unwrap();
    }
}
