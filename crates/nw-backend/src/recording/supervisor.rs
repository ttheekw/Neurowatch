//! Recording state machine — tracks which cameras are recording and where.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nw_core::camera::CameraId;

#[derive(Debug, Clone, PartialEq)]
pub enum RecordingState {
    Idle,
    Starting,
}

#[cfg(test)]
impl RecordingState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordingSource {
    Manual,
    ConfiguredLocal,
    GlobalAllLocal,
}

#[derive(Debug, Clone)]
struct CameraRec {
    state: RecordingState,
    source: RecordingSource,
    storage_path: PathBuf,
}

/// In-memory recording state per camera.  Stored in `AppState`.
pub struct RecordingSupervisor {
    cameras: HashMap<CameraId, CameraRec>,
}

impl Default for RecordingSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingSupervisor {
    pub fn new() -> Self {
        Self {
            cameras: HashMap::new(),
        }
    }

    /// Mark a camera as actively recording.
    pub fn start(&mut self, cam_id: CameraId, storage_path: PathBuf) {
        self.cameras.insert(
            cam_id,
            CameraRec {
                state: RecordingState::Starting,
                source: RecordingSource::Manual,
                storage_path,
            },
        );
    }

    /// Mark a camera as recording via global "record all local cameras".
    /// Cameras with Manual source (manually started OR manually stopped) are not overridden,
    /// preserving explicit per-camera choices over the global setting.
    pub fn start_global_all_local(&mut self, cam_id: CameraId, storage_path: PathBuf) {
        if let Some(rec) = self.cameras.get(&cam_id) {
            if rec.source == RecordingSource::Manual
                || rec.source == RecordingSource::ConfiguredLocal
            {
                return;
            }
        }
        self.cameras.insert(
            cam_id,
            CameraRec {
                state: RecordingState::Starting,
                source: RecordingSource::GlobalAllLocal,
                storage_path,
            },
        );
    }

    /// Mark a camera as recording because its persisted local recording config is enabled.
    pub fn start_configured_local(&mut self, cam_id: CameraId, storage_path: PathBuf) {
        if let Some(rec) = self.cameras.get(&cam_id) {
            if rec.source == RecordingSource::Manual {
                return;
            }
        }
        self.cameras.insert(
            cam_id,
            CameraRec {
                state: RecordingState::Starting,
                source: RecordingSource::ConfiguredLocal,
                storage_path,
            },
        );
    }

    /// Mark a camera as idle (recording stopped).
    pub fn stop(&mut self, cam_id: &CameraId) {
        if let Some(rec) = self.cameras.get_mut(cam_id) {
            rec.state = RecordingState::Idle;
            rec.source = RecordingSource::Manual;
        }
    }

    /// Stop recording only when it was started by the global all-cameras setting.
    pub fn stop_global_all_local(&mut self, cam_id: &CameraId) {
        if let Some(rec) = self.cameras.get_mut(cam_id) {
            if rec.state == RecordingState::Starting
                && rec.source == RecordingSource::GlobalAllLocal
            {
                rec.state = RecordingState::Idle;
                rec.source = RecordingSource::Manual;
            }
        }
    }

    /// Stop recording only when it was started by persisted local camera config.
    pub fn stop_configured_local(&mut self, cam_id: &CameraId) {
        if self
            .cameras
            .get(cam_id)
            .is_some_and(|rec| rec.source == RecordingSource::ConfiguredLocal)
        {
            self.cameras.remove(cam_id);
        }
    }

    pub fn wants_recording(&self, cam_id: &CameraId) -> bool {
        self.cameras
            .get(cam_id)
            .is_some_and(|r| r.state == RecordingState::Starting)
    }

    /// Point an active recording that writes under `from` at `to` (the
    /// recording location changed). Returns whether it moved.
    pub fn relocate(&mut self, cam_id: &CameraId, from: &Path, to: PathBuf) -> bool {
        match self.cameras.get_mut(cam_id) {
            Some(rec)
                if rec.state == RecordingState::Starting && rec.storage_path.starts_with(from) =>
            {
                rec.storage_path = to;
                true
            }
            _ => false,
        }
    }

