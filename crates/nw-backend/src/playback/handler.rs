//! IPC handler for `playback.*` methods.
//!
//! Methods:
//!   playback.get_timeline  — query DB segments for a camera/range
//!   playback.open          — open a file pipeline and return session info
//!   playback.seek          — seek an open session
//!   playback.set_speed     — change playback rate
//!   playback.set_playing   — play/pause an open session
//!   playback.close         — stop a session and free SHM

use std::sync::Arc;

use nw_core::ipc::RpcError;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::playback::session::{PlaybackOutput, PlaybackSegment, PlaybackSession};
use crate::recording::segment_tracker;

const GET_TIMELINE_SQL: &str = "\
SELECT start_ts, end_ts, path \
FROM segments \
WHERE camera_id = ?1 \
  AND (end_ts >= ?2 OR end_ts IS NULL) \
  AND start_ts <= ?3 \
  AND local_deleted = 0 \
ORDER BY start_ts ASC";

const FIND_CONTAINING_SEGMENT_SQL: &str = "\
SELECT start_ts, end_ts, path \
FROM segments \
WHERE camera_id = ?1 \
  AND start_ts <= ?2 \
  AND (end_ts >= ?2 OR end_ts IS NULL) \
  AND local_deleted = 0 \
ORDER BY start_ts DESC \
LIMIT 1";

const FIND_NEXT_SEGMENT_SQL: &str = "\
SELECT start_ts, end_ts, path \
FROM segments \
WHERE camera_id = ?1 \
  AND start_ts > ?2 \
  AND local_deleted = 0 \
ORDER BY start_ts ASC \
LIMIT 1";

const FIND_PREV_SEGMENT_SQL: &str = "\
SELECT start_ts, end_ts, path \
FROM segments \
WHERE camera_id = ?1 \
  AND start_ts < ?2 \
  AND local_deleted = 0 \
ORDER BY start_ts DESC \
LIMIT 1";

pub struct PlaybackHandler {
    /// Cameras whose whole archive [`Self::index_segments`] has already read.
    archive_indexed: std::sync::Mutex<std::collections::HashSet<String>>,
    state: Arc<RwLock<AppState>>,
}

