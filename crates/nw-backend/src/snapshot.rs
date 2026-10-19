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
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Snapshot workers are busy; try again".to_owned())?;
        let frame = self.latest(&camera.id);
        let streams = Arc::clone(&self.streams);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if let Some(frame) = frame {
                if let Ok(jpeg) = decode_keyframe(&frame) {
                    return Ok(jpeg);
                }
            }
            if shared {
                return decode_keyframe(&shared_keyframe(&streams, &camera)?);
            }
            let pipeline = crate::pipeline::builder::build(&stream_snapshot_pipeline(&camera))
                .map_err(|_| "The snapshot stream could not be opened".to_owned())?;
            take_picture(&pipeline)
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
    use nw_core::event::{EventKind, EventPhase};
    let mut notices = state.read().await.events.subscribe();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            notice = notices.recv() => {
                let notice = match notice { Ok(notice) => notice, Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue, Err(_) => break };
                if notice.phase != EventPhase::Start || !matches!(notice.event.kind, EventKind::Motion | EventKind::Intrusion | EventKind::LineCrossing) || tasks.len() >= 2 { continue; }
                let Some(camera_id) = notice.event.camera_id else { continue; };
                let (camera, pictures, hub, db) = {
                    let state = state.read().await;
                    (state.config.cameras.iter().find(|c| c.id == camera_id).cloned(), Arc::clone(&state.snapshots), Arc::clone(&state.events), Arc::clone(&state.db))
                };
                let Some(camera) = camera else { continue; };
                tasks.spawn(async move {
                    let Ok(jpeg) = pictures.capture(camera).await else { return; };
                    let relative = format!("{}.jpg", notice.event.id);
                    let Ok(path) = thumbnail_path(&relative) else { return; };
                    let saved = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
                        let temporary = path.with_extension("jpg.part");
                        std::fs::write(&temporary, jpeg)?; std::fs::rename(temporary, path)
                    }).await;
                    if !matches!(saved, Ok(Ok(()))) { return; }
                    if hub.update(notice.event.id, |event| event.thumbnail = Some(relative.clone())).is_none() {
                        let _ = tokio::task::spawn_blocking(move || {
                            hub.flush();
                            if let Ok(conn) = db.lock() { let _ = conn.execute("UPDATE events SET thumbnail=?1 WHERE id=?2", rusqlite::params![relative, notice.event.id]); }
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

    #[tokio::test]
    async fn snapshot_fallback_reuses_an_open_hub_and_closes_an_unwatched_one() {
        use crate::streams::{Delivery, StreamHubs, SubscriberPolicy};
        let fixture = crate::streams::testing::CameraFile::new(150);
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
}