    /// Returns the storage path for an actively recording camera.
    pub fn recording_storage_path(&self, cam_id: &CameraId) -> Option<&PathBuf> {
        self.cameras
            .get(cam_id)
            .filter(|r| r.state == RecordingState::Starting)
            .map(|r| &r.storage_path)
    }

    /// Returns `{cam_id → state_str}` for all known cameras.
    #[cfg(test)]
    pub fn status_all(&self) -> HashMap<CameraId, String> {
        self.cameras
            .iter()
            .map(|(id, r)| (id.clone(), r.state.as_str().to_owned()))
            .collect()
    }

    pub fn status_all_with_pipeline_states(
        &self,
        pipeline_states: &HashMap<CameraId, String>,
    ) -> HashMap<CameraId, String> {
        self.cameras
            .iter()
            .map(|(id, r)| {
                let status = match r.state {
                    RecordingState::Starting => match pipeline_states.get(id).map(String::as_str) {
                        // Only report "recording" once the recording branch has confirmed
                        // its first segment is open (recording_ready). "streaming" alone
                        // means the live-view pipeline is up but the recording branch has
                        // not yet opened a segment file.
                        Some("recording_ready") => "recording",
                        Some("error") | Some("offline") | Some("teardown_failed") => "failed",
                        _ => "starting",
                    },
                    RecordingState::Idle => "idle",
                };
                (id.clone(), status.to_owned())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::RecordingSupervisor;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn only_recordings_in_the_old_location_move() {
        let mut supervisor = RecordingSupervisor::new();
        supervisor.start("a".to_owned(), PathBuf::from("/old/a/2026-10-06"));
        supervisor.start("b".to_owned(), PathBuf::from("/own/b/2026-10-06"));
        let old = PathBuf::from("/old");

        assert!(supervisor.relocate(&"a".to_owned(), &old, PathBuf::from("/new/a/2026-10-06")));
        assert!(!supervisor.relocate(&"b".to_owned(), &old, PathBuf::from("/new/b/2026-10-06")));
        assert!(!supervisor.relocate(&"c".to_owned(), &old, PathBuf::from("/new/c/2026-10-06")));
        assert_eq!(
            supervisor.recording_storage_path(&"a".to_owned()),
            Some(&PathBuf::from("/new/a/2026-10-06"))
        );
        assert_eq!(
            supervisor.recording_storage_path(&"b".to_owned()),
            Some(&PathBuf::from("/own/b/2026-10-06"))
        );
    }

    #[test]
    fn status_distinguishes_starting_recording_and_failed_lifecycle() {
        let cam_id = "cam-status".to_owned();
        let mut supervisor = RecordingSupervisor::new();
        supervisor.start(cam_id.clone(), PathBuf::from("/tmp/recordings"));

        assert_eq!(
            supervisor
                .status_all_with_pipeline_states(&HashMap::new())
                .get(&cam_id)
                .map(String::as_str),
            Some("starting")
        );

        let mut pipeline_states = HashMap::new();
        pipeline_states.insert(cam_id.clone(), "streaming".to_owned());
        assert_eq!(
            supervisor
                .status_all_with_pipeline_states(&pipeline_states)
                .get(&cam_id)
                .map(String::as_str),
            Some("starting")
        );

        pipeline_states.insert(cam_id.clone(), "recording_ready".to_owned());
        assert_eq!(
            supervisor
                .status_all_with_pipeline_states(&pipeline_states)
                .get(&cam_id)
                .map(String::as_str),
            Some("recording")
        );

        pipeline_states.insert(cam_id.clone(), "error".to_owned());
        assert_eq!(
            supervisor
                .status_all_with_pipeline_states(&pipeline_states)
                .get(&cam_id)
                .map(String::as_str),
            Some("failed")
        );
    }
}
