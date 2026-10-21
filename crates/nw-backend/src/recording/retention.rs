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

#[cfg(test)]
mod tests;

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
        let (db, hub, threshold, roots, quotas, event_policies, offline, archive_quota) = {
            let st = state.read().await;
            // Nothing is cleaned up on a missing location: its files are not
            // gone, only out of reach (an unmounted share), and cleanup would
            // drop them from the index or measure the wrong disk.
            let ready = |root: &std::path::Path| st.storage.is_ready(root);
            let mut roots = vec![expand_user_path(&st.config.recording.storage_path)];
            let mut quotas = Vec::new();
            let mut event_policies = Vec::new();
            for cam in &st.config.cameras {
                let root = expand_user_path(
                    cam.recording
                        .local
                        .storage_path
                        .as_deref()
                        .unwrap_or(&st.config.recording.storage_path),
                );
                if !ready(&root) {
                    continue;
                }
                event_policies.push((
                    cam.id.clone(),
                    cam.recording.clone(),
                    root,
                    st.event_retention_since(&cam.id),
                ));
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
            // The archive: its own size limit, and its drive's threshold.
            let archive = &st.config.recording.archive;
            let archive_quota = (archive.enabled && !archive.path.trim().is_empty())
                .then(|| {
                    let root = expand_user_path(&archive.path);
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                    archive
                        .quota_gb
                        .map(|gb| i64::from(gb) * 1024 * 1024 * 1024)
                })
                .flatten();
            roots.retain(|root| ready(root));
            (
                Arc::clone(&st.db),
                Arc::clone(&st.events),
                st.config.recording.disk_threshold_percent,
                roots,
                quotas,
                event_policies,
                st.storage.missing_roots(),
                archive_quota,
            )
        };

        let passes = tokio::task::spawn_blocking(move || {
            let active_after = SystemTime::now() - ACTIVE_SEGMENT_GRACE;
            let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos()/1_000_000) as i64;
            hub.flush();
            for (camera, settings, root, since) in event_policies {
                let marked = db.lock().ok().and_then(|conn| {
                    crate::recording::event_retention::refresh_flags(&conn,&camera,since,settings.event_pre_sec,settings.event_post_sec,now_ms).ok()
                }).is_some();
                if !marked || since.is_none() { continue; }
                // Five-minute delivery/index grace plus pre-roll. This mode
                // saves space, not writes; retained files stay whole segments.
                let keep_ms = i64::from(settings.non_event_keep_hours)*3_600_000
                    + i64::from(settings.event_pre_sec)*1000 + 300_000;
                match crate::recording::event_retention::prune_unmarked(&db,&camera,&root,now_ms.saturating_sub(keep_ms),active_after) {
                    Ok(deleted) if deleted>0 => tracing::info!(camera_id=%camera,deleted,"Unmarked event-policy recordings trimmed"),
                    Err(error) => tracing::warn!(camera_id=%camera,%error,"Event retention skipped"),
                    _ => {}
                }
            }
            for (cam_id, quota_bytes) in &quotas {
                quota::enforce(&db, cam_id, *quota_bytes, Some(active_after), &offline);
            }
            if let Some(quota_bytes) = archive_quota {
                quota::enforce_archive(&db, quota_bytes, &offline);
            }
            roots
                .into_iter()
                .map(|root| {
                    let pass = quota::enforce_disk_threshold(
                        &db,
                        &root,
                        threshold,
                        active_after,
                        &offline,
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
