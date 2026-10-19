//! Event-retention bookkeeping. Legacy continuous footage is never eligible,
//! and event marks survive expiry of the event-history row that created them.
use rusqlite::{params, Connection};

#[derive(Default)]
pub struct EligibilityClock {
    intervals: std::collections::HashMap<String, (String, i64, i64)>,
}

impl EligibilityClock {
    /// Restart enrollment on config changes, detector loss, manual override,
    /// schedule changes, or backwards wall-clock changes. Keeping extra
    /// boundary footage is preferable to deleting uncertain coverage.
    pub fn observe(
        &mut self,
        camera: &str,
        fingerprint: &str,
        healthy_events: bool,
        now: i64,
    ) -> Option<i64> {
        if !healthy_events {
            self.intervals.remove(camera);
            return None;
        }
        let interval = self
            .intervals
            .entry(camera.to_owned())
            .or_insert_with(|| (fingerprint.to_owned(), now, now));
        if interval.0 != fingerprint || now < interval.2 {
            *interval = (fingerprint.to_owned(), now, now);
        }
        interval.2 = now;
        Some(interval.1)
    }

    pub fn retain_cameras(&mut self, cameras: &[nw_core::camera::CameraConfig]) {
        self.intervals
            .retain(|id, _| cameras.iter().any(|camera| &camera.id == id));
    }

    pub fn since(&self, camera: &str) -> Option<i64> {
        self.intervals.get(camera).map(|interval| interval.1)
    }
}

pub struct Candidate {
    pub id: i64,
    pub path: String,
}

/// Mark only finished files captured inside the current healthy automatic
/// event-policy interval. `None` updates event marks without enrolling files.
pub fn refresh_flags(
    conn: &Connection,
    camera: &str,
    eligible_since: Option<i64>,
    pre_sec: u32,
    post_sec: u32,
    now_ms: i64,
) -> rusqlite::Result<()> {
    if let Some(since) = eligible_since {
        conn.execute(
            "UPDATE segments SET event_retention_eligible=1
             WHERE camera_id=?1 AND local_deleted=0 AND start_ts>=?2
               AND end_ts IS NOT NULL AND end_ts<=?3 AND timing_exact>0 AND event_retention_eligible=0",
            params![camera, since, now_ms],
        )?;
    }
    conn.execute(
        "UPDATE segments SET has_event=1 WHERE camera_id=?1 AND has_event=0
         AND EXISTS (
           SELECT 1 FROM events e WHERE e.camera_id=?1
             AND e.event_type IN ('motion','intrusion','line_crossing','tamper','digital_input','camera_event','bookmark')
             AND e.ts-?2<=segments.end_ts
             AND (e.end_ts IS NULL OR e.end_ts+?3>=segments.start_ts))",
        params![camera, i64::from(pre_sec)*1000, i64::from(post_sec)*1000],
    )?;
    Ok(())
}

/// Bounded oldest-first pass. Callers must recheck filesystem safety and the
/// row's eligibility before deleting; this query alone is not permission.
pub fn trim_candidates(
    conn: &Connection,
    camera: &str,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id,path FROM segments WHERE camera_id=?1 AND local_deleted=0
         AND event_retention_eligible=1 AND has_event=0
         AND end_ts IS NOT NULL AND end_ts<?2 AND timing_exact>0 AND {}
         ORDER BY start_ts,id LIMIT 128",
        super::NOT_KEPT
    ))?;
    let rows = stmt
        .query_map(params![camera, cutoff_ms], |r| {
            Ok(Candidate {
                id: r.get(0)?,
                path: r.get(1)?,
            })
        })?
        .collect();
    rows
}

