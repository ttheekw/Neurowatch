//! Loopback RTSP schedule transitions; isolated config, database and files.
use super::*;
use nw_core::camera::{RecordingMode, ScheduleRule};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
};

fn handler(state: Arc<tokio::sync::RwLock<AppState>>) -> crate::ipc::handler::RpcHandler {
    use crate::stats::test_support::{StaticCameras, StaticGpu, StaticIpc, StaticSystem};
    let stats = Arc::new(crate::stats::PerformanceAggregator::new(
        Arc::new(StaticSystem(Default::default())),
        Arc::new(StaticGpu(nw_core::stats::GpuStatus::unavailable("test"))),
        Arc::new(StaticIpc(Default::default())),
        Arc::new(StaticCameras(std::sync::Mutex::new(vec![]))),
    ));
    let viewer = Arc::new(crate::viewer::ViewerManager::new(
        HwAccelKind::Software,
        crate::viewer::capabilities::ViewerCapabilities::unavailable("test"),
    ));
    crate::ipc::handler::RpcHandler::new(state, viewer, stats)
}

struct Source(Child);
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
async fn ready(state: &Arc<tokio::sync::RwLock<AppState>>, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !state.read().await.recording_pipeline_ready(id) {
        assert!(
            Instant::now() < deadline,
            "recording pipeline never became ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "loopback RTSP + x264/x265 + optional mock RTSP-server runtime"]
async fn loopback_schedule_transitions_finalize_real_recordings_without_event_mode_restart() {
    gstreamer::init().unwrap();
    for codec in ["h264", "h265"] {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/motion_rtsp.py");
        let mut source = Source(
            Command::new("/usr/bin/python3")
                .arg(fixture)
                .arg(codec)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut port = String::new();
        BufReader::new(source.0.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        let port: u16 = port.trim().parse().unwrap();
        let scratch =
            Scratch(std::env::temp_dir().join(format!("nw-schedule-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&scratch.0).unwrap();
        let mut camera = CameraConfig::new(
            "Schedule qualification",
            format!("rtsp://127.0.0.1:{port}/motion"),
        );
        camera.id = "scheduled".into();
        camera.recording.local.enabled = true;
        camera.recording.local.storage_path = Some(scratch.0.display().to_string());
        let mut config = AppConfig::default();
        config.cameras.push(camera.clone());
        let mut app = AppState::with_database(
            config,
            scratch.0.join("config.json"),
            HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        );
        app.set_headless(true);
        app.spawn_supervisor(&camera);
        let db = Arc::clone(&app.db);
        let counters = app.stream_health.counters_for(&camera.id);
        let state = Arc::new(tokio::sync::RwLock::new(app));
        let mut verification = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                ready(&state, &camera.id).await;
                let ingest = state.read().await.ingest[&camera.id].state_rx.clone();
                // Full-day weekly overrides let this exercise the production
                // scheduler at any host-local time without moving its clock.
                let rule = |mode| ScheduleRule {
                    days: (0..7).collect(),
                    start: "00:00".into(),
                    end: "00:00".into(),
                    mode,
                };
                {
                    let mut edited = state.read().await.config.cameras[0].clone();
                    edited.recording.weekly_rules = vec![rule(RecordingMode::Events)];
                    let response = handler(Arc::clone(&state))
                        .dispatch(&serde_json::json!({
                            "jsonrpc":"2.0", "id":1, "method":"camera.update", "params":edited
                        }))
                        .await;
                    assert!(response.get("error").is_none(), "{response}");
                    let st = state.read().await;
                    assert!(ingest.same_channel(&st.ingest[&camera.id].state_rx));
                    assert!(
                        st.event_retention_since(&camera.id).is_none(),
                        "detection is off: never trim"
                    );
                }
                let before = counters.ingested_units.load(Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(2200)).await;
                assert!(counters.ingested_units.load(Ordering::Relaxed) > before + 10);
                {
                    let mut st = state.write().await;
                    st.config.cameras[0].recording.weekly_rules = vec![rule(RecordingMode::Off)];
                    st.reconcile_recording_policies();
                    assert!(!st.recording.wants_recording(&camera.id));
                    assert!(!ingest.same_channel(&st.ingest[&camera.id].state_rx));
                    assert!(!st.recording.has_manual_override(&camera.id));
                }
                // Wait for the actual recording to finalize before resuming.
                tokio::time::sleep(Duration::from_millis(500)).await;
                {
                    let mut st = state.write().await;
                    st.config.cameras[0].recording.weekly_rules =
                        vec![rule(RecordingMode::Continuous)];
                    st.reconcile_recording_policies();
                    assert!(st.recording.wants_recording(&camera.id));
                }
                ready(&state, &camera.id).await;
                tokio::time::sleep(Duration::from_millis(2200)).await;
                {
                    let mut st = state.write().await;
                    // Explicit Stop remains authoritative across schedule changes.
                    st.recording.stop(&camera.id);
                    let cam = st.config.cameras[0].clone();
                    st.restart_ingest(&cam);
                    st.reconcile_recording_policies();
                    assert!(st.recording.has_manual_override(&camera.id));
                    assert!(!st.recording.wants_recording(&camera.id));
                    // Explicit Start likewise overrides an Off window.
                    st.config.cameras[0].recording.weekly_rules = vec![rule(RecordingMode::Off)];
                    let cam = st.config.cameras[0].clone();
                    let dir = st.local_recording_output_dir(&cam);
                    st.recording.start(camera.id.clone(), dir);
                    st.restart_ingest(&cam);
                    st.reconcile_recording_policies();
                    assert!(st.recording.wants_recording(&camera.id));
                }
                ready(&state, &camera.id).await;
                assert_eq!(state.read().await.active_views(), 0);
                tokio::time::sleep(Duration::from_millis(1200)).await;
            })
        };
        let verified = tokio::time::timeout(Duration::from_secs(60), &mut verification).await;
        if verified.is_err() {
            verification.abort();
        }
        let tasks = {
            let mut st = state.write().await;
            st.stop_all();
            st.take_supervisor_tasks()
        };
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
        }
        verified
            .expect("qualification deadline")
            .expect("schedule transitions");
        let id = "scheduled".to_owned();
        let dir = scratch.0.join(&id);
        // Age only temporary test files past the production settle guard.
        for date in std::fs::read_dir(&dir).unwrap().flatten() {
            for file in std::fs::read_dir(date.path()).unwrap().flatten() {
                std::fs::File::options()
                    .write(true)
                    .open(file.path())
                    .unwrap()
                    .set_modified(std::time::SystemTime::now() - Duration::from_secs(600))
                    .unwrap();
            }
        }
        crate::recording::segment_tracker::scan_camera_tree(&db, &id, &dir, 60_000);
        let conn = db.lock().unwrap();
        let (count, exact): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(timing_exact),0) FROM segments WHERE camera_id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            count >= 3,
            "three finalized runs expected for {codec}: {count}"
        );
        assert_eq!(count, exact, "each real Matroska file has measured timing");
    }
}
