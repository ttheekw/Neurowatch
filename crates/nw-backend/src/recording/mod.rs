//! Recording subsystem — state management, segment tracking, and retention
//! (camera quotas and the disk threshold).

pub mod archive;
pub mod event_retention;
pub mod mkv_timing;
pub mod policy;
pub mod quota;
pub mod retention;
pub mod segment_tracker;
pub mod supervisor;

/// Footage around a kept bookmark: this much before it starts and after it ends.
pub const KEPT_PAD_MS: i64 = 30_000;

/// SQL condition on a `segments` row: no kept bookmark covers it (padded by
/// [`KEPT_PAD_MS`]; a bookmark without a camera covers every camera). Every
/// cleanup that deletes recordings includes it.
pub const NOT_KEPT: &str = "NOT EXISTS (SELECT 1 FROM events e WHERE e.kept = 1 \
     AND e.event_type = 'bookmark' \
     AND (e.camera_id IS NULL OR e.camera_id = segments.camera_id) \
     AND e.ts - 30000 < COALESCE(segments.end_ts, segments.start_ts + 86400000) \
     AND COALESCE(e.end_ts, e.ts) + 30000 > segments.start_ts)";
