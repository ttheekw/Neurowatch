//! In-memory jobs for authorization tests: no files or export threads.

use super::*;

pub(crate) fn running_job(service: &ExportService, camera_id: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let job = Arc::new(Job {
        id: id.clone(),
        camera_id: camera_id.into(),
        start_ms: 1000,
        end_ms: 2000,
        path: std::env::temp_dir().join(format!("{id}.mp4")),
        created_ms: 1000,
        timelapse: None,
        cancel: AtomicBool::new(false),
        progress: AtomicU32::new(0),
        state: Mutex::new(JobState::Running),
    });
    let mut jobs = service.jobs.lock().unwrap();
    jobs.by_id.insert(id.clone(), job);
    jobs.order.push_back(id.clone());
    id
}

pub(crate) fn cancel_requested(service: &ExportService, id: &str) -> bool {
    service.jobs.lock().unwrap().by_id[id]
        .cancel
        .load(Ordering::Relaxed)
}
