//! The `events` table (schema v7, `kept` since v9, see [`crate::db::schema`]). Times are
//! milliseconds since the Unix epoch, like the recording index.

use nw_core::event::{EventKind, EventRecord, EventSource};
use rusqlite::{params, Connection, OptionalExtension, Row};

const COLUMNS: &str = "id, ts, end_ts, camera_id, event_type, source, topic, score, zones, \
                       bbox, thumbnail, detail, acknowledged, kept";

/// Most events one query returns.
pub const MAX_LIST: u32 = 1000;

pub fn insert(conn: &Connection, record: &EventRecord, seen_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO events (id, ts, end_ts, last_ts, camera_id, event_type, source, \
         topic, score, zones, bbox, thumbnail, detail, acknowledged, kept) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            record.id,
            record.start_ms,
            record.end_ms,
            seen_ms,
            record.camera_id,
            record.kind.as_str(),
            record.source.as_str(),
            record.topic,
            record.score,
            zones_json(record),
            bbox_json(record),
            record.thumbnail,
            record.detail,
            record.acknowledged,
            record.kept,
        ],
    )
    .map(|_| ())
}

/// Store an event's new state. An event deleted meanwhile stays deleted.
pub fn update(conn: &Connection, record: &EventRecord, seen_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE events SET end_ts = ?2, last_ts = ?3, score = ?4, zones = ?5, bbox = ?6, \
         thumbnail = ?7, detail = ?8 WHERE id = ?1",
        params![
            record.id,
            record.end_ms,
            seen_ms,
            record.score,
            zones_json(record),
            bbox_json(record),
            record.thumbnail,
            record.detail,
        ],
    )
    .map(|_| ())
}

/// Change a bookmark's note (`Some(None)` clears it) or whether its footage
/// is kept. `None` when there is no bookmark with this id.
pub fn edit_bookmark(
    conn: &Connection,
    id: i64,
    note: Option<Option<&str>>,
    kept: Option<bool>,
) -> rusqlite::Result<Option<EventRecord>> {
    if let Some(note) = note {
        conn.execute(
            "UPDATE events SET detail = ?2 WHERE id = ?1 AND event_type = 'bookmark'",
            params![id, note],
        )?;
    }
    if let Some(kept) = kept {
        conn.execute(
            "UPDATE events SET kept = ?2 WHERE id = ?1 AND event_type = 'bookmark'",
            params![id, kept],
        )?;
    }
    Ok(get(conn, id)?.filter(|record| record.kind == EventKind::Bookmark))
}

pub fn touch(conn: &Connection, ids: &[i64], seen_ms: i64) -> rusqlite::Result<()> {
    let mut statement =
        conn.prepare_cached("UPDATE events SET last_ts = ?2 WHERE id = ?1 AND end_ts IS NULL")?;
    for id in ids {
        statement.execute(params![id, seen_ms])?;
    }
    Ok(())
}

/// Close events left open by a backend that stopped, at the last time they
/// were seen going on.
pub fn close_dangling(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE events SET end_ts = MAX(ts, COALESCE(last_ts, ts)) WHERE end_ts IS NULL",
        [],
    )
}

pub fn max_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<EventRecord>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM events WHERE id = ?1"),
        [id],
        from_row,
    )
    .optional()
}

/// Which events `events.list` returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub cameras: Vec<String>,
    pub kinds: Vec<EventKind>,
    /// Events going on at any time in `[from_ms, to_ms]`.
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// Paging: only events that sort after this one (newest first).
    pub before: Option<(i64, i64)>,
    pub unacknowledged_only: bool,
    pub ongoing: Option<bool>,
    /// Literal, case-insensitive phrase across event text and current camera names.
    pub search: Option<String>,
    pub search_cameras: Vec<String>,
    pub limit: u32,
}

