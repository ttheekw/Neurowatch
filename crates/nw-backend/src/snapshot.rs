//! Independent open-source, one-picture snapshots. Cache encoded recording
//! keyframes without decoding; a request alone starts a short-lived decoder.
use crate::taps::KeyframeSink;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

pub const MAX_JPEG_BYTES: usize = 128 * 1024;
/// Longest an event waits for a keyframe at or after its start.
const KEYFRAME_WAIT: Duration = Duration::from_secs(4);
/// A plugin draws a picture some time after it arrives: the box is read this
/// long after the picture's time.
const DRAWING_DELAY_US: i64 = 800_000;
const MAX_KEYFRAME_BYTES: usize = 8 * 1024 * 1024;
const TAIL: &str = "videoconvert ! videoscale ! video/x-raw,width=640,height=360,pixel-aspect-ratio=1/1 ! jpegenc quality=75 snapshot=true ! appsink name=snapshot_sink max-buffers=1 drop=true sync=false";

#[derive(Clone)]
struct Keyframe {
    caps: gstreamer::Caps,
    buffer: gstreamer::Buffer,
    seen: Instant,
}
pub struct SnapshotService {
    streams: Arc<crate::streams::StreamHubs>,
    cache: Mutex<HashMap<String, Keyframe>>,
    used: AtomicUsize,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for SnapshotService {
    fn default() -> Self {
        Self {
            streams: Arc::default(),
            cache: Mutex::default(),
            used: AtomicUsize::new(0),
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}
impl KeyframeSink for SnapshotService {
    fn record(&self, id: &str, caps: gstreamer::Caps, buffer: gstreamer::Buffer) {
        if buffer.size() > MAX_KEYFRAME_BYTES || buffer.size() == 0 {
            return;
        }
        if let Ok(mut cache) = self.cache.try_lock() {
            let bytes = buffer.size();
            if let Some(old) = cache.insert(
                id.to_owned(),
                Keyframe {
                    caps,
                    buffer,
                    seen: Instant::now(),
                },
            ) {
                self.used.fetch_sub(old.buffer.size(), Ordering::Relaxed);
            }
            self.used.fetch_add(bytes, Ordering::Relaxed);
            while self.used.load(Ordering::Relaxed) > 32 * 1024 * 1024 || cache.len() > 1024 {
                let oldest = cache
                    .iter()
                    .min_by_key(|(_, frame)| frame.seen)
                    .map(|(id, _)| id.clone());
                if let Some(old) = oldest.and_then(|id| cache.remove(&id)) {
                    self.used.fetch_sub(old.buffer.size(), Ordering::Relaxed);
                } else {
                    break;
                }
            }
        }
    }
    fn forget(&self, id: &str) {
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(old) = cache.remove(id) {
                self.used.fetch_sub(old.buffer.size(), Ordering::Relaxed);
            }
        }
    }
}
impl SnapshotService {
    pub fn new(streams: Arc<crate::streams::StreamHubs>) -> Self {
        Self {
            streams,
            ..Self::default()
        }
    }
    fn latest(&self, id: &str) -> Option<Keyframe> {
        self.cache
            .lock()
            .ok()?
            .get(id)
            .filter(|f| f.seen.elapsed() < Duration::from_secs(10))
            .cloned()
    }
    pub async fn capture(&self, camera: nw_core::camera::CameraConfig) -> Result<Vec<u8>, String> {
        let shared = crate::streams::enabled()
            && crate::pipeline::builder::shared_view_supported(&camera.url);
        self.capture_from(camera, shared).await
    }

    async fn capture_from(
        &self,
        camera: nw_core::camera::CameraConfig,
        shared: bool,
    ) -> Result<Vec<u8>, String> {
        self.capture_inner(camera, shared, None)
            .await
            .map(|(jpeg, _)| jpeg)
    }

    /// A picture of the first keyframe whose time is at or after `min_us`
    /// (µs since the Unix epoch, the picture clock of plugin drawings), with
    /// that time; waits a few seconds for it on an open stream. Without one
    /// in time (or without an open stream) it gives what [`Self::capture`]
    /// gives, with no time.
    pub async fn capture_since(
        &self,
        camera: nw_core::camera::CameraConfig,
        min_us: i64,
    ) -> Result<(Vec<u8>, Option<i64>), String> {
        let shared = crate::streams::enabled()
            && crate::pipeline::builder::shared_view_supported(&camera.url);
        self.capture_inner(camera, shared, Some(min_us)).await
    }

    async fn capture_inner(
        &self,
        camera: nw_core::camera::CameraConfig,
        shared: bool,
        since_us: Option<i64>,
    ) -> Result<(Vec<u8>, Option<i64>), String> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Snapshot workers are busy; try again".to_owned())?;
        let frame = self.latest(&camera.id);
        let streams = Arc::clone(&self.streams);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if let Some(min_us) = since_us.filter(|_| shared) {
                if let Some((caps, buffer, time_us)) =
                    streams.keyframe_since(&camera, min_us, KEYFRAME_WAIT)
                {
                    let picked = Keyframe {
                        caps,
                        buffer,
                        seen: Instant::now(),
                    };
                    if let Ok(jpeg) = decode_keyframe(&picked) {
                        return Ok((jpeg, Some(time_us)));
                    }
                }
            }
            if let Some(frame) = frame {
                if let Ok(jpeg) = decode_keyframe(&frame) {
                    return Ok((jpeg, None));
                }
            }
            if shared {
                return decode_keyframe(&shared_keyframe(&streams, &camera)?)
                    .map(|jpeg| (jpeg, None));
            }
            let pipeline = crate::pipeline::builder::build(&stream_snapshot_pipeline(&camera))
                .map_err(|_| "The snapshot stream could not be opened".to_owned())?;
            take_picture(&pipeline).map(|jpeg| (jpeg, None))
        })
        .await
        .map_err(|_| "Snapshot worker stopped".to_owned())?
    }
}

