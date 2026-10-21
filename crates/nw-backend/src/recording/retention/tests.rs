use super::*;
use crate::config::AppConfig;
use crate::pipeline::hwaccel::HwAccelKind;
use crate::storage_guard::Readiness;
use nw_core::camera::CameraConfig;
use rusqlite::params;

const SEGMENT_BYTES: i64 = 800 * 1024 * 1024;

struct Fixture {
    root: PathBuf,
    state: Arc<RwLock<AppState>>,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("nw-periodic-retention-{}", uuid::Uuid::new_v4()));
        let storage = root.join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        let mut camera = CameraConfig::new("Test camera", "rtsp://127.0.0.1/unused");
        camera.id = "cam".into();
        let mut config = AppConfig::default();
        config.recording.storage_path = storage.to_string_lossy().into_owned();
        config.recording.disk_threshold_percent = 0;
        config.cameras = vec![camera];
        let state = Arc::new(RwLock::new(AppState::new(
            config,
            root.join("config.json"),
            HwAccelKind::Software,
        )));
        Self { root, state }
    }

    async fn segment(&self, name: &str, start_ms: i64, tier: &str, active: bool) -> PathBuf {
        let folder = if tier == "archive" {
            "archive"
        } else {
            "storage"
        };
        let path = self.root.join(folder).join("cam").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        // Quotas use indexed sizes; the files can stay small in these tests.
        file.set_len(4).unwrap();
        if !active {
            file.set_modified(SystemTime::now() - Duration::from_secs(120))
                .unwrap();
        }
        self.state.read().await.db.lock().unwrap().execute(
            "INSERT INTO segments(camera_id,start_ts,end_ts,path,size_bytes,tier,timing_exact) \
             VALUES('cam',?1,?2,?3,?4,?5,1)",
            params![start_ms, start_ms + 1000, path.to_string_lossy(), SEGMENT_BYTES, tier],
        ).unwrap();
        path
    }

    async fn indexed(&self, path: &std::path::Path) -> bool {
        self.state
            .read()
            .await
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE path=?1",
                [path.to_string_lossy()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn periodic_camera_quota_preserves_active_kept_and_archived_recordings() {
    let fixture = Fixture::new();
    fixture.state.write().await.config.cameras[0]
        .recording
        .local
        .quota_gb = Some(1);
    let old = fixture.segment("old.mkv", 1000, "local", false).await;
    let kept = fixture.segment("kept.mkv", 100_000, "local", false).await;
    let active = fixture.segment("active.mkv", 200_000, "local", true).await;
    let archived = fixture
        .segment("archived.mkv", 300_000, "archive", false)
        .await;
    fixture
        .state
        .read()
        .await
        .db
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO events(id,ts,end_ts,last_ts,camera_id,event_type,source,kept) \
         VALUES(100,100000,101000,101000,'cam','bookmark','user',1)",
            [],
        )
        .unwrap();

    Retention::default().run_once(&fixture.state).await;

    assert!(!old.exists());
    assert!(!fixture.indexed(&old).await);
    for path in [&kept, &active, &archived] {
        assert!(
            path.exists(),
            "protected recording was removed: {}",
            path.display()
        );
        assert!(fixture.indexed(path).await);
    }
}

#[tokio::test]
async fn missing_camera_storage_keeps_files_and_missing_file_rows_until_ready() {
    let fixture = Fixture::new();
    let root = fixture.root.join("storage");
    {
        let mut state = fixture.state.write().await;
        state.config.cameras[0].recording.local.storage_path =
            Some(root.to_string_lossy().into_owned());
        state.config.cameras[0].recording.local.quota_gb = Some(1);
        state
            .storage
            .set(&root, Readiness::Missing("drive disconnected".into()));
    }
    let missing = fixture
        .segment("unreachable.mkv", 1000, "local", false)
        .await;
    let present = fixture
        .segment("present.mkv", 100_000, "local", false)
        .await;
    std::fs::remove_file(&missing).unwrap();
    let mut retention = Retention::default();

    retention.run_once(&fixture.state).await;

    assert!(
        fixture.indexed(&missing).await,
        "NotFound on an offline drive must preserve the index"
    );
    assert!(present.exists());
    assert!(fixture.indexed(&present).await);

    fixture
        .state
        .read()
        .await
        .storage
        .set(&root, Readiness::Ready);
    retention.run_once(&fixture.state).await;

    assert!(
        !fixture.indexed(&missing).await,
        "a reachable drive permits stale-row cleanup"
    );
    assert!(present.exists());
    assert!(fixture.indexed(&present).await);
}

#[tokio::test]
async fn archive_quota_resumes_after_its_drive_returns_without_deleting_local_files() {
    let fixture = Fixture::new();
    let archive = fixture.root.join("archive");
    {
        let mut state = fixture.state.write().await;
        state.config.recording.archive.enabled = true;
        state.config.recording.archive.path = archive.to_string_lossy().into_owned();
        state.config.recording.archive.quota_gb = Some(1);
        state
            .storage
            .set(&archive, Readiness::Missing("archive disconnected".into()));
    }
    let old = fixture.segment("old.mkv", 1000, "archive", false).await;
    let newer = fixture
        .segment("newer.mkv", 100_000, "archive", false)
        .await;
    let local = fixture.segment("local.mkv", 500, "local", false).await;
    let mut retention = Retention::default();

    retention.run_once(&fixture.state).await;
    assert!(old.exists() && newer.exists() && local.exists());
    assert!(fixture.indexed(&old).await);

    fixture
        .state
        .read()
        .await
        .storage
        .set(&archive, Readiness::Ready);
    retention.run_once(&fixture.state).await;

    assert!(!old.exists());
    assert!(!fixture.indexed(&old).await);
    assert!(newer.exists() && local.exists());
    assert!(fixture.indexed(&newer).await);
    assert!(fixture.indexed(&local).await);
}

#[tokio::test]
async fn disabled_disk_threshold_clears_a_previous_stuck_warning_without_deleting() {
    let fixture = Fixture::new();
    let root = fixture.root.join("storage");
    let old = fixture.segment("old.mkv", 1000, "local", false).await;
    let mut retention = Retention::default();
    retention.stuck.insert(root.clone());

    retention.run_once(&fixture.state).await;

    assert!(!retention.stuck.contains(&root));
    assert!(old.exists());
    assert!(fixture.indexed(&old).await);
}
