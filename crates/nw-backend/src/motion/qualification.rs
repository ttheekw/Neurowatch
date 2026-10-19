//! Opt-in loopback media checks. Never uses the developer's config or service.
use crate::{app_state::AppState, config::AppConfig, pipeline::hwaccel::HwAccelKind};
use nw_core::camera::{CameraConfig, MotionSource};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, RwLock};

struct Source(Child);
impl Source {
    fn start(codec: &str) -> (Self, String) {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/motion_rtsp.py");
        let child = Command::new("/usr/bin/python3")
            .arg(fixture)
            .arg(codec)
            .stdout(Stdio::piped())
            .spawn()
            .expect("RTSP test fixture");
        let mut source = Self(child);
        let mut port = String::new();
        BufReader::new(source.0.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        let port: u16 = port
            .trim()
            .parse()
            .expect("fixture must print its bound loopback port");
        (source, format!("rtsp://127.0.0.1:{port}/motion"))
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn until(mut condition: impl FnMut() -> bool, detail: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "Timed out: {detail}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "loopback RTSP + x264/x265 + optional mock RTSP-server runtime"]
async fn loopback_motion_lifecycle_preserves_recording_in_desktop_and_headless_modes() {
    gstreamer::init().unwrap();
    for (headless, codec) in [(false, "h264"), (true, "h265")] {
        let (_source, url) = Source::start(codec);
        let scratch =
            Scratch(std::env::temp_dir().join(format!("nw-motion-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&scratch.0).unwrap();
        let mut camera = CameraConfig::new("qualification", &url);
        camera.id = "qualification".into();
        camera.sub_url = Some(url);
        camera.motion.source = MotionSource::Builtin;
        camera.motion.sensitivity = 10;
        camera.motion.min_duration_sec = 0.0;
        camera.motion.cooldown_sec = 0.0;
        camera.recording.local.enabled = true;
        camera.recording.local.storage_path = Some(scratch.0.to_string_lossy().into_owned());
        let mut config = AppConfig::default();
        config.cameras.push(camera.clone());
        let mut app = AppState::with_database(
            config,
            scratch.0.join("config.json"),
            HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        );
        app.set_headless(headless);
        app.spawn_supervisor(&camera);
        let counters = app.stream_health.counters_for(&camera.id);
        let motion = Arc::clone(&app.builtin_motion);
        let events = Arc::clone(&app.events);
        let state = Arc::new(RwLock::new(app));
        let (shutdown, receiver) = broadcast::channel(1);
        let task = tokio::spawn(Arc::clone(&motion).run(
            Arc::clone(&state),
            Arc::clone(&events),
            receiver,
        ));
        let mut verification = {
            let state = Arc::clone(&state);
            let motion = Arc::clone(&motion);
            let events = Arc::clone(&events);
            tokio::spawn(async move {
                until(
                    || motion.status(&camera.id).is_some_and(|s| s.frames >= 5),
                    "decoded analysis pictures",
                )
                .await;
                until(
                    || counters.ingested_units.load(Ordering::Relaxed) >= 10,
                    "independent recording ingest",
                )
                .await;
                until(
                    || {
                        events
                            .open_events()
                            .iter()
                            .any(|e| e.source == nw_core::event::EventSource::Builtin)
                    },
                    "built-in motion event",
                )
                .await;
                let status = motion.status(&camera.id).unwrap();
                assert!(status.decoder_running && status.using_substream);
                assert_eq!(
                    state.read().await.active_views(),
                    0,
                    "motion must need no desktop lease"
                );
                assert!(motion.snapshot(&camera.id, true).1.is_some());
                eprintln!("{codec}, headless={headless}: {:.2} analysis fps, {:.3} detector ms/picture, {} queue drops", status.analysis_fps, status.analysis_ms, status.dropped_frames);

                // Break only the independent analysis URL; recording keeps its original
                // main URL/connection. Reconciliation must retire the old detector.
                state.write().await.config.cameras[0].sub_url =
                    Some("rtsp://127.0.0.1:1/unavailable".into());
                until(
                    || {
                        motion
                            .status(&camera.id)
                            .is_some_and(|s| s.state == "backoff")
                    },
                    "analysis failure/backoff",
                )
                .await;
                assert!(motion.snapshot(&camera.id, true).1.is_none());
                let before = counters.ingested_units.load(Ordering::Relaxed);
                until(
                    || counters.ingested_units.load(Ordering::Relaxed) >= before + 10,
                    "recording survives analysis failure",
                )
                .await;

                {
                    let mut app = state.write().await;
                    app.config.cameras[0].sub_url = camera.sub_url.clone();
                    app.config.cameras[0].motion.low_cpu = true;
                }
                until(
                    || {
                        motion.status(&camera.id).is_some_and(|s| {
                            s.state == "running" && s.low_cpu && s.skipped_delta_frames > 10
                        })
                    },
                    "keyframe-only recovery",
                )
                .await;
                assert!(motion
                    .status(&camera.id)
                    .unwrap()
                    .decoder
                    .as_deref()
                    .unwrap()
                    .starts_with("avdec_"));
                state.write().await.config.cameras[0].motion.source = MotionSource::Off;
                until(
                    || motion.status(&camera.id).is_none(),
                    "opt-out releases decoder and preview",
                )
                .await;
                let before = counters.ingested_units.load(Ordering::Relaxed);
                until(
                    || counters.ingested_units.load(Ordering::Relaxed) >= before + 10,
                    "recording survives opt-out",
                )
                .await;
                assert!(motion.metrics().is_empty());
                assert!(!events
                    .open_events()
                    .iter()
                    .any(|e| e.source == nw_core::event::EventSource::Builtin));
            })
        };
        let verified = tokio::time::timeout(Duration::from_secs(70), &mut verification).await;
        if verified.is_err() {
            verification.abort();
        }
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        state.write().await.stop_all();
        // Give the ingest a chance to finalize its temporary recording before
        // removing this test's directory; no user files are involved.
        tokio::time::sleep(Duration::from_millis(500)).await;
        events.flush();
        verified
            .expect("qualification deadline")
            .expect("qualification checks");
    }
}