fn shared_keyframe(
    streams: &crate::streams::StreamHubs,
    camera: &nw_core::camera::CameraConfig,
) -> Result<Keyframe, String> {
    use crate::streams::{Delivery, SubscriberPolicy, Unit};
    // A stream already connected gives its cached keyframe at once: never
    // connect to the camera's main stream only for a picture (a thumbnail is
    // small) while its sub stream is open.
    if let Some((caps, buffer)) = streams.open_keyframe(camera, Duration::from_secs(2)) {
        return Ok(Keyframe {
            caps,
            buffer,
            seen: Instant::now(),
        });
    }
    let subscription = streams.subscribe(camera, SubscriberPolicy::snapshot(), None)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(Delivery::Unit { unit, .. }) =
            subscription.recv(deadline.saturating_duration_since(Instant::now()))
        {
            if unit.keyframe() {
                return Ok(Keyframe {
                    caps: unit.caps,
                    buffer: unit.buffer,
                    seen: Instant::now(),
                });
            }
        }
        if subscription.state().phase == crate::streams::Phase::Closed {
            break;
        }
    }
    Err("No snapshot keyframe arrived within five seconds".into())
}

/// One picture straight from the camera's stream. A stream joined between
/// keyframes would otherwise decode to a grey smear (`avdec_*` outputs
/// pictures whose reference frames it never saw): only keyframes reach the
/// decoder, so the first picture is a whole one.
fn stream_snapshot_pipeline(camera: &nw_core::camera::CameraConfig) -> String {
    let source = crate::pipeline::builder::select_pipeline(
        camera,
        crate::pipeline::hwaccel::HwAccelKind::Software,
    );
    let prefix = source
        .split("appsink name=frame_sink")
        .next()
        .unwrap_or("")
        .replace("video/x-raw,format=RGB", "video/x-raw")
        .replace(
            "application/x-rtp,media=video ! decodebin",
            "application/x-rtp,media=video ! parsebin ! \
             identity drop-buffer-flags=delta-unit silent=true ! decodebin",
        );
    format!("{prefix}{TAIL}")
}

/// Keep the cloud's keyframes (thumbnails) and local snapshots independent, with no decoder
/// or file I/O on recording's streaming thread.
pub struct KeyframeFanout {
    pub local: Arc<SnapshotService>,
    pub cloud: Option<Arc<dyn KeyframeSink>>,
}
impl KeyframeSink for KeyframeFanout {
    fn record(&self, id: &str, caps: gstreamer::Caps, buffer: gstreamer::Buffer) {
        self.local.record(id, caps.clone(), buffer.clone());
        if let Some(cloud) = &self.cloud {
            cloud.record(id, caps, buffer);
        }
    }
    fn forget(&self, id: &str) {
        self.local.forget(id);
        if let Some(cloud) = &self.cloud {
            cloud.forget(id);
        }
    }
}

