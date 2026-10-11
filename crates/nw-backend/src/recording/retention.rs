//! Periodic retention: camera quotas and the recording-drive threshold.
//!
//! Runs after each segment-index pass, so the oldest recordings are known.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;

use crate::app_state::{expand_user_path, AppState};
use crate::recording::quota;
use crate::storage_drives;

/// A segment file modified this recently may still be being written and is
/// never deleted by a periodic pass.
const ACTIVE_SEGMENT_GRACE: Duration = Duration::from_secs(60);

/// Deletes old recordings; remembers which drives could not be brought under
/// the threshold so that is logged once, not every pass.
#[derive(Default)]
pub struct Retention {
    stuck: HashSet<PathBuf>,
}

impl Retention {
    pub async fn run_once(&mut self, state: &Arc<RwLock<AppState>>) {
        let (db, threshold, roots, quotas) = {
            let st = state.read().await;
            let mut roots = vec![expand_user_path(&st.config.recording.storage_path)];
            let mut quotas = Vec::new();
            for cam in &st.config.cameras {
                if let Some(path) = cam.recording.local.storage_path.as_deref() {
                    let root = expand_user_path(path);
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                }
                if let Some(gb) = cam.recording.local.quota_gb.filter(|gb| *gb > 0) {
                    quotas.push((cam.id.clone(), i64::from(gb) * 1024 * 1024 * 1024));
                }
            }
            (
                Arc::clone(&st.db),
                st.config.recording.disk_threshold_percent,
                roots,
                quotas,
            )
        };

        let passes = tokio::task::spawn_blocking(move || {
            let active_after = SystemTime::now() - ACTIVE_SEGMENT_GRACE;
            for (cam_id, quota_bytes) in &quotas {
                quota::enforce(&db, cam_id, *quota_bytes, Some(active_after));
            }
            roots
                .into_iter()
                .map(|root| {
                    let pass = quota::enforce_disk_threshold(
                        &db,
                        &root,
                        threshold,
                        active_after,
                        |path| storage_drives::space(&path.to_string_lossy()),
                    );
                    (root, pass)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let Ok(passes) = passes else { return };

        for (root, pass) in passes {
            let Some(pass) = pass else {
                self.stuck.remove(&root);
                continue;
            };
            let used_percent = pass.used_bytes.saturating_mul(100) / pass.total_bytes.max(1);
            if pass.cleanup.deleted > 0 {
                tracing::info!(
                    storage = %root.display(),
                    used_percent,
                    threshold_percent = threshold,
                    deleted = pass.cleanup.deleted,
                    freed_bytes = pass.cleanup.freed_bytes,
                    "Oldest recordings deleted (disk threshold)"
                );
            }
            if pass.cleanup.satisfied {
                self.stuck.remove(&root);
            } else if self.stuck.insert(root.clone()) {
                tracing::warn!(
                    storage = %root.display(),
                    used_percent,
                    threshold_percent = threshold,
                    "Recording drive is over the disk threshold and no more old recordings can be deleted"
                );
            }
        }
    }
}