/// Newest first.
pub fn list(conn: &Connection, filter: &Filter) -> rusqlite::Result<Vec<EventRecord>> {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    let mut bind = |value: rusqlite::types::Value| {
        values.push(value);
        format!("?{}", values.len())
    };
    if !filter.cameras.is_empty() {
        let marks: Vec<String> = filter
            .cameras
            .iter()
            .map(|c| bind(c.clone().into()))
            .collect();
        clauses.push(format!("camera_id IN ({})", marks.join(", ")));
    }
    if !filter.kinds.is_empty() {
        let marks: Vec<String> = filter
            .kinds
            .iter()
            .map(|k| bind(k.as_str().to_owned().into()))
            .collect();
        clauses.push(format!("event_type IN ({})", marks.join(", ")));
    }
    if let Some(to) = filter.to_ms {
        let mark = bind(to.into());
        clauses.push(format!("ts <= {mark}"));
    }
    if let Some(from) = filter.from_ms {
        let mark = bind(from.into());
        clauses.push(format!("(end_ts IS NULL OR end_ts >= {mark})"));
    }
    if let Some((ts, id)) = filter.before {
        let ts = bind(ts.into());
        let id = bind(id.into());
        clauses.push(format!("(ts < {ts} OR (ts = {ts} AND id < {id}))"));
    }
    if filter.unacknowledged_only {
        clauses.push("acknowledged = 0".into());
    }
    if let Some(ongoing) = filter.ongoing {
        clauses.push(
            if ongoing {
                "end_ts IS NULL"
            } else {
                "end_ts IS NOT NULL"
            }
            .into(),
        );
    }
    if let Some(search) = filter.search.as_ref().filter(|s| !s.is_empty()) {
        // instr(), not LIKE: %, _ and quotes are ordinary search characters.
        let mark = bind(search.clone().into());
        let mut alternatives = vec![format!(
            "instr(lower(replace(event_type, '_', ' ')), {mark}) > 0 OR \
             instr(lower(COALESCE(detail, '')), {mark}) > 0 OR \
             instr(lower(COALESCE(topic, '')), {mark}) > 0 OR \
             instr(lower(source), {mark}) > 0 OR \
             instr(lower(COALESCE(camera_id, '')), {mark}) > 0 OR \
             instr(lower(CASE event_type \
             WHEN 'line_crossing' THEN 'Line crossed' \
             WHEN 'digital_input' THEN 'Alarm input' \
             WHEN 'disk_low' THEN 'Recording drive almost full' \
             WHEN 'storage_missing' THEN 'Recording storage missing' \
             WHEN 'backend_started' THEN 'NeuroWatch started' \
             WHEN 'tamper' THEN 'Tampering' ELSE replace(event_type, '_', ' ') END), {mark}) > 0"
        )];
        if !filter.search_cameras.is_empty() {
            let marks: Vec<String> = filter
                .search_cameras
                .iter()
                .map(|c| bind(c.clone().into()))
                .collect();
            alternatives.push(format!("camera_id IN ({})", marks.join(", ")));
        }
        clauses.push(format!("({})", alternatives.join(" OR ")));
    }
    let limit = bind(i64::from(filter.limit.clamp(1, MAX_LIST)).into());
    let condition = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM events {condition} ORDER BY ts DESC, id DESC LIMIT {limit}"
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(values), from_row)?;
    rows.collect()
}

pub fn acknowledge(conn: &Connection, ids: &[i64], acknowledged: bool) -> rusqlite::Result<usize> {
    let mut statement = conn.prepare_cached("UPDATE events SET acknowledged = ?2 WHERE id = ?1")?;
    let mut changed = 0;
    for id in ids {
        changed += statement.execute(params![id, acknowledged])?;
    }
    Ok(changed)
}