impl PlaybackHandler {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self {
            archive_indexed: std::sync::Mutex::new(std::collections::HashSet::new()),
            state,
        }
    }

    pub async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "playback.get_timeline" => self.get_timeline(params).await,
            "playback.open" => self.open(params).await,
            "playback.seek" => self.seek(params).await,
            "playback.set_speed" => self.set_speed(params).await,
            "playback.set_playing" => self.set_playing(params).await,
            "playback.set_audio" => self.set_audio(params).await,
            "playback.status" => self.status(params).await,
            "playback.close" => self.close(params).await,
            _ => Err(RpcError::method_not_found()),
        }
    }

    // ------------------------------------------------------------------
    // playback.get_timeline
    // ------------------------------------------------------------------

    async fn get_timeline(&self, params: &Value) -> Result<Value, RpcError> {
        let camera_id = params["camera_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'camera_id'"))?;
        let start_ms = params["start_ms"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'start_ms'"))?;
        let end_ms = params["end_ms"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'end_ms'"))?;

        self.refresh_local_segments(camera_id).await;
        let db = self.db_handle().await;
        let conn = db
            .lock()
            .map_err(|_| RpcError::internal("DB lock poisoned"))?;

        let mut stmt = conn
            .prepare(GET_TIMELINE_SQL)
            .map_err(|e| RpcError::internal(e.to_string()))?;

        let rows: Vec<Value> = stmt
            .query_map(rusqlite::params![camera_id, start_ms, end_ms], |r| {
                Ok(json!({
                    "start_ms": r.get::<_, i64>(0)?,
                    "end_ms":   r.get::<_, Option<i64>>(1)?,
                    "path":     r.get::<_, String>(2)?,
                }))
            })
            .map_err(|e| RpcError::internal(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

        tracing::debug!(
            camera_id = %camera_id,
            start_ms,
            end_ms,
            count = rows.len(),
            "Playback timeline loaded"
        );

        Ok(Value::Array(rows))
    }

    // ------------------------------------------------------------------
    // playback.open
    // ------------------------------------------------------------------

    async fn open(&self, params: &Value) -> Result<Value, RpcError> {
        let camera_id = params["camera_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'camera_id'"))?
            .to_string();
        let ts_ms = params["ts_ms"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'ts_ms'"))?;
        // Opened paused, the session shows its first frame and waits for Play;
        // left out, it plays at once (the original behaviour).
        let playing = params["playing"].as_bool().unwrap_or(true);

        self.refresh_local_segments(&camera_id).await;

        let segment_duration_ms = {
            let st = self.state.read().await;
            (st.config.recording.segment_minutes as i64) * 60_000
        };

        // Find the best segment for this timestamp.
        let ((seg_start, seg_end, _path, open_ts), segments) = {
            let db = self.db_handle().await;
            let conn = db
                .lock()
                .map_err(|_| RpcError::internal("DB lock poisoned"))?;
            let resolved = resolve_segment_for_ts(&conn, &camera_id, ts_ms, segment_duration_ms)
                .ok_or_else(|| {
                    RpcError::internal(format!(
                        "No segment found for camera '{}' near ts {}",
                        camera_id, ts_ms
                    ))
                })?;
            let segments = playlist_from_segment(&conn, &camera_id, &resolved, segment_duration_ms);
            (resolved, segments)
        };

        let initial_seek_ms = (open_ts - seg_start).max(0);
        let session_id = Uuid::new_v4().to_string();
        let shm_key = format!("pb_{session_id}");

        let session = PlaybackSession::spawn_with(
            session_id.clone(),
            camera_id,
            segments,
            initial_seek_ms,
            PlaybackOutput::Shm {
                key: shm_key.clone(),
            },
            1.0,
            playing,
        );

        let mut st = self.state.write().await;
        st.playback_sessions.insert(session_id.clone(), session);

        tracing::info!(
            session_id = %session_id,
            shm_key = %shm_key,
            seg_start,
            seg_end,
            initial_seek_ms,
            "Playback session opened"
        );

        Ok(json!({
            "session_id": session_id,
            "shm_key": shm_key,
            "start_ts": seg_start,
            "end_ts": seg_end,
        }))
    }

    // ------------------------------------------------------------------
    // playback.seek
    // ------------------------------------------------------------------

    async fn seek(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?;
        let ts_ms = params["ts_ms"]
            .as_i64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'ts_ms'"))?;

        let (camera_id, current_seg_start, current_seg_end) = {
            let st = self.state.read().await;
            let session = st
                .playback_sessions
                .get(session_id)
                .ok_or_else(|| RpcError::invalid_params("Session not found"))?;
            let (current_seg_start, current_seg_end) = session.current_segment_range();
            (session.cam_id.clone(), current_seg_start, current_seg_end)
        };

        // Fast path: seek inside the currently opened segment.
        if ts_ms >= current_seg_start && ts_ms < current_seg_end {
            let st = self.state.read().await;
            let session = st
                .playback_sessions
                .get(session_id)
                .ok_or_else(|| RpcError::invalid_params("Session not found"))?;
            let (segment_start_ts, segment_end_ts) = session.current_segment_range();
            let max_offset_ms = (segment_end_ts - segment_start_ts).max(0);
            let offset_ms = (ts_ms - segment_start_ts).clamp(0, max_offset_ms);
            session.seek(offset_ms);
            return Ok(json!({
                "ok": true,
                "start_ts": segment_start_ts,
                "end_ts": segment_end_ts,
            }));
        }

        self.refresh_local_segments(&camera_id).await;
        let segment_duration_ms = {
            let st = self.state.read().await;
            (st.config.recording.segment_minutes as i64) * 60_000
        };

        let maybe_target = {
            let db = self.db_handle().await;
            let conn = db
                .lock()
                .map_err(|_| RpcError::internal("DB lock poisoned"))?;
            resolve_segment_for_ts(&conn, &camera_id, ts_ms, segment_duration_ms).map(|resolved| {
                let segments =
                    playlist_from_segment(&conn, &camera_id, &resolved, segment_duration_ms);
                (resolved, segments)
            })
        };

        let Some(((seg_start, seg_end, _path, open_ts), segments)) = maybe_target else {
            tracing::debug!(
                session_id = %session_id,
                camera_id = %camera_id,
                ts_ms,
                "Seek ignored: no segment available near requested timestamp"
            );
            return Ok(json!({ "ok": true }));
        };

        let initial_seek_ms = (open_ts - seg_start).max(0);
        let mut st = self.state.write().await;
        // Play/pause and speed are read now, under the write lock, not from
        // before the segment lookup: a pause that arrived meanwhile must hold.
        // So is its audio: the desktop turned it on for this session once, and
        // a seek outside the playlist must not mute it behind the speaker's back.
        let (was_playing, current_speed, audio_on) = match st.playback_sessions.remove(session_id) {
            Some(prev_session) => {
                prev_session.stop();
                (
                    prev_session.is_playing(),
                    prev_session.current_speed(),
                    prev_session.audio_enabled(),
                )
            }
            None => return Err(RpcError::invalid_params("Session not found")),
        };

        // Reopened in the state it was in, so a paused view never plays a moment.
        let new_session = PlaybackSession::spawn_with(
            session_id.to_string(),
            camera_id.clone(),
            segments,
            initial_seek_ms,
            PlaybackOutput::Shm {
                key: format!("pb_{session_id}"),
            },
            current_speed,
            was_playing,
        );
        new_session.set_audio(audio_on);
        st.playback_sessions
            .insert(session_id.to_string(), new_session);

        tracing::debug!(
            session_id = %session_id,
            camera_id = %camera_id,
            requested_ts = ts_ms,
            resolved_ts = open_ts,
            seg_start,
            seg_end,
            "Seek switched playback session to matching segment"
        );

        Ok(json!({
            "ok": true,
            "start_ts": seg_start,
            "end_ts": seg_end,
        }))
    }

    // ------------------------------------------------------------------
    // playback.set_speed
    // ------------------------------------------------------------------

    async fn set_speed(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?;
        let rate = params["rate"]
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params("Missing 'rate'"))?;

        let st = self.state.read().await;
        let session = st
            .playback_sessions
            .get(session_id)
            .ok_or_else(|| RpcError::invalid_params("Session not found"))?;

        session.set_speed(rate);
        Ok(json!({ "ok": true }))
    }

    // ------------------------------------------------------------------
    // playback.set_playing
    // ------------------------------------------------------------------

    async fn set_playing(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?;
        let playing = params["playing"]
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("Missing 'playing'"))?;

        let st = self.state.read().await;
        let session = st
            .playback_sessions
            .get(session_id)
            .ok_or_else(|| RpcError::invalid_params("Session not found"))?;

        session.set_playing(playing);
        Ok(json!({ "ok": true }))
    }

    // ------------------------------------------------------------------
    // playback.set_audio / playback.status
    // ------------------------------------------------------------------

    /// Hear (or mute) one session's recorded audio. The desktop keeps one
    /// session audible at a time.
    async fn set_audio(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?;
        let enabled = params["enabled"]
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("Missing 'enabled'"))?;
        let st = self.state.read().await;
        let session = st
            .playback_sessions
            .get(session_id)
            .ok_or_else(|| RpcError::invalid_params("Session not found"))?;
        session.set_audio(enabled);
        Ok(json!({ "ok": true, "has_audio": session.has_audio() }))
    }

    /// A session's state: `has_audio` (the recording being played has the
    /// camera's audio), `audio` (on), `playing`, `speed`.
    async fn status(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?;
        let st = self.state.read().await;
        let session = st
            .playback_sessions
            .get(session_id)
            .ok_or_else(|| RpcError::invalid_params("Session not found"))?;
        Ok(json!({
            "has_audio": session.has_audio(),
            "audio": session.audio_enabled(),
            "audible": session.audible(),
            "playing": session.is_playing(),
            "speed": session.current_speed(),
        }))
    }

    // ------------------------------------------------------------------
    // playback.close
    // ------------------------------------------------------------------

    async fn close(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = params["session_id"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("Missing 'session_id'"))?;

        let mut st = self.state.write().await;
        if let Some(session) = st.playback_sessions.remove(session_id) {
            session.stop();
            let (seg_start, seg_end) = session.current_segment_range();
            tracing::info!(
                session_id = %session.session_id,
                cam_id = %session.cam_id,
                shm_key = %session.shm_key,
                seg_start,
                seg_end,
                "Playback session closed"
            );
        }

        Ok(json!({ "ok": true }))
    }

    /// Index new segment files of every configured camera. Each camera's
    /// archive is read once; after that only the last two days' folders, so a
    /// long archive is not re-read on every pass.
    ///
    /// Runs on its own schedule, not inside the coverage report: the report
    /// has a few-second budget, and a first pass over a large archive does not
    /// fit in it (the report was then dropped on every tick).
    pub async fn index_segments(&self) {
        let camera_ids: Vec<String> = {
            let st = self.state.read().await;
            st.config.cameras.iter().map(|c| c.id.clone()).collect()
        };
        let recent_day = (time::OffsetDateTime::now_utc() - time::Duration::days(2))
            .date()
            .to_string();
        for camera_id in &camera_ids {
            let archived = self
                .archive_indexed
                .lock()
                .map(|indexed| indexed.contains(camera_id))
                .unwrap_or(false);
            self.scan_segments(camera_id, archived.then_some(recent_day.as_str()))
                .await;
            if let Ok(mut indexed) = self.archive_indexed.lock() {
                indexed.insert(camera_id.clone());
            }
        }
    }

    /// Recorded time ranges per configured camera, for a playback timeline
    /// elsewhere (an extension). Times only: never paths or files. Reads the segment index
    /// only; [`Self::index_segments`] keeps it current.
    pub async fn coverage_report(
        &self,
        since_ms: i64,
        max_ranges: usize,
    ) -> Vec<(String, Vec<(i64, i64)>)> {
        let (db, camera_ids) = {
            let st = self.state.read().await;
            let ids: Vec<String> = st.config.cameras.iter().map(|c| c.id.clone()).collect();
            (Arc::clone(&st.db), ids)
        };
        let now = time::OffsetDateTime::now_utc();
        let closed_before = (now.unix_timestamp_nanos() / 1_000_000) as i64 - SEGMENT_SETTLE_MS;
        tokio::task::spawn_blocking(move || {
            let Ok(conn) = db.lock() else {
                return Vec::new();
            };
            camera_ids
                .into_iter()
                .map(|camera_id| {
                    let ranges =
                        recording_coverage(&conn, &camera_id, since_ms, closed_before, max_ranges);
                    (camera_id, ranges)
                })
                .filter(|(_, ranges)| !ranges.is_empty())
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// The recordings to play for `camera_id` from `ts_ms`: the segment
    /// holding it and the later ones, and the offset into the first.
    pub async fn playlist_from(
        &self,
        camera_id: &str,
        ts_ms: i64,
    ) -> Result<(Vec<crate::playback::session::PlaybackSegment>, i64), String> {
        self.refresh_local_segments(camera_id).await;
        let segment_duration_ms = {
            let st = self.state.read().await;
            (st.config.recording.segment_minutes as i64) * 60_000
        };
        let ((seg_start, _seg_end, _path, open_ts), segments) = {
            let db = self.db_handle().await;
            let conn = db.lock().map_err(|_| "DB lock poisoned".to_owned())?;
            let resolved = resolve_segment_for_ts(&conn, camera_id, ts_ms, segment_duration_ms)
                .ok_or_else(|| "no recording at that time".to_owned())?;
            let segments = playlist_from_segment(&conn, camera_id, &resolved, segment_duration_ms);
            (resolved, segments)
        };
        Ok((segments, (open_ts - seg_start).max(0)))
    }

    /// The recordings of `camera_id` overlapping `start_ms`..`end_ms`, oldest
    /// first (clip export).
    pub async fn segments_between(
        &self,
        camera_id: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<PlaybackSegment>, String> {
        self.refresh_local_segments(camera_id).await;
        let segment_duration_ms = {
            let st = self.state.read().await;
            (st.config.recording.segment_minutes as i64) * 60_000
        };
        let db = self.db_handle().await;
        let conn = db.lock().map_err(|_| "DB lock poisoned".to_owned())?;
        Ok(segments_in_range(
            &conn,
            camera_id,
            start_ms,
            end_ms,
            segment_duration_ms,
        ))
    }

    async fn db_handle(&self) -> Arc<std::sync::Mutex<rusqlite::Connection>> {
        let st = self.state.read().await;
        Arc::clone(&st.db)
    }

    async fn refresh_local_segments(&self, camera_id: &str) {
        self.scan_segments(camera_id, None).await;
    }

    /// Index a camera's segment files; only date folders from `since_day` on
    /// when given.
    async fn scan_segments(&self, camera_id: &str, since_day: Option<&str>) {
        let (db, camera_roots, segment_duration_ms) = {
            let st = self.state.read().await;
            let cam = st.config.cameras.iter().find(|c| c.id == camera_id);
            let mut camera_roots = Vec::new();
            if let Some(cam) = cam {
                // Manual/per-camera recording may use a custom path, while
                // "record all" always uses the global path. Scan both so a
                // dormant custom setting cannot hide global recordings.
                camera_roots
                    .push(expand_user_path(&st.config.recording.storage_path).join(camera_id));
                if let Some(raw_path) = cam.recording.local.storage_path.as_deref() {
                    let custom_root = expand_user_path(raw_path).join(camera_id);
                    if !camera_roots.contains(&custom_root) {
                        camera_roots.push(custom_root);
                    }
                }
            }
            let segment_duration_ms = (st.config.recording.segment_minutes as i64) * 60_000;
            (Arc::clone(&st.db), camera_roots, segment_duration_ms)
        };

        if camera_roots.is_empty() {
            return;
        }

        let cam_id = camera_id.to_owned();
        let since_day = since_day.map(str::to_owned);
        let _ = tokio::task::spawn_blocking(move || {
            for root in camera_roots {
                if root.exists() {
                    tracing::debug!(camera_id = %cam_id, root = %root.display(), "Refreshing local segments");
                    match &since_day {
                        Some(day) => segment_tracker::scan_camera_tree_since(
                            &db,
                            &cam_id,
                            &root,
                            segment_duration_ms,
                            day,
                        ),
                        None => segment_tracker::scan_camera_tree(
                            &db,
                            &cam_id,
                            &root,
                            segment_duration_ms,
                        ),
                    }
                }
            }
        })
        .await;
    }
}

/// Segments closer together than this are one continuous recording.
const COVERAGE_GAP_MS: i64 = 2_000;
/// A segment file untouched for this long is closed (no longer being written).
const SEGMENT_SETTLE_MS: i64 = 10_000;

/// Time ranges (ms since the epoch) for which `camera_id` has closed,
/// undeleted segments ending after `since_ms`: contiguous segments merged,
/// newest `max_ranges` kept, oldest first.
///
/// A segment's end is its file's modification time, so the one still being
/// written keeps ending "now"; segments ending at or after `closed_before_ms`
/// are left out until they close, because an unfinished file is not reliably
/// seekable.
pub(crate) fn recording_coverage(
    conn: &rusqlite::Connection,
    camera_id: &str,
    since_ms: i64,
    closed_before_ms: i64,
    max_ranges: usize,
) -> Vec<(i64, i64)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT start_ts, end_ts FROM segments \
         WHERE camera_id = ?1 AND end_ts IS NOT NULL AND end_ts > ?2 AND start_ts >= ?2 \
           AND end_ts < ?3 AND local_deleted = 0 \
         ORDER BY start_ts ASC, id ASC",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(
        rusqlite::params![camera_id, since_ms, closed_before_ms],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    ) else {
        return Vec::new();
    };
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for (start, end) in rows.flatten() {
        if end <= start {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if start - last.1 <= COVERAGE_GAP_MS => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }
    let excess = ranges.len().saturating_sub(max_ranges);
    ranges.drain(..excess);
    ranges
}

fn expand_user_path(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| std::path::PathBuf::from(path))
    } else {
        std::path::PathBuf::from(path)
    }
}