fn decode_keyframe(frame: &Keyframe) -> Result<Vec<u8>, String> {
    let pipeline = crate::pipeline::builder::build(&format!(
        "appsrc name=snapshot_source format=time ! decodebin ! {TAIL}"
    ))
    .map_err(|_| "Snapshot decoder unavailable".to_owned())?;
    let _guard = StopPipeline(pipeline.clone());
    let source = pipeline
        .by_name("snapshot_source")
        .ok_or("Snapshot source missing")?
        .downcast::<gstreamer_app::AppSrc>()
        .map_err(|_| "Invalid snapshot source")?;
    source.set_caps(Some(&frame.caps));
    let mut buffer = frame.buffer.copy();
    buffer.make_mut().set_pts(gstreamer::ClockTime::ZERO);
    buffer.make_mut().set_dts(gstreamer::ClockTime::ZERO);
    source
        .push_buffer(buffer)
        .map_err(|_| "Snapshot input failed")?;
    source
        .end_of_stream()
        .map_err(|_| "Snapshot input failed")?;
    take_picture(&pipeline)
}
struct StopPipeline(gstreamer::Pipeline);
impl Drop for StopPipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gstreamer::State::Null);
    }
}
fn take_picture(pipeline: &gstreamer::Pipeline) -> Result<Vec<u8>, String> {
    let _guard = StopPipeline(pipeline.clone());
    let sink = pipeline
        .by_name("snapshot_sink")
        .ok_or("Snapshot output missing")?
        .downcast::<gstreamer_app::AppSink>()
        .map_err(|_| "Invalid snapshot output")?;
    crate::pipeline::hwaccel::software_only_in(pipeline.upcast_ref());
    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|_| "Snapshot stream failed")?;
    let sample = sink
        .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
        .ok_or("No snapshot picture arrived within five seconds")?;
    let data = sample
        .buffer()
        .ok_or("Snapshot picture missing")?
        .map_readable()
        .map_err(|_| "Snapshot picture unreadable")?;
    if data.len() > MAX_JPEG_BYTES || !data.as_slice().starts_with(&[255, 216]) {
        return Err("Snapshot picture is too large or invalid".into());
    }
    Ok(data.as_slice().to_vec())
}

pub fn thumbnail_path(relative: &str) -> Result<PathBuf, String> {
    let id = relative
        .strip_suffix(".jpg")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| "Invalid event picture path".to_owned())?;
    if relative != format!("{id}.jpg") {
        return Err("Invalid event picture path".into());
    }
    Ok(crate::events::events_dir().join(relative))
}