/// Delete at most one batch of settled, unmarked, enrolled recordings, only
/// inside this camera's configured tree. Never follows a final symlink, never
/// deletes a directory, and never touches a recently modified file.
pub fn prune_unmarked(
    db: &std::sync::Arc<std::sync::Mutex<Connection>>,
    camera: &str,
    storage_root: &std::path::Path,
    cutoff_ms: i64,
    active_after: std::time::SystemTime,
) -> anyhow::Result<usize> {
    let Ok(root) = storage_root.canonicalize() else {
        return Ok(0);
    };
    let Ok(camera_root) = root.join(camera).canonicalize() else {
        return Ok(0);
    };
    if camera_root == root || !camera_root.starts_with(&root) {
        return Ok(0);
    }
    let mut conn = db
        .lock()
        .map_err(|_| anyhow::anyhow!("Recording database unavailable"))?;
    let tx = conn.transaction()?;
    let candidates = trim_candidates(&tx, camera, cutoff_ms)?;
    let mut deleted = 0;
    for candidate in candidates {
        let path = std::path::Path::new(&candidate.path);
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if !meta.is_file()
            || meta.is_symlink()
            || meta
                .modified()
                .map_or(true, |modified| modified > active_after)
        {
            continue;
        }
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&camera_root) {
            continue;
        }
        // The transaction serializes any event-mark changes over this batch.
        // Unlink the validated canonical target rather than resolving the
        // indexed path again. No recursive removal or broad filesystem scan.
        match std::fs::remove_file(&canonical) {
            Ok(()) => {
                tx.execute("DELETE FROM segments WHERE id=?1", [candidate.id])?;
                deleted += 1;
            }
            Err(error) => {
                tracing::warn!(camera_id=camera, %error, "Cannot trim unmarked recording")
            }
        }
    }
    tx.commit()?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "real x264 splitmux recording and filesystem cleanup"]
    fn recorded_segments_keep_event_padding_and_legacy_footage_while_trimming_unmarked() {
        use gstreamer::prelude::*;
        use std::{
            sync::{Arc, Mutex},
            time::{Duration, SystemTime},
        };
        gstreamer::init().unwrap();
        let root = std::env::temp_dir().join(format!("nw-retention-{}", uuid::Uuid::new_v4()));
        let dir = root.join("cam");
        std::fs::create_dir_all(&dir).unwrap();
        let epoch: i64 = 1_789_659_000_000;
        let location = dir.join(format!("cam_{epoch}_1_%05d.mkv"));
        let pipeline = gstreamer::parse::launch(&format!(
            "videotestsrc num-buffers=160 ! video/x-raw,width=64,height=36,framerate=10/1 \
             ! videoconvert ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=10 \
             ! h264parse ! splitmuxsink location=\"{}\" max-size-time=3000000000 muxer-factory=matroskamux", location.display()))
            .unwrap().downcast::<gstreamer::Pipeline>().unwrap();
        pipeline.set_state(gstreamer::State::Playing).unwrap();
        let done = pipeline.bus().unwrap().timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(20),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        let _ = pipeline.set_state(gstreamer::State::Null);
        assert!(done.is_some_and(|m| m.type_() == gstreamer::MessageType::Eos));
        for file in std::fs::read_dir(&dir).unwrap().flatten() {
            std::fs::File::options()
                .write(true)
                .open(file.path())
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(600))
                .unwrap();
        }
        let db = Arc::new(Mutex::new(crate::db::connection::open_in_memory().unwrap()));
        crate::recording::segment_tracker::scan_and_record(&db, &"cam".into(), &dir, 3_000);
        let rows: Vec<(String, i64, i64)> = {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT path,start_ts,end_ts FROM segments ORDER BY start_ts")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(rows.len() >= 5);
        let since = rows[1].1;
        let event = rows[2].1 + 500;
        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO events (id,ts,end_ts,last_ts,camera_id,event_type,source) VALUES (1,?1,?1,?1,'cam','motion','builtin')", [event]).unwrap();
            refresh_flags(&conn, "cam", Some(since), 1, 1, epoch + 600_000).unwrap();
            // Event padding overlaps the preceding segment; both stay marked.
            for index in [1, 2] {
                assert_eq!(
                    conn.query_row(
                        "SELECT has_event FROM segments WHERE path=?1",
                        [&rows[index].0],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                    1
                );
            }
            // Removing history never clears already-sticky file protection.
            conn.execute("DELETE FROM events", []).unwrap();
            refresh_flags(&conn, "cam", None, 0, 0, epoch + 600_000).unwrap();
        }
        assert_eq!(
            prune_unmarked(
                &db,
                "cam",
                &root,
                epoch + 600_000,
                SystemTime::now() - Duration::from_secs(60)
            )
            .unwrap(),
            rows.len() - 3
        );
        for (index, (path, _, _)) in rows.iter().enumerate() {
            assert_eq!(
                std::path::Path::new(path).exists(),
                index <= 2,
                "legacy continuous and padded event files survive; unmarked eligible files go"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unhealthy_detection_and_manual_overrides_never_enroll_footage() {
        let mut clock = EligibilityClock::default();
        assert_eq!(clock.observe("cam", "policy", true, 1000), Some(1000));
        assert_eq!(clock.observe("cam", "policy", true, 2000), Some(1000));
        assert_eq!(clock.observe("cam", "policy", false, 3000), None);
        assert_eq!(clock.observe("cam", "policy", true, 4000), Some(4000));
        assert_eq!(clock.observe("cam", "edited", true, 5000), Some(5000));
        assert_eq!(clock.observe("cam", "edited", true, 2000), Some(2000));
    }

    #[test]
    fn cleanup_keeps_active_unsettled_and_outside_root_files() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, SystemTime};
        let root = std::env::temp_dir().join(format!(
            "nw-event-retention-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("cam")).unwrap();
        let db = Arc::new(Mutex::new(crate::db::connection::open_in_memory().unwrap()));
        for name in ["trim", "active", "unsettled", "event"] {
            let file = root.join("cam").join(format!("{name}.mkv"));
            std::fs::write(&file, b"test").unwrap();
            db.lock().unwrap().execute("INSERT INTO segments(camera_id,start_ts,end_ts,path,size_bytes,event_retention_eligible,has_event,timing_exact) VALUES('cam',1000,2000,?1,4,1,?2,?3)", params![file.to_string_lossy(), i64::from(name=="event"), i64::from(name!="unsettled")]).unwrap();
        }
        let outside = root.join("outside.mkv");
        std::fs::write(&outside, b"outside").unwrap();
        db.lock().unwrap().execute("INSERT INTO segments(camera_id,start_ts,end_ts,path,event_retention_eligible,timing_exact) VALUES('cam',1000,2000,?1,1,1)",[outside.to_string_lossy()]).unwrap();
        // Deterministic modification times, without touching any real recordings.
        let old = SystemTime::now() - Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(root.join("cam/trim.mkv"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(
            prune_unmarked(
                &db,
                "cam",
                &root,
                3000,
                SystemTime::now() - Duration::from_secs(60)
            )
            .unwrap(),
            1
        );
        assert!(!root.join("cam/trim.mkv").exists());
        for path in [
            "cam/active.mkv",
            "cam/unsettled.mkv",
            "cam/event.mkv",
            "outside.mkv",
        ] {
            assert!(root.join(path).exists(), "{path}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn only_new_event_policy_intervals_can_be_trimmed_and_event_marks_are_sticky() {
        let conn = crate::db::connection::open_in_memory().unwrap();
        conn.execute_batch("INSERT INTO segments(camera_id,start_ts,end_ts,path,size_bytes,timing_exact) VALUES
            ('cam',1000,2000,'old-continuous.mkv',10,1), ('cam',5000,6000,'event.mkv',10,1),
            ('cam',10000,11000,'no-event.mkv',10,1), ('other',10000,11000,'other.mkv',10,1);
            INSERT INTO events(id,ts,end_ts,last_ts,camera_id,event_type,source) VALUES(1,5200,5500,5500,'cam','motion','builtin');").unwrap();
        refresh_flags(&conn, "cam", Some(4000), 0, 0, 20000).unwrap();
        assert_eq!(
            trim_candidates(&conn, "cam", 15000)
                .unwrap()
                .iter()
                .map(|r| r.path.as_str())
                .collect::<Vec<_>>(),
            vec!["no-event.mkv"]
        );
        conn.execute("DELETE FROM events", []).unwrap();
        refresh_flags(&conn, "cam", None, 0, 0, 20000).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT has_event FROM segments WHERE path='event.mkv'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT event_retention_eligible FROM segments WHERE path='old-continuous.mkv'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