/// Undeleted segments of `camera_id` overlapping `start_ms`..`end_ms`, oldest
/// first, one per file; an open segment ends `segment_duration_ms` after its
/// start.
pub(crate) fn segments_in_range(
    conn: &rusqlite::Connection,
    camera_id: &str,
    start_ms: i64,
    end_ms: i64,
    segment_duration_ms: i64,
) -> Vec<PlaybackSegment> {
    let Ok(mut stmt) = conn.prepare(GET_TIMELINE_SQL) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(rusqlite::params![camera_id, start_ms, end_ms], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    rows.flatten()
        .filter_map(|(start_ts, end_ts, path)| {
            let end_ts = end_ts
                .unwrap_or_else(|| start_ts.saturating_add(segment_duration_ms.max(1_000)))
                .max(start_ts.saturating_add(1));
            (end_ts > start_ms && start_ts < end_ms && seen.insert(path.clone())).then_some(
                PlaybackSegment {
                    path,
                    start_ts,
                    end_ts,
                },
            )
        })
        .collect()
}

fn playlist_from_segment(
    conn: &rusqlite::Connection,
    camera_id: &str,
    current: &(i64, i64, String, i64),
    segment_duration_ms: i64,
) -> Vec<PlaybackSegment> {
    let (current_start, current_end, current_path, _) = current;
    let mut playlist = vec![PlaybackSegment {
        path: current_path.clone(),
        start_ts: *current_start,
        end_ts: *current_end,
    }];
    let mut seen = std::collections::HashSet::from([current_path.clone()]);
    let Ok(mut stmt) = conn.prepare(
        "SELECT start_ts, end_ts, path FROM segments \
         WHERE camera_id = ?1 AND start_ts >= ?2 AND local_deleted = 0 \
         ORDER BY start_ts ASC, id ASC",
    ) else {
        return playlist;
    };
    let Ok(rows) = stmt.query_map(rusqlite::params![camera_id, current_start], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) else {
        return playlist;
    };

    for row in rows.flatten() {
        let (start_ts, end_ts, path) = row;
        if !seen.insert(path.clone()) {
            continue;
        }
        let end_ts = end_ts
            .unwrap_or_else(|| start_ts.saturating_add(segment_duration_ms.max(1_000)))
            .max(start_ts.saturating_add(1));
        playlist.push(PlaybackSegment {
            path,
            start_ts,
            end_ts,
        });
    }
    playlist
}

fn resolve_segment_for_ts(
    conn: &rusqlite::Connection,
    camera_id: &str,
    ts_ms: i64,
    segment_duration_ms: i64,
) -> Option<(i64, i64, String, i64)> {
    let fallback_end = |start: i64, end_opt: Option<i64>| {
        end_opt.unwrap_or_else(|| start.saturating_add(segment_duration_ms.max(1_000)))
    };

    // 1) Prefer segment that contains the requested timestamp.
    if let Ok(Some((start, end_opt, path))) = conn
        .query_row(
            FIND_CONTAINING_SEGMENT_SQL,
            rusqlite::params![camera_id, ts_ms],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
    {
        let end = fallback_end(start, end_opt).max(start.saturating_add(1));
        let open_ts = ts_ms.clamp(start, end);
        return Some((start, end, path, open_ts));
    }

    // 2) If requested point is in a gap, jump to the next segment.
    if let Ok(Some((start, end_opt, path))) = conn
        .query_row(
            FIND_NEXT_SEGMENT_SQL,
            rusqlite::params![camera_id, ts_ms],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
    {
        let end = fallback_end(start, end_opt).max(start.saturating_add(1));
        return Some((start, end, path, start));
    }

    // 3) Otherwise snap to the nearest previous segment.
    if let Ok(Some((start, end_opt, path))) = conn
        .query_row(
            FIND_PREV_SEGMENT_SQL,
            rusqlite::params![camera_id, ts_ms],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
    {
        let end = fallback_end(start, end_opt).max(start.saturating_add(1));
        let open_ts = ts_ms.clamp(start, end);
        return Some((start, end, path, open_ts));
    }

    None
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use nw_core::camera::CameraConfig;
    use nw_core::ipc::RpcError;
    use rusqlite::{params, Connection};
    use serde_json::{json, Value};
    use tokio::sync::RwLock;

    use super::{
        expand_user_path, playlist_from_segment, recording_coverage, resolve_segment_for_ts,
        segments_in_range, PlaybackHandler, GET_TIMELINE_SQL,
    };
    use crate::app_state::AppState;
    use crate::config::AppConfig;
    use crate::pipeline::hwaccel::HwAccelKind;
    use crate::playback::session::{PlaybackOutput, PlaybackSession};

    const INVALID_PARAMS: i32 = -32602;
    const METHOD_NOT_FOUND: i32 = -32601;
    const INTERNAL_ERROR: i32 = -32603;
    const MATROSKA_HEADER: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "neurowatch-playback-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn migrated_memory_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        crate::db::schema::apply_migrations(&conn).expect("apply migrations");
        Arc::new(Mutex::new(conn))
    }

    /// Config whose global recording path points at an isolated temp dir, so
    /// no test ever scans the user's real recordings folder.
    fn isolated_config(name: &str) -> (AppConfig, PathBuf) {
        let root = unique_temp_dir(name);
        let mut config = AppConfig::default();
        config.recording.storage_path = root.join("global").to_string_lossy().into_owned();
        (config, root)
    }

    struct Fixture {
        handler: PlaybackHandler,
        state: Arc<RwLock<AppState>>,
        db: Arc<Mutex<Connection>>,
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(config: AppConfig, root: &Path) -> Fixture {
        let mut app = AppState::new(config, root.join("config.json"), HwAccelKind::Software);
        let db = migrated_memory_db();
        app.db = Arc::clone(&db);
        let state = Arc::new(RwLock::new(app));
        Fixture {
            handler: PlaybackHandler::new(Arc::clone(&state)),
            state,
            db,
            root: root.to_path_buf(),
        }
    }

    fn camera(id: &str) -> CameraConfig {
        let mut cam = CameraConfig::new("Camera", "rtsp://example.invalid/stream");
        cam.id = id.to_owned();
        cam
    }

    fn insert_segment(
        db: &Arc<Mutex<Connection>>,
        camera_id: &str,
        start_ts: i64,
        end_ts: Option<i64>,
        path: &str,
        local_deleted: bool,
    ) {
        db.lock()
            .expect("db lock")
            .execute(
                "INSERT INTO segments (camera_id, start_ts, end_ts, path, local_deleted) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![camera_id, start_ts, end_ts, path, i64::from(local_deleted)],
            )
            .expect("insert segment");
    }

    fn segment_rows(db: &Arc<Mutex<Connection>>, camera_id: &str) -> Vec<(i64, String)> {
        let conn = db.lock().expect("db lock");
        let mut stmt = conn
            .prepare("SELECT start_ts, path FROM segments WHERE camera_id = ?1 ORDER BY start_ts")
            .expect("prepare");
        stmt.query_map([camera_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows")
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, bytes).expect("write file");
    }

    fn assert_rpc_error(result: Result<Value, RpcError>, code: i32, message: &str) {
        let err = result.expect_err("expected RPC error");
        assert_eq!(err.code, code, "unexpected code for error {err:?}");
        assert_eq!(err.message, message);
        assert!(err.data.is_none());
    }

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        crate::db::schema::apply_migrations(&conn).expect("apply migrations");
        conn
    }

    fn seed(conn: &Connection, rows: &[(&str, i64, Option<i64>, &str, bool)]) {
        for (camera_id, start, end, path, deleted) in rows {
            conn.execute(
                "INSERT INTO segments (camera_id, start_ts, end_ts, path, local_deleted) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![camera_id, start, end, path, i64::from(*deleted)],
            )
            .expect("insert segment");
        }
    }

    // ------------------------------------------------------------------
    // Dispatch validation and session control semantics
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn dispatch_rejects_unknown_methods_with_method_not_found() {
        let (config, root) = isolated_config("unknown-method");
        let fx = fixture(config, &root);

        for method in [
            "playback.unknown",
            "playback.get_timeline_v2",
            "playback",
            "recording.status",
            "",
        ] {
            assert_rpc_error(
                fx.handler.dispatch(method, &json!({})).await,
                METHOD_NOT_FOUND,
                "Method not found",
            );
        }
    }

    #[tokio::test]
    async fn a_seek_to_another_recording_keeps_the_sessions_audio_on() {
        let (mut config, root) = isolated_config("seek-keeps-audio");
        config.cameras.push(camera("cam-a"));
        let fx = fixture(config, &root);
        // Playing nothing yet (an empty playlist), its audio turned on.
        let session = PlaybackSession::spawn_with(
            "s-audio".into(),
            "cam-a".into(),
            Vec::new(),
            0,
            PlaybackOutput::Shm {
                key: "pb_seek_keeps_audio".into(),
            },
            1.0,
            true,
        );
        session.set_audio(true);
        fx.state
            .write()
            .await
            .playback_sessions
            .insert("s-audio".into(), session);
        insert_segment(
            &fx.db,
            "cam-a",
            5_000,
            Some(65_000),
            "/missing/a.mkv",
            false,
        );

        let result = fx
            .handler
            .dispatch(
                "playback.seek",
                &json!({"session_id": "s-audio", "ts_ms": 10_000}),
            )
            .await
            .expect("seek");
        assert_eq!(result["start_ts"], 5_000, "reopened on the other recording");
        let st = fx.state.read().await;
        let reopened = st.playback_sessions.get("s-audio").expect("session");
        assert!(reopened.audio_enabled(), "its audio stays on");
        assert!(reopened.is_playing());
    }

    #[tokio::test]
    async fn audio_and_status_need_an_open_session() {
        let root = unique_temp_dir("audio-status");
        let (config, _path) = isolated_config("audio-status");
        let fx = fixture(config, &root);
        for (method, params) in [
            (
                "playback.set_audio",
                json!({"session_id": "nope", "enabled": true}),
            ),
            ("playback.status", json!({"session_id": "nope"})),
        ] {
            assert_rpc_error(
                fx.handler.dispatch(method, &params).await,
                -32602,
                "Session not found",
            );
        }
        assert_rpc_error(
            fx.handler
                .dispatch("playback.set_audio", &json!({"session_id": "nope"}))
                .await,
            -32602,
            "Missing 'enabled'",
        );
    }

    #[tokio::test]
    async fn dispatch_reports_missing_or_mistyped_parameters() {
        let (config, root) = isolated_config("missing-params");
        let fx = fixture(config, &root);

        let cases: Vec<(&str, Value, &str)> = vec![
            ("playback.get_timeline", Value::Null, "Missing 'camera_id'"),
            ("playback.get_timeline", json!({}), "Missing 'camera_id'"),
            (
                "playback.get_timeline",
                json!({"camera_id": 7, "start_ms": 0, "end_ms": 1}),
                "Missing 'camera_id'",
            ),
            (
                "playback.get_timeline",
                json!({"camera_id": "cam"}),
                "Missing 'start_ms'",
            ),
            (
                "playback.get_timeline",
                json!({"camera_id": "cam", "start_ms": "0", "end_ms": 1}),
                "Missing 'start_ms'",
            ),
            (
                "playback.get_timeline",
                json!({"camera_id": "cam", "start_ms": 0}),
                "Missing 'end_ms'",
            ),
            (
                "playback.get_timeline",
                json!({"camera_id": "cam", "start_ms": 0, "end_ms": 1.5}),
                "Missing 'end_ms'",
            ),
            ("playback.open", json!({}), "Missing 'camera_id'"),
            (
                "playback.open",
                json!({"camera_id": "cam"}),
                "Missing 'ts_ms'",
            ),
            (
                "playback.open",
                json!({"camera_id": "cam", "ts_ms": 12.5}),
                "Missing 'ts_ms'",
            ),
            ("playback.seek", json!({}), "Missing 'session_id'"),
            (
                "playback.seek",
                json!({"session_id": 3}),
                "Missing 'session_id'",
            ),
            (
                "playback.seek",
                json!({"session_id": "s"}),
                "Missing 'ts_ms'",
            ),
            (
                "playback.seek",
                json!({"session_id": "s", "ts_ms": "10"}),
                "Missing 'ts_ms'",
            ),
            ("playback.set_speed", json!({}), "Missing 'session_id'"),
            (
                "playback.set_speed",
                json!({"session_id": "s"}),
                "Missing 'rate'",
            ),
            (
                "playback.set_speed",
                json!({"session_id": "s", "rate": "fast"}),
                "Missing 'rate'",
            ),
            ("playback.set_playing", json!({}), "Missing 'session_id'"),
            (
                "playback.set_playing",
                json!({"session_id": "s"}),
                "Missing 'playing'",
            ),
            (
                "playback.set_playing",
                json!({"session_id": "s", "playing": 1}),
                "Missing 'playing'",
            ),
            ("playback.close", json!({}), "Missing 'session_id'"),
            (
                "playback.close",
                json!({"session_id": null}),
                "Missing 'session_id'",
            ),
        ];

        for (method, params, message) in cases {
            let result = fx.handler.dispatch(method, &params).await;
            let err = result.expect_err(&format!("{method} {params} should fail"));
            assert_eq!(err.code, INVALID_PARAMS, "{method} {params}");
            assert_eq!(err.message, message, "{method} {params}");
        }
        assert!(fx.state.read().await.playback_sessions.is_empty());
    }

    #[tokio::test]
    async fn session_controls_reject_unknown_sessions() {
        let (config, root) = isolated_config("unknown-session");
        let fx = fixture(config, &root);
        // A matching segment must not let seek bypass the session lookup.
        insert_segment(&fx.db, "cam-1", 1_000, Some(2_000), "/tmp/a.mkv", false);

        let calls = [
            (
                "playback.seek",
                json!({"session_id": "missing", "ts_ms": 1_500}),
            ),
            (
                "playback.set_speed",
                json!({"session_id": "missing", "rate": 2.0}),
            ),
            (
                "playback.set_speed",
                json!({"session_id": "missing", "rate": 1}),
            ),
            (
                "playback.set_playing",
                json!({"session_id": "missing", "playing": false}),
            ),
        ];
        for (method, params) in calls {
            assert_rpc_error(
                fx.handler.dispatch(method, &params).await,
                INVALID_PARAMS,
                "Session not found",
            );
        }
        assert!(fx.state.read().await.playback_sessions.is_empty());
    }

    #[tokio::test]
    async fn close_is_idempotent_for_unknown_sessions() {
        let (config, root) = isolated_config("close");
        let fx = fixture(config, &root);

        for _ in 0..2 {
            let result = fx
                .handler
                .dispatch("playback.close", &json!({"session_id": "never-opened"}))
                .await
                .expect("close unknown session");
            assert_eq!(result, json!({"ok": true}));
        }
        assert!(fx.state.read().await.playback_sessions.is_empty());
    }

    // ------------------------------------------------------------------
    // playback.get_timeline over seeded SQLite and local segment files
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn get_timeline_returns_overlapping_live_segments_in_start_order() {
        let (config, root) = isolated_config("timeline-seeded");
        let fx = fixture(config, &root);
        // Inserted out of order to prove ORDER BY start_ts.
        insert_segment(&fx.db, "cam-1", 3_000, Some(4_000), "/rec/b.mkv", false);
        insert_segment(&fx.db, "cam-1", 1_000, Some(2_000), "/rec/a.mkv", false);
        insert_segment(&fx.db, "cam-1", 5_000, None, "/rec/open.mkv", false);
        insert_segment(
            &fx.db,
            "cam-1",
            2_500,
            Some(2_600),
            "/rec/deleted.mkv",
            true,
        );
        insert_segment(&fx.db, "cam-2", 1_000, Some(2_000), "/rec/other.mkv", false);
        insert_segment(&fx.db, "cam-1", 9_000, Some(9_500), "/rec/late.mkv", false);

        let rows = fx
            .handler
            .dispatch(
                "playback.get_timeline",
                &json!({"camera_id": "cam-1", "start_ms": 1_500, "end_ms": 5_500}),
            )
            .await
            .expect("timeline");
        assert_eq!(
            rows,
            json!([
                {"start_ms": 1_000, "end_ms": 2_000, "path": "/rec/a.mkv"},
                {"start_ms": 3_000, "end_ms": 4_000, "path": "/rec/b.mkv"},
                {"start_ms": 5_000, "end_ms": null, "path": "/rec/open.mkv"},
            ])
        );

        // Range bounds are inclusive on both segment edges.
        let edges = fx
            .handler
            .dispatch(
                "playback.get_timeline",
                &json!({"camera_id": "cam-1", "start_ms": 2_000, "end_ms": 3_000}),
            )
            .await
            .expect("edge timeline");
        let paths: Vec<&str> = edges
            .as_array()
            .expect("array")
            .iter()
            .map(|row| row["path"].as_str().expect("path"))
            .collect();
        assert_eq!(paths, ["/rec/a.mkv", "/rec/b.mkv"]);
    }

    #[tokio::test]
    async fn get_timeline_returns_empty_array_when_nothing_matches() {
        let (config, root) = isolated_config("timeline-empty");
        let fx = fixture(config, &root);
        insert_segment(&fx.db, "cam-1", 1_000, Some(2_000), "/rec/a.mkv", false);
        insert_segment(&fx.db, "cam-1", 3_000, Some(4_000), "/rec/gone.mkv", true);

        for params in [
            json!({"camera_id": "cam-1", "start_ms": 2_001, "end_ms": 2_999}),
            json!({"camera_id": "cam-1", "start_ms": 3_100, "end_ms": 3_900}),
            json!({"camera_id": "cam-1", "start_ms": 0, "end_ms": 999}),
            json!({"camera_id": "unknown", "start_ms": 0, "end_ms": i64::MAX}),
        ] {
            let rows = fx
                .handler
                .dispatch("playback.get_timeline", &params)
                .await
                .expect("timeline");
            assert_eq!(rows, json!([]), "params {params}");
        }
    }

    #[tokio::test]
    async fn get_timeline_indexes_local_segments_from_global_and_custom_roots() {
        let (mut config, root) = isolated_config("timeline-scan");
        let global = PathBuf::from(&config.recording.storage_path);
        let custom = root.join("custom");
        let mut cam = camera("cam-scan");
        cam.recording.local.storage_path = Some(custom.to_string_lossy().into_owned());
        config.cameras.push(cam);

        let dated = global.join("cam-scan").join("2026-09-18");
        let global_seg = dated.join("cam-scan_1700000000000_1_0.mkv");
        let custom_seg = custom
            .join("cam-scan")
            .join("cam-scan_1700000100000_2_0.mkv");
        let bogus_seg = dated.join("cam-scan_1700000200000_3_0.mkv");
        let foreign_seg = dated.join("other_1700000300000_4_0.mkv");
        write_file(&global_seg, &MATROSKA_HEADER);
        write_file(&custom_seg, &MATROSKA_HEADER);
        write_file(&bogus_seg, b"not a matroska file");
        write_file(&foreign_seg, &MATROSKA_HEADER);

        let fx = fixture(config, &root);
        // A stale index row for a file that is no longer valid Matroska is purged.
        insert_segment(
            &fx.db,
            "cam-scan",
            1_700_000_200_000,
            Some(1_700_000_260_000),
            &bogus_seg.to_string_lossy(),
            false,
        );

        let params = json!({
            "camera_id": "cam-scan",
            "start_ms": 1_699_999_000_000_i64,
            "end_ms": i64::MAX,
        });
        let rows = fx
            .handler
            .dispatch("playback.get_timeline", &params)
            .await
            .expect("timeline");
        let rows = rows.as_array().expect("array");
        assert_eq!(rows.len(), 2, "rows: {rows:?}");
        assert_eq!(rows[0]["start_ms"], json!(1_700_000_000_000_i64));
        assert_eq!(rows[0]["path"], json!(global_seg.to_string_lossy()));
        assert_eq!(rows[1]["start_ms"], json!(1_700_000_100_000_i64));
        assert_eq!(rows[1]["path"], json!(custom_seg.to_string_lossy()));
        for row in rows {
            let start = row["start_ms"].as_i64().expect("start");
            let end = row["end_ms"].as_i64().expect("scanned segments are closed");
            assert!(end > start, "row {row}");
        }

        // Refreshing again is idempotent and never indexes foreign files.
        fx.handler
            .dispatch("playback.get_timeline", &params)
            .await
            .expect("second timeline");
        assert_eq!(
            segment_rows(&fx.db, "cam-scan"),
            vec![
                (1_700_000_000_000, global_seg.to_string_lossy().into_owned()),
                (1_700_000_100_000, custom_seg.to_string_lossy().into_owned()),
            ]
        );
        assert!(segment_rows(&fx.db, "other").is_empty());
    }

    #[tokio::test]
    async fn get_timeline_tolerates_missing_recording_directories() {
        let (mut config, root) = isolated_config("timeline-missing-dir");
        let mut cam = camera("cam-missing");
        cam.recording.local.storage_path =
            Some(root.join("does-not-exist").to_string_lossy().into_owned());
        config.cameras.push(cam);
        let fx = fixture(config, &root);
        insert_segment(&fx.db, "cam-missing", 1_000, None, "/rec/kept.mkv", false);

        let rows = fx
            .handler
            .dispatch(
                "playback.get_timeline",
                &json!({"camera_id": "cam-missing", "start_ms": 0, "end_ms": 10_000}),
            )
            .await
            .expect("timeline");
        assert_eq!(
            rows,
            json!([{"start_ms": 1_000, "end_ms": null, "path": "/rec/kept.mkv"}])
        );
        assert!(!root.join("global").exists());
        assert!(!root.join("does-not-exist").exists());
    }

    // ------------------------------------------------------------------
    // playback.open without a playable segment
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn open_without_any_matching_segment_is_an_internal_error() {
        let (mut config, root) = isolated_config("open-none");
        config.cameras.push(camera("cam-empty"));
        let fx = fixture(config, &root);
        // Only unusable rows: another camera's recording and a quota-deleted one.
        insert_segment(&fx.db, "cam-other", 1_000, Some(2_000), "/rec/o.mkv", false);
        insert_segment(&fx.db, "cam-empty", 1_000, Some(2_000), "/rec/d.mkv", true);

        for (camera_id, ts_ms) in [("cam-empty", 1_234_i64), ("cam-unconfigured", -5)] {
            assert_rpc_error(
                fx.handler
                    .dispatch(
                        "playback.open",
                        &json!({"camera_id": camera_id, "ts_ms": ts_ms}),
                    )
                    .await,
                INTERNAL_ERROR,
                &format!("No segment found for camera '{camera_id}' near ts {ts_ms}"),
            );
        }
        assert!(fx.state.read().await.playback_sessions.is_empty());
    }

    #[tokio::test]
    async fn open_does_not_index_invalid_local_files() {
        let (mut config, root) = isolated_config("open-invalid-file");
        config.cameras.push(camera("cam-bad"));
        let bad = PathBuf::from(&config.recording.storage_path)
            .join("cam-bad")
            .join("2026-09-18")
            .join("cam-bad_1700000000000_1_0.mkv");
        write_file(&bad, b"\x00\x00\x00\x00 truncated");
        let fx = fixture(config, &root);

        assert_rpc_error(
            fx.handler
                .dispatch(
                    "playback.open",
                    &json!({"camera_id": "cam-bad", "ts_ms": 1_700_000_000_500_i64}),
                )
                .await,
            INTERNAL_ERROR,
            "No segment found for camera 'cam-bad' near ts 1700000000500",
        );
        assert!(segment_rows(&fx.db, "cam-bad").is_empty());
        assert!(fx.state.read().await.playback_sessions.is_empty());
    }

    // ------------------------------------------------------------------
    // Segment resolution and playlist edge cases
    // ------------------------------------------------------------------

    #[test]
    fn resolve_open_ended_segments_use_configured_duration_with_floor() {
        let conn = schema_conn();
        seed(&conn, &[("cam", 1_000, None, "/rec/open.mkv", false)]);

        let resolved = |ts, duration| resolve_segment_for_ts(&conn, "cam", ts, duration);
        assert_eq!(
            resolved(1_500, 60_000),
            Some((1_000, 61_000, "/rec/open.mkv".to_owned(), 1_500))
        );
        // Past the nominal end of an open segment the open point is clamped.
        assert_eq!(
            resolved(100_000, 60_000),
            Some((1_000, 61_000, "/rec/open.mkv".to_owned(), 61_000))
        );
        // Zero or tiny configured durations fall back to a one-second window.
        for duration in [0, 500, -60_000] {
            assert_eq!(
                resolved(1_500, duration),
                Some((1_000, 2_000, "/rec/open.mkv".to_owned(), 1_500)),
                "duration {duration}"
            );
        }
        // An open segment that starts after the request is reached as "next".
        assert_eq!(
            resolved(500, 60_000),
            Some((1_000, 61_000, "/rec/open.mkv".to_owned(), 1_000))
        );
    }

    #[test]
    fn resolve_zero_length_segments_get_a_minimum_one_ms_span() {
        let conn = schema_conn();
        seed(
            &conn,
            &[("cam", 1_000, Some(1_000), "/rec/zero.mkv", false)],
        );

        assert_eq!(
            resolve_segment_for_ts(&conn, "cam", 1_000, 60_000),
            Some((1_000, 1_001, "/rec/zero.mkv".to_owned(), 1_000))
        );
        // Snapping to the previous zero-length segment clamps to its padded end.
        assert_eq!(
            resolve_segment_for_ts(&conn, "cam", 5_000, 60_000),
            Some((1_000, 1_001, "/rec/zero.mkv".to_owned(), 1_001))
        );
    }

    #[test]
    fn resolve_prefers_latest_starting_overlap_and_skips_deleted_rows() {
        let conn = schema_conn();
        seed(
            &conn,
            &[
                ("cam", 1_000, Some(5_000), "/rec/long.mkv", false),
                ("cam", 2_000, Some(3_000), "/rec/short.mkv", false),
                ("cam", 6_000, Some(7_000), "/rec/deleted.mkv", true),
                ("cam", 8_000, Some(9_000), "/rec/last.mkv", false),
                ("other", 6_000, Some(7_000), "/rec/other.mkv", false),
            ],
        );

        let path_and_open = |ts| {
            resolve_segment_for_ts(&conn, "cam", ts, 60_000)
                .map(|(_, _, path, open_ts)| (path, open_ts))
        };
        assert_eq!(path_and_open(2_500), Some(("/rec/short.mkv".into(), 2_500)));
        assert_eq!(path_and_open(4_000), Some(("/rec/long.mkv".into(), 4_000)));
        assert_eq!(path_and_open(0), Some(("/rec/long.mkv".into(), 1_000)));
        // Deleted and foreign segments are invisible: the gap jumps forward.
        assert_eq!(path_and_open(6_500), Some(("/rec/last.mkv".into(), 8_000)));
        assert_eq!(path_and_open(9_999), Some(("/rec/last.mkv".into(), 9_000)));
    }

    #[test]
    fn resolve_returns_none_without_usable_segments() {
        let conn = schema_conn();
        assert_eq!(resolve_segment_for_ts(&conn, "cam", 1_000, 60_000), None);

        seed(
            &conn,
            &[
                ("cam", 1_000, Some(2_000), "/rec/deleted.mkv", true),
                ("other", 1_000, Some(2_000), "/rec/other.mkv", false),
            ],
        );
        assert_eq!(resolve_segment_for_ts(&conn, "cam", 1_500, 60_000), None);

        // Query failures (no schema) degrade to "no segment" instead of panicking.
        let bare = Connection::open_in_memory().expect("in-memory SQLite");
        assert_eq!(resolve_segment_for_ts(&bare, "cam", 1_500, 60_000), None);
    }

    #[test]
    fn playlist_normalizes_following_segments_and_filters_unusable_rows() {
        let conn = schema_conn();
        seed(
            &conn,
            &[
                ("cam", 0, Some(500), "/rec/earlier.mkv", false),
                ("cam", 1_000, Some(2_000), "/rec/a.mkv", false),
                ("cam", 2_000, None, "/rec/open.mkv", false),
                ("cam", 3_000, Some(3_000), "/rec/zero.mkv", false),
                ("cam", 3_500, Some(3_600), "/rec/zero.mkv", false),
                ("cam", 4_000, Some(5_000), "/rec/deleted.mkv", true),
                ("other", 5_000, Some(6_000), "/rec/other.mkv", false),
            ],
        );
        let current = (1_000, 2_000, "/rec/a.mkv".to_owned(), 1_500);

        let spans = |duration| {
            playlist_from_segment(&conn, "cam", &current, duration)
                .into_iter()
                .map(|s| (s.path, s.start_ts, s.end_ts))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            spans(60_000),
            vec![
                ("/rec/a.mkv".to_owned(), 1_000, 2_000),
                ("/rec/open.mkv".to_owned(), 2_000, 62_000),
                ("/rec/zero.mkv".to_owned(), 3_000, 3_001),
            ]
        );
        assert_eq!(spans(0)[1], ("/rec/open.mkv".to_owned(), 2_000, 3_000));
    }

    #[test]
    fn playlist_keeps_the_resolved_segment_when_the_index_is_unavailable() {
        let bare = Connection::open_in_memory().expect("in-memory SQLite");
        let current = (1_000, 2_000, "/rec/current.mkv".to_owned(), 1_000);

        let playlist = playlist_from_segment(&bare, "cam", &current, 60_000);
        assert_eq!(playlist.len(), 1);
        assert_eq!(playlist[0].path, "/rec/current.mkv");
        assert_eq!((playlist[0].start_ts, playlist[0].end_ts), (1_000, 2_000));

        // A resolved segment that is not (or no longer) indexed still leads.
        let conn = schema_conn();
        seed(
            &conn,
            &[("cam", 2_000, Some(3_000), "/rec/next.mkv", false)],
        );
        let paths: Vec<String> = playlist_from_segment(&conn, "cam", &current, 60_000)
            .into_iter()
            .map(|s| s.path)
            .collect();
        assert_eq!(paths, ["/rec/current.mkv", "/rec/next.mkv"]);
    }

    #[test]
    fn expand_user_path_only_expands_home_prefix() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_user_path("~/Videos/nw"), home.join("Videos/nw"));
        }
        for raw in ["/srv/recordings", "relative/dir", "~", "~other/clips", ""] {
            assert_eq!(expand_user_path(raw), PathBuf::from(raw), "raw {raw:?}");
        }
    }

    #[test]
    fn coverage_merges_contiguous_closed_segments_and_keeps_the_newest_ranges() {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(
            "CREATE TABLE segments (
                id INTEGER PRIMARY KEY, camera_id TEXT NOT NULL, start_ts INTEGER NOT NULL,
                end_ts INTEGER, path TEXT NOT NULL, local_deleted INTEGER NOT NULL DEFAULT 0);",
        )
        .expect("create schema");
        let rows: [(&str, i64, Option<i64>, i64); 8] = [
            ("cam-1", 10_000, Some(70_000), 0),
            ("cam-1", 70_500, Some(130_000), 0), // contiguous within 2 s: merged
            ("cam-1", 200_000, Some(260_000), 0),
            ("cam-1", 300_000, Some(360_000), 1), // deleted: skipped
            ("cam-1", 400_000, None, 0),          // no end yet: skipped
            ("cam-1", 460_000, Some(495_000), 0), // written to recently (still open): skipped
            ("cam-1", 1_000, Some(5_000), 0),     // before `since`: skipped
            ("cam-2", 10_000, Some(20_000), 0),
        ];
        for (camera, start, end, deleted) in rows {
            conn.execute(
                "INSERT INTO segments (camera_id, start_ts, end_ts, path, local_deleted)
                 VALUES (?1, ?2, ?3, '/tmp/x.mkv', ?4)",
                params![camera, start, end, deleted],
            )
            .expect("insert segment");
        }

        assert_eq!(
            recording_coverage(&conn, "cam-1", 8_000, 490_000, 10),
            vec![(10_000, 130_000), (200_000, 260_000)]
        );
        assert_eq!(
            recording_coverage(&conn, "cam-1", 8_000, 490_000, 1),
            vec![(200_000, 260_000)]
        );
        assert!(recording_coverage(&conn, "cam-3", 0, 490_000, 10).is_empty());
    }

    #[test]
    fn segments_in_range_keep_overlapping_files_once_in_start_order() {
        let conn = schema_conn();
        seed(
            &conn,
            &[
                ("cam", 0, Some(1_000), "/a.mkv", false),
                ("cam", 1_000, Some(2_000), "/b.mkv", false),
                ("cam", 2_000, Some(3_000), "/c.mkv", true),
                ("cam", 3_000, None, "/d.mkv", false),
                ("other", 1_000, Some(2_000), "/e.mkv", false),
            ],
        );
        let paths = |start, end| -> Vec<String> {
            segments_in_range(&conn, "cam", start, end, 60_000)
                .into_iter()
                .map(|s| s.path)
                .collect()
        };
        assert_eq!(paths(500, 3_500), vec!["/a.mkv", "/b.mkv", "/d.mkv"]);
        assert_eq!(paths(1_000, 1_500), vec!["/b.mkv"]);
        let open = segments_in_range(&conn, "cam", 3_000, 4_000, 60_000);
        assert_eq!(
            open[0].end_ts, 63_000,
            "open segment gets the configured length"
        );
    }

    #[test]
    fn timeline_query_includes_open_segments() {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(
            "\
            CREATE TABLE segments (
                id INTEGER PRIMARY KEY,
                camera_id TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER,
                path TEXT NOT NULL,
                local_deleted INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("create schema");

        conn.execute(
            "INSERT INTO segments (camera_id, start_ts, end_ts, path, local_deleted)
             VALUES (?1, ?2, NULL, ?3, 0)",
            params!["cam-1", 1_000_i64, "/tmp/open.mkv"],
        )
        .expect("insert segment");

        let mut stmt = conn.prepare(GET_TIMELINE_SQL).expect("prepare query");
        let rows = stmt
            .query_map(params!["cam-1", 1_500_i64, 2_000_i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .expect("run query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect rows");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 1_000_i64);
        assert_eq!(rows[0].1, None);
        assert_eq!(rows[0].2, "/tmp/open.mkv");
    }

    #[test]
    fn resolve_segment_for_ts_handles_gaps_and_edges() {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(
            "\
            CREATE TABLE segments (
                id INTEGER PRIMARY KEY,
                camera_id TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER,
                path TEXT NOT NULL,
                local_deleted INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("create schema");

        conn.execute(
            "INSERT INTO segments (camera_id, start_ts, end_ts, path, local_deleted)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params!["cam-1", 1_000_i64, 2_000_i64, "/tmp/a.mkv"],
        )
        .expect("insert first segment");
        conn.execute(
            "INSERT INTO segments (camera_id, start_ts, end_ts, path, local_deleted)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params!["cam-1", 3_000_i64, 4_000_i64, "/tmp/b.mkv"],
        )
        .expect("insert second segment");

        let inside = resolve_segment_for_ts(&conn, "cam-1", 1_500_i64, 60_000).expect("inside");
        assert_eq!(inside.0, 1_000_i64);
        assert_eq!(inside.1, 2_000_i64);
        assert_eq!(inside.2, "/tmp/a.mkv");
        assert_eq!(inside.3, 1_500_i64);

        let in_gap = resolve_segment_for_ts(&conn, "cam-1", 2_500_i64, 60_000).expect("in gap");
        assert_eq!(in_gap.0, 3_000_i64);
        assert_eq!(in_gap.1, 4_000_i64);
        assert_eq!(in_gap.2, "/tmp/b.mkv");
        assert_eq!(in_gap.3, 3_000_i64);

        let after_last =
            resolve_segment_for_ts(&conn, "cam-1", 4_500_i64, 60_000).expect("after last");
        assert_eq!(after_last.0, 3_000_i64);
        assert_eq!(after_last.1, 4_000_i64);
        assert_eq!(after_last.2, "/tmp/b.mkv");
        assert_eq!(after_last.3, 4_000_i64);
    }

    #[test]
    fn playlist_continues_from_resolved_segment_without_duplicate_paths() {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(
            "\
            CREATE TABLE segments (
                id INTEGER PRIMARY KEY,
                camera_id TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER,
                path TEXT NOT NULL,
                local_deleted INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("create schema");
        for (id, start, end, path) in [
            (1, 1_000_i64, 2_000_i64, "/tmp/a.mkv"),
            (2, 2_000_i64, 3_000_i64, "/tmp/b.mkv"),
            (3, 3_000_i64, 4_000_i64, "/tmp/c.mkv"),
        ] {
            conn.execute(
                "INSERT INTO segments (id, camera_id, start_ts, end_ts, path, local_deleted) \
                 VALUES (?1, 'cam-1', ?2, ?3, ?4, 0)",
                params![id, start, end, path],
            )
            .expect("insert segment");
        }

        let resolved = (2_000, 3_000, "/tmp/b.mkv".to_owned(), 2_250);
        let playlist = playlist_from_segment(&conn, "cam-1", &resolved, 60_000);

        assert_eq!(playlist.len(), 2);
        assert_eq!(playlist[0].path, "/tmp/b.mkv");
        assert_eq!(playlist[1].path, "/tmp/c.mkv");
        assert_eq!((playlist[1].start_ts, playlist[1].end_ts), (3_000, 4_000));
    }
}