/// Only two image requests can be active; event producers never wait for them.
pub async fn run(
    state: Arc<tokio::sync::RwLock<crate::app_state::AppState>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    use nw_core::event::EventPhase;
    let mut notices = state.read().await.events.subscribe();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            notice = notices.recv() => {
                let notice = match notice { Ok(notice) => notice, Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue, Err(_) => break };
                if notice.phase != EventPhase::Start || !notice.event.kind.is_activity() || tasks.len() >= 2 { continue; }
                let Some(camera_id) = notice.event.camera_id.clone() else { continue; };
                let (camera, pictures, hub, db, plugins) = {
                    let state = state.read().await;
                    (state.config.cameras.iter().find(|c| c.id == camera_id).cloned(), Arc::clone(&state.snapshots), Arc::clone(&state.events), Arc::clone(&state.db), Arc::clone(&state.plugins))
                };
                let Some(camera) = camera else { continue; };
                tasks.spawn(async move {
                    // A plugin's event: the picture is the first keyframe at or
                    // after its start, and its box the drawing of that picture.
                    let from_plugin = notice.event.source == nw_core::event::EventSource::Plugin;
                    let captured = if from_plugin {
                        pictures.capture_since(camera, notice.event.start_ms.saturating_mul(1000)).await
                    } else {
                        pictures.capture(camera).await.map(|jpeg| (jpeg, None))
                    };
                    let Ok((jpeg, picture_us)) = captured else { return; };
                    let relative = format!("{}.jpg", notice.event.id);
                    let Ok(path) = thumbnail_path(&relative) else { return; };
                    let saved = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
                        let temporary = path.with_extension("jpg.part");
                        std::fs::write(&temporary, jpeg)?; std::fs::rename(temporary, path)
                    }).await;
                    if !matches!(saved, Ok(Ok(()))) { return; }
                    let event_box = match (picture_us, from_plugin) {
                        (Some(picture_us), true) => {
                            let wait = (picture_us + DRAWING_DELAY_US - crate::events::now_ms() * 1000).max(0);
                            tokio::time::sleep(Duration::from_micros(wait as u64)).await;
                            let history = plugins.overlays.history.clone();
                            let event = notice.event.clone();
                            let reference = hub.open_event(event.id).and_then(|open| open.bbox);
                            let servers = event.topic.as_deref().map(|topic| {
                                let mut servers = plugins.servers_of_manifest(topic);
                                servers.push(topic.to_owned());
                                servers
                            }).unwrap_or_default();
                            tokio::task::spawn_blocking(move || {
                                let history = history?;
                                let camera_id = event.camera_id.as_deref()?;
                                                                crate::plugin_host::event_box::lookup(&history, camera_id, &servers, picture_us, event.detail.as_deref(), reference)
                            }).await.ok().flatten()
                        }
                        _ => None,
                    };
                    let id = notice.event.id;
                    let box_in_hub = event_box;
                    let updated = hub.update(id, |event| {
                        event.thumbnail = Some(relative.clone());
                        if box_in_hub.is_some() { event.bbox = box_in_hub; }
                    });
                    if updated.is_none() {
                        let _ = tokio::task::spawn_blocking(move || {
                            hub.flush();
                            if let Ok(conn) = db.lock() {
                                let _ = conn.execute("UPDATE events SET thumbnail=?1 WHERE id=?2", rusqlite::params![relative, id]);
                                if let Some(rect) = box_in_hub {
                                    if let Ok(json) = serde_json::to_string(&rect) { let _ = conn.execute("UPDATE events SET bbox=?1 WHERE id=?2", rusqlite::params![json, id]); }
                                }
                            }
                        }).await;
                    }
                });
            }
        }
    }
    tasks.abort_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taps::KeyframeSink;

    /// Tests that point event pictures at a private data directory run one at a time.
    static DATA_HOME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A camera whose sub stream is open (a plugin watches it) gives its
    /// snapshot from there: its main stream is never connected for a picture.
    #[tokio::test]
    async fn a_snapshot_uses_the_open_sub_stream_instead_of_connecting_main() {
        use crate::streams::{Delivery, StreamHubs, SubscriberPolicy};
        // 30 s of video: a shorter one could end before the picture is asked for
        // on a loaded machine (coverage builds), leaving no open stream.
        let fixture = crate::streams::testing::CameraFile::new(750);
        let streams = Arc::new(StreamHubs::default());
        let service = SnapshotService::new(Arc::clone(&streams));
        let mut camera = fixture.camera.clone();
        let sub = camera.url.clone();
        // Main: nothing answers there; connecting would only fail.
        camera.url = "rtsp://127.0.0.1:9/main".into();
        camera.sub_url = Some(sub.clone());
        let mut sub_source = camera.clone();
        sub_source.url = sub;
        let watcher = streams
            .subscribe(&sub_source, SubscriberPolicy::plugin(), None)
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
        while streams.streaming_source(&camera).is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let started = Instant::now();
        let jpeg = service.capture_from(camera, true).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the cached keyframe, at once"
        );
        assert!(jpeg.starts_with(&[255, 216]));
        assert_eq!(streams.open_count(), 1, "main was never connected");
        drop(watcher);
    }

    #[tokio::test]
    async fn snapshot_fallback_reuses_an_open_hub_and_closes_an_unwatched_one() {
        use crate::streams::{Delivery, StreamHubs, SubscriberPolicy};
        // 30 s of video: a shorter one could end before the picture is asked for
        // on a loaded machine (coverage builds), leaving no open stream.
        let fixture = crate::streams::testing::CameraFile::new(750);
        let streams = Arc::new(StreamHubs::default());
        let service = SnapshotService::new(Arc::clone(&streams));
        let subscriber = streams
            .subscribe(&fixture.camera, SubscriberPolicy::recorder(), None)
            .unwrap();
        let first = tokio::task::spawn_blocking(move || {
            assert!(matches!(
                subscriber.recv(Duration::from_secs(3)),
                Some(Delivery::Unit { .. })
            ));
            subscriber
        })
        .await
        .unwrap();
        let started = Instant::now();
        let jpeg = service
            .capture_from(fixture.camera.clone(), true)
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(jpeg.starts_with(&[255, 216]) && jpeg.len() <= MAX_JPEG_BYTES);
        assert_eq!(streams.open_count(), 1);
        assert_eq!(streams.summary()[0].1, 1, "snapshot already unsubscribed");
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(2);
        while streams.open_count() != 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(streams.open_count(), 0);
        let started = Instant::now();
        let jpeg = service
            .capture_from(fixture.camera.clone(), true)
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(jpeg.starts_with(&[255, 216]) && jpeg.len() <= MAX_JPEG_BYTES);
        let deadline = Instant::now() + Duration::from_secs(2);
        while streams.open_count() != 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            streams.open_count(),
            0,
            "an unwatched snapshot leaves no connection"
        );
        streams.shutdown();
    }

    #[test]
    fn cached_keyframe_is_a_one_shot_jpeg_and_forget_releases_it() {
        gstreamer::init().unwrap();
        let pipeline = crate::pipeline::builder::build("videotestsrc num-buffers=1 ! video/x-raw,width=320,height=180 ! jpegenc ! appsink name=test_sink sync=false").unwrap();
        let sink = pipeline
            .by_name("test_sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let sample = sink
            .try_pull_sample(gstreamer::ClockTime::from_seconds(2))
            .unwrap();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        let service = SnapshotService::default();
        service.record(
            "test",
            sample.caps().unwrap().to_owned(),
            sample.buffer().unwrap().to_owned(),
        );
        let cached = service.latest("test").unwrap();
        let jpeg = decode_keyframe(&cached).unwrap();
        assert_eq!(&jpeg[..2], &[255, 216]);
        assert!(jpeg.len() < MAX_JPEG_BYTES);
        service.forget("test");
        assert!(service.latest("test").is_none());
    }
    #[test]
    fn stream_snapshots_decode_only_keyframes() {
        gstreamer::init().expect("gstreamer");
        let camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://127.0.0.1:1/live");
        let pipeline = stream_snapshot_pipeline(&camera);
        assert!(
            pipeline.contains("parsebin ! identity drop-buffer-flags=delta-unit"),
            "{pipeline}"
        );
        assert!(pipeline.ends_with(TAIL));
        crate::pipeline::builder::build(&pipeline).expect("the snapshot pipeline parses");
    }

    #[test]
    fn thumbnail_paths_reject_traversal_and_use_only_event_ids() {
        assert!(thumbnail_path("../outside.jpg").is_err());
        assert!(thumbnail_path("/outside.jpg").is_err());
        assert!(thumbnail_path("1.jpg").is_ok());
    }
    #[test]
    fn encoded_snapshot_cache_has_a_global_memory_budget() {
        gstreamer::init().unwrap();
        let service = SnapshotService::default();
        for i in 0..5 {
            service.record(
                &format!("camera-{i}"),
                gstreamer::Caps::builder("image/jpeg").build(),
                gstreamer::Buffer::with_size(MAX_KEYFRAME_BYTES).unwrap(),
            );
        }
        let bytes: usize = service
            .cache
            .lock()
            .unwrap()
            .values()
            .map(|frame| frame.buffer.size())
            .sum();
        assert!(
            bytes <= 32 * 1024 * 1024,
            "dense walls must not retain unlimited main-stream keyframes"
        );
    }

    fn jpeg_sample() -> (gstreamer::Caps, gstreamer::Buffer) {
        gstreamer::init().unwrap();
        let pipeline = crate::pipeline::builder::build("videotestsrc num-buffers=1 ! video/x-raw,width=320,height=180 ! jpegenc ! appsink name=test_sink sync=false").unwrap();
        let sink = pipeline
            .by_name("test_sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let sample = sink
            .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
            .unwrap();
        pipeline.set_state(gstreamer::State::Null).unwrap();
        (
            sample.caps().unwrap().to_owned(),
            sample.buffer().unwrap().to_owned(),
        )
    }

    #[test]
    fn empty_and_oversized_keyframes_are_not_cached_and_replacements_keep_the_budget() {
        gstreamer::init().unwrap();
        let service = SnapshotService::default();
        let caps = gstreamer::Caps::builder("image/jpeg").build();
        service.record("empty", caps.clone(), gstreamer::Buffer::new());
        service.record(
            "huge",
            caps.clone(),
            gstreamer::Buffer::with_size(MAX_KEYFRAME_BYTES + 1).unwrap(),
        );
        assert!(service.latest("empty").is_none() && service.latest("huge").is_none());
        assert_eq!(service.used.load(Ordering::Relaxed), 0);
        service.record(
            "cam",
            caps.clone(),
            gstreamer::Buffer::with_size(100).unwrap(),
        );
        service.record(
            "cam",
            caps.clone(),
            gstreamer::Buffer::with_size(40).unwrap(),
        );
        assert_eq!(
            service.used.load(Ordering::Relaxed),
            40,
            "the old frame is released"
        );
        service.forget("unknown");
        assert_eq!(service.used.load(Ordering::Relaxed), 40);
        service.forget("cam");
        assert_eq!(service.used.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_cache_never_holds_more_than_a_thousand_cameras() {
        gstreamer::init().unwrap();
        let service = SnapshotService::default();
        let caps = gstreamer::Caps::builder("image/jpeg").build();
        for i in 0..1030 {
            service.record(
                &format!("c{i}"),
                caps.clone(),
                gstreamer::Buffer::with_size(8).unwrap(),
            );
        }
        assert_eq!(service.cache.lock().unwrap().len(), 1024);
    }

    #[tokio::test]
    async fn requests_beyond_the_two_workers_are_refused_while_they_are_busy() {
        let service = SnapshotService::default();
        let first = service.slots.clone().try_acquire_owned().unwrap();
        let second = service.slots.clone().try_acquire_owned().unwrap();
        let camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://127.0.0.1:9/live");
        let error = service
            .capture_from(camera.clone(), false)
            .await
            .unwrap_err();
        assert!(error.contains("busy"), "{error}");
        drop((first, second));
        // A cached picture is served from the decoder, never from the camera.
        let (caps, buffer) = jpeg_sample();
        service.record(&camera.id, caps, buffer);
        let jpeg = service.capture_from(camera, false).await.unwrap();
        assert!(jpeg.starts_with(&[255, 216]));
    }

    #[test]
    fn the_cloud_and_local_caches_both_receive_keyframes_and_forgetting() {
        #[derive(Default)]
        struct Counting(
            std::sync::atomic::AtomicUsize,
            std::sync::atomic::AtomicUsize,
        );
        impl KeyframeSink for Counting {
            fn record(&self, _: &str, _: gstreamer::Caps, _: gstreamer::Buffer) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn forget(&self, _: &str) {
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        gstreamer::init().unwrap();
        let cloud = Arc::new(Counting::default());
        let local = Arc::new(SnapshotService::default());
        let fanout = KeyframeFanout {
            local: Arc::clone(&local),
            cloud: Some(Arc::clone(&cloud) as Arc<dyn KeyframeSink>),
        };
        let caps = gstreamer::Caps::builder("image/jpeg").build();
        fanout.record(
            "cam",
            caps.clone(),
            gstreamer::Buffer::with_size(10).unwrap(),
        );
        assert!(local.latest("cam").is_some());
        assert_eq!(cloud.0.load(Ordering::SeqCst), 1);
        fanout.forget("cam");
        assert!(local.latest("cam").is_none());
        assert_eq!(cloud.1.load(Ordering::SeqCst), 1);
        // Without a cloud only the local cache is kept.
        let local_only = KeyframeFanout {
            local: Arc::clone(&local),
            cloud: None,
        };
        local_only.record("cam", caps, gstreamer::Buffer::with_size(10).unwrap());
        assert!(local.latest("cam").is_some());
        local_only.forget("cam");
        assert!(local.latest("cam").is_none());
    }

    #[test]
    fn a_picture_that_is_missing_wrong_or_not_a_jpeg_is_an_error() {
        gstreamer::init().unwrap();
        let build = |description: &str| crate::pipeline::builder::build(description).unwrap();
        // No output element at all, or one that is not an appsink.
        let error = take_picture(&build("videotestsrc num-buffers=1 ! fakesink")).unwrap_err();
        assert_eq!(error, "Snapshot output missing");
        let error = take_picture(&build(
            "videotestsrc num-buffers=1 ! fakesink name=snapshot_sink",
        ))
        .unwrap_err();
        assert_eq!(error, "Invalid snapshot output");
        // A stream that ends before a picture arrives.
        let error = take_picture(&build(
            "videotestsrc num-buffers=0 ! appsink name=snapshot_sink sync=false",
        ))
        .unwrap_err();
        assert!(error.contains("No snapshot picture"), "{error}");
        // A raw picture is not a JPEG.
        let error = take_picture(&build(
            "videotestsrc num-buffers=1 ! video/x-raw,width=16,height=16 ! appsink name=snapshot_sink sync=false",
        ))
        .unwrap_err();
        assert!(error.contains("too large or invalid"), "{error}");
        // A JPEG over the size limit is refused as well.
        let error = take_picture(&build(
            "videotestsrc num-buffers=1 pattern=snow ! video/x-raw,width=1280,height=720 ! jpegenc quality=100 ! appsink name=snapshot_sink sync=false",
        ))
        .unwrap_err();
        assert!(error.contains("too large or invalid"), "{error}");
    }

    #[test]
    fn event_picture_names_are_the_event_id_and_nothing_else() {
        for bad in [
            "0.jpg", "-3.jpg", "01.jpg", "+1.jpg", "1.png", "1", "a.jpg", "1/2.jpg", "",
        ] {
            assert!(thumbnail_path(bad).is_err(), "{bad}");
        }
        let path = thumbnail_path("42.jpg").unwrap();
        assert!(path.ends_with("events/42.jpg") || path.ends_with("events\\42.jpg"));
    }

    #[tokio::test]
    async fn a_new_activity_event_gets_its_picture_saved_and_attached() {
        use nw_core::event::{EventKind, EventSource};
        let _env = DATA_HOME.lock().await;
        let data = std::env::temp_dir().join(format!("nw-snapshot-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        // Event pictures live under the user's data directory: use a private one.
        std::env::set_var("XDG_DATA_HOME", &data);
        let mut config = crate::config::AppConfig::default();
        let camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://127.0.0.1:9/live");
        config.cameras.push(camera.clone());
        let state = Arc::new(tokio::sync::RwLock::new(
            crate::app_state::AppState::with_database(
                config,
                data.join("config.json"),
                crate::pipeline::hwaccel::HwAccelKind::Software,
                crate::db::connection::open_in_memory().unwrap(),
            ),
        ));
        let (hub, pictures) = {
            let st = state.read().await;
            (Arc::clone(&st.events), Arc::clone(&st.snapshots))
        };
        let (caps, buffer) = jpeg_sample();
        pictures.record(&camera.id, caps, buffer);
        let (shutdown, shutdown_rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(run(Arc::clone(&state), shutdown_rx));
        // Ignored: not an activity, no camera, and an unknown camera.
        let started = Instant::now();
        let attached = loop {
            hub.start(crate::events::NewEvent::new(
                None,
                EventKind::Motion,
                EventSource::Onvif,
            ));
            hub.start(crate::events::NewEvent::new(
                Some("unknown".into()),
                EventKind::Motion,
                EventSource::Onvif,
            ));
            hub.start(crate::events::NewEvent::new(
                Some(camera.id.clone()),
                EventKind::CameraOffline,
                EventSource::System,
            ));
            hub.start(crate::events::NewEvent::new(
                Some(camera.id.clone()),
                EventKind::Motion,
                EventSource::Onvif,
            ));
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(event) = hub
                .open_events()
                .into_iter()
                .find(|e| e.thumbnail.is_some())
            {
                break event;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "no picture was attached"
            );
        };
        let relative = attached.thumbnail.unwrap();
        assert_eq!(relative, format!("{}.jpg", attached.id));
        let saved = std::fs::read(data.join("neurowatch/events").join(&relative)).unwrap();
        assert!(saved.starts_with(&[255, 216]));
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_dir_all(&data);
    }

    fn unit(delta: bool, arrived_us: i64, picture_us: Option<i64>) -> crate::streams::SharedUnit {
        let (caps, mut buffer) = jpeg_sample();
        if delta {
            buffer
                .make_mut()
                .set_flags(gstreamer::BufferFlags::DELTA_UNIT);
        }
        crate::streams::SharedUnit {
            media: crate::streams::Media::Video,
            buffer,
            caps,
            arrived_us,
            frame: picture_us.map(|time_us| crate::streams::frames::Identity {
                seq: 1,
                time_us,
                observed_us: arrived_us,
            }),
        }
    }

    #[test]
    fn the_keyframe_for_an_event_is_the_first_at_or_after_its_start() {
        gstreamer::init().unwrap();
        let start = 10_000_000;
        let cached_older = unit(false, 8_000_000, Some(8_000_000));
        let delta_after = unit(true, 10_500_000, Some(10_500_000));
        let at_start = unit(false, 10_000_100, Some(10_000_000));
        let later = unit(false, 12_000_000, Some(12_000_000));
        let order = [&cached_older, &delta_after, &later, &at_start];
        let picked = order
            .iter()
            .find(|u| crate::streams::keyframe_since(u, start))
            .map(|u| crate::streams::unit_time_us(u));
        assert_eq!(
            picked,
            Some(12_000_000),
            "older and delta units are skipped"
        );
        assert!(crate::streams::keyframe_since(&at_start, start));
        assert!(!crate::streams::keyframe_since(&cached_older, start));
        // Without a picture identity the arrival time is the unit's time.
        let bare = unit(false, 11_000_000, None);
        assert_eq!(crate::streams::unit_time_us(&bare), 11_000_000);
        assert!(crate::streams::keyframe_since(&bare, start));
    }

    /// A plugin event with no stream open falls back to the cached picture and
    /// keeps the plugin's box; an event of another source is left as it was.
    #[tokio::test]
    async fn plugin_events_fall_back_and_keep_their_box_and_others_are_unchanged() {
        use nw_core::event::{EventKind, EventSource};
        let _env = DATA_HOME.lock().await;
        let data = std::env::temp_dir().join(format!("nw-snapshot-box-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        std::env::set_var("XDG_DATA_HOME", &data);
        let mut config = crate::config::AppConfig::default();
        let camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://127.0.0.1:9/live");
        config.cameras.push(camera.clone());
        let state = Arc::new(tokio::sync::RwLock::new(
            crate::app_state::AppState::with_database(
                config,
                data.join("config.json"),
                crate::pipeline::hwaccel::HwAccelKind::Software,
                crate::db::connection::open_in_memory().unwrap(),
            ),
        ));
        let (hub, pictures) = {
            let st = state.read().await;
            (Arc::clone(&st.events), Arc::clone(&st.snapshots))
        };
        let (caps, buffer) = jpeg_sample();
        pictures.record(&camera.id, caps, buffer);
        let (shutdown, shutdown_rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(run(Arc::clone(&state), shutdown_rx));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let rect = [0.1, 0.2, 0.3, 0.4];
        let mut ids = Vec::new();
        for source in [EventSource::Plugin, EventSource::Onvif] {
            let started = hub.start(crate::events::NewEvent::new(
                Some(camera.id.clone()),
                EventKind::Motion,
                source,
            ));
            hub.update(started.id, |event| event.bbox = Some(rect));
            ids.push(started.id);
            // The two picture workers are never both needed at once here.
            let deadline = Instant::now() + Duration::from_secs(20);
            while hub
                .open_event(started.id)
                .is_some_and(|event| event.thumbnail.is_none())
            {
                assert!(Instant::now() < deadline, "no picture was attached");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        for id in ids {
            let event = hub.open_event(id).unwrap();
            assert_eq!(event.bbox, Some(rect));
            assert_eq!(event.thumbnail, Some(format!("{id}.jpg")));
        }
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_dir_all(&data);
    }
}