/// Delete events; returns their thumbnails (relative paths) to remove.
pub fn delete(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<String>> {
    let mut thumbnails = Vec::new();
    for id in ids {
        let thumbnail: Option<Option<String>> = conn
            .query_row("SELECT thumbnail FROM events WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        conn.execute("DELETE FROM events WHERE id = ?1", [id])?;
        thumbnails.extend(thumbnail.flatten());
    }
    Ok(thumbnails)
}

/// Delete finished events that ended before `before_ms`; returns their
/// thumbnails to remove.
pub fn prune(conn: &Connection, before_ms: i64) -> rusqlite::Result<Vec<String>> {
    let thumbnails = {
        let mut statement = conn.prepare(
            "SELECT thumbnail FROM events WHERE end_ts IS NOT NULL AND end_ts < ?1 \
             AND kept = 0 AND thumbnail IS NOT NULL",
        )?;
        let rows = statement.query_map([before_ms], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    conn.execute(
        "DELETE FROM events WHERE end_ts IS NOT NULL AND end_ts < ?1 AND kept = 0",
        [before_ms],
    )?;
    Ok(thumbnails)
}

fn zones_json(record: &EventRecord) -> Option<String> {
    (!record.zones.is_empty())
        .then(|| serde_json::to_string(&record.zones).ok())
        .flatten()
}

fn bbox_json(record: &EventRecord) -> Option<String> {
    record
        .bbox
        .and_then(|bbox| serde_json::to_string(&bbox).ok())
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<EventRecord> {
    let kind: String = row.get(4)?;
    let source: String = row.get(5)?;
    let zones: Option<String> = row.get(8)?;
    let bbox: Option<String> = row.get(9)?;
    Ok(EventRecord {
        id: row.get(0)?,
        start_ms: row.get(1)?,
        end_ms: row.get(2)?,
        camera_id: row.get(3)?,
        kind: EventKind::parse(&kind).unwrap_or(EventKind::CameraEvent),
        source: EventSource::parse(&source).unwrap_or(EventSource::System),
        topic: row.get(6)?,
        score: row.get::<_, Option<f64>>(7)?.map(|s| s as f32),
        zones: zones
            .and_then(|z| serde_json::from_str(&z).ok())
            .unwrap_or_default(),
        bbox: bbox.and_then(|b| serde_json::from_str(&b).ok()),
        thumbnail: row.get(10)?,
        detail: row.get(11)?,
        acknowledged: row.get(12)?,
        kept: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        id: i64,
        cam: Option<&str>,
        kind: EventKind,
        start: i64,
        end: Option<i64>,
    ) -> EventRecord {
        EventRecord {
            id,
            camera_id: cam.map(str::to_owned),
            kind,
            source: EventSource::System,
            start_ms: start,
            end_ms: end,
            topic: None,
            score: Some(0.5),
            zones: vec!["z1".into()],
            bbox: Some([0.1, 0.2, 0.3, 0.4]),
            thumbnail: None,
            detail: None,
            acknowledged: false,
            kept: false,
        }
    }

    fn db_with(records: &[EventRecord]) -> Connection {
        let conn = crate::db::connection::open_in_memory().expect("db");
        for r in records {
            insert(&conn, r, r.start_ms).expect("insert");
        }
        conn
    }

    #[test]
    fn a_record_round_trips() {
        let original = record(1, Some("a"), EventKind::Intrusion, 10, Some(20));
        let conn = db_with(std::slice::from_ref(&original));
        assert_eq!(get(&conn, 1).unwrap(), Some(original));
        assert_eq!(get(&conn, 2).unwrap(), None);
    }

    #[test]
    fn list_filters_by_camera_kind_and_overlapping_time_newest_first() {
        let conn = db_with(&[
            record(1, Some("a"), EventKind::Motion, 100, Some(200)),
            record(2, Some("b"), EventKind::Motion, 150, Some(160)),
            record(3, Some("a"), EventKind::CameraOffline, 300, None),
            record(4, None, EventKind::DiskLow, 400, Some(410)),
        ]);
        let ids = |filter: Filter| -> Vec<i64> {
            list(
                &conn,
                &Filter {
                    limit: 100,
                    ..filter
                },
            )
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect()
        };
        assert_eq!(ids(Filter::default()), [4, 3, 2, 1]);
        assert_eq!(
            ids(Filter {
                cameras: vec!["a".into()],
                ..Default::default()
            }),
            [3, 1]
        );
        assert_eq!(
            ids(Filter {
                kinds: vec![EventKind::Motion],
                ..Default::default()
            }),
            [2, 1]
        );
        // Going on during 180..350: #1 (until 200) and the open #3.
        assert_eq!(
            ids(Filter {
                from_ms: Some(180),
                to_ms: Some(350),
                ..Default::default()
            }),
            [3, 1]
        );
        assert_eq!(
            ids(Filter {
                before: Some((300, 3)),
                ..Default::default()
            }),
            [2, 1]
        );
        assert_eq!(
            list(
                &conn,
                &Filter {
                    limit: 1,
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn kept_bookmarks_never_expire() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::apply_migrations(&conn).unwrap();
        let mut kept = record(1, Some("c"), EventKind::Bookmark, 10, Some(10));
        kept.kept = true;
        insert(&conn, &kept, 10).unwrap();
        insert(
            &conn,
            &record(2, Some("c"), EventKind::Bookmark, 10, Some(10)),
            10,
        )
        .unwrap();
        prune(&conn, 1_000).unwrap();
        assert!(get(&conn, 1).unwrap().is_some_and(|r| r.kept));
        assert!(get(&conn, 2).unwrap().is_none());
        assert!(edit_bookmark(&conn, 1, None, Some(false))
            .unwrap()
            .is_some_and(|r| !r.kept));
    }

    #[test]
    fn updates_never_bring_back_a_deleted_event() {
        let mut open = record(1, Some("a"), EventKind::Motion, 100, None);
        let conn = db_with(std::slice::from_ref(&open));
        delete(&conn, &[1]).unwrap();
        open.end_ms = Some(150);
        update(&conn, &open, 150).unwrap();
        assert_eq!(get(&conn, 1).unwrap(), None);
    }

    #[test]
    fn search_is_literal_and_combines_with_status_and_paging() {
        let mut alarm = record(1, Some("a"), EventKind::DigitalInput, 100, Some(200));
        alarm.detail = Some("Loading dock at 95% capacity".into());
        alarm.topic = Some("Device/Trigger/Relay".into());
        let conn = db_with(&[
            alarm,
            record(2, Some("b"), EventKind::CameraOffline, 150, None),
            record(3, Some("a"), EventKind::DigitalInput, 300, None),
        ]);
        let ids = |search: &str, extra: Filter| -> Vec<i64> {
            list(
                &conn,
                &Filter {
                    search: Some(search.into()),
                    limit: 100,
                    ..extra
                },
            )
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect()
        };
        assert_eq!(ids("alarm input", Filter::default()), [3, 1]);
        assert_eq!(ids("loading dock", Filter::default()), [1]);
        assert_eq!(ids("relay", Filter::default()), [1]);
        assert_eq!(ids("95%", Filter::default()), [1]);
        assert!(ids("_", Filter::default()).is_empty());
        assert!(ids("' OR 1=1 --", Filter::default()).is_empty());
        assert_eq!(
            ids(
                "alarm",
                Filter {
                    ongoing: Some(false),
                    ..Default::default()
                }
            ),
            [1]
        );
        assert_eq!(
            ids(
                "alarm",
                Filter {
                    ongoing: Some(true),
                    ..Default::default()
                }
            ),
            [3]
        );
        assert_eq!(
            ids(
                "alarm",
                Filter {
                    before: Some((300, 3)),
                    ..Default::default()
                }
            ),
            [1]
        );
        assert_eq!(
            ids(
                "entrance",
                Filter {
                    search_cameras: vec!["b".into()],
                    ..Default::default()
                }
            ),
            [2]
        );
    }

    #[test]
    fn prune_removes_only_finished_old_events() {
        let conn = db_with(&[
            record(1, Some("a"), EventKind::Motion, 100, Some(200)),
            record(2, Some("a"), EventKind::Motion, 100, None),
            record(3, Some("a"), EventKind::Motion, 500, Some(600)),
        ]);
        prune(&conn, 300).unwrap();
        let left: Vec<i64> = list(
            &conn,
            &Filter {
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(left, [3, 2]);
    }

    #[test]
    fn acknowledge_marks_events() {
        let conn = db_with(&[record(1, Some("a"), EventKind::Motion, 1, Some(2))]);
        assert_eq!(acknowledge(&conn, &[1, 9], true).unwrap(), 1);
        assert!(get(&conn, 1).unwrap().unwrap().acknowledged);
        let unacknowledged = list(
            &conn,
            &Filter {
                unacknowledged_only: true,
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(unacknowledged.is_empty());
    }
}
