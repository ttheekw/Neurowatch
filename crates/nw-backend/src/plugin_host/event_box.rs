//! The box of a plugin event, taken from the drawing of the very picture the
//! event's thumbnail shows. The plugin's own start box belongs to the picture
//! it analysed when the event started; the thumbnail is the first keyframe at
//! or after that, so the two differ. Drawings are kept in [`super::history`]
//! with the same picture clock as keyframes (`frames::Identity::time_us`).
use super::history::{DrawingRecord, History, PAGE};
use nw_analytics_protocol::messages::OverlayItem;

/// How far a drawing's picture may be from the keyframe's, µs.
pub const TOLERANCE_US: i64 = 500_000;
/// Pages of drawings read for one lookup (a busy plugin has many).
const MAX_PAGES: usize = 8;

/// The drawing nearest `picture_us` that has boxes, among drawings of one of
/// `plugins`, and within [`TOLERANCE_US`]; its box for the event's object.
/// `label` is the event's class label (a box label starts with it, ignoring
/// case); `reference` the plugin's own start box. A box with the class label
/// wins over the others, then the one nearest `reference`.
pub fn matching_box(
    records: &[DrawingRecord],
    plugins: &[String],
    picture_us: i64,
    label: Option<&str>,
    reference: Option<[f32; 4]>,
) -> Option<[f32; 4]> {
    let nearest = records
        .iter()
        .filter(|record| plugins.contains(&record.plugin))
        .filter(|record| record.items.iter().any(|item| box_of(item).is_some()))
        .map(|record| record.time_us)
        .filter(|time| time.abs_diff(picture_us) <= TOLERANCE_US as u64)
        .min_by_key(|time| time.abs_diff(picture_us))?;
    let boxes: Vec<(Option<&str>, [f32; 4])> = records
        .iter()
        .filter(|record| record.time_us == nearest && plugins.contains(&record.plugin))
        .flat_map(|record| record.items.iter().filter_map(box_of))
        .collect();
    let wanted = label.map(str::trim).filter(|label| !label.is_empty());
    let labelled: Vec<[f32; 4]> = boxes
        .iter()
        .filter(|(text, _)| {
            wanted.is_some_and(|wanted| {
                text.is_some_and(|text| {
                    text.trim_start()
                        .to_lowercase()
                        .starts_with(&wanted.to_lowercase())
                })
            })
        })
        .map(|(_, rect)| *rect)
        .collect();
    let candidates: Vec<[f32; 4]> = if labelled.is_empty() {
        boxes.iter().map(|(_, rect)| *rect).collect()
    } else {
        labelled
    };
    match reference {
        Some(reference) => candidates.into_iter().min_by(|a, b| {
            distance(a, &reference)
                .partial_cmp(&distance(b, &reference))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        None => candidates.into_iter().next(),
    }
}

fn distance(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum()
}

/// A box item with a usable normalised rectangle (as events accept them).
fn box_of(item: &OverlayItem) -> Option<(Option<&str>, [f32; 4])> {
    let OverlayItem::Box { rect, label, .. } = item else {
        return None;
    };
    let valid = rect
        .iter()
        .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        && rect[2] > 0.0
        && rect[3] > 0.0
        && rect[0] + rect[2] <= 1.0 + 1e-3
        && rect[1] + rect[3] <= 1.0 + 1e-3;
    valid.then_some((label.as_deref(), *rect))
}

/// Read the drawings around `picture_us` from the history (pending ones are
/// written first) and choose the event's box. Blocking.
pub fn lookup(
    history: &History,
    camera_id: &str,
    plugins: &[String],
    picture_us: i64,
    label: Option<&str>,
    reference: Option<[f32; 4]>,
) -> Option<[f32; 4]> {
    if plugins.is_empty() {
        return None;
    }
    history.flush();
    let to = picture_us + TOLERANCE_US;
    let mut records = Vec::new();
    let mut after = None;
    for _ in 0..MAX_PAGES {
        let page = history
            .list(camera_id, picture_us - TOLERANCE_US, to, after, PAGE)
            .ok()?;
        let last = page.last().map(|record| (record.time_us, record.id));
        let full = page.len() >= PAGE;
        records.extend(page);
        match last {
            Some(last) if full => after = Some(last),
            _ => break,
        }
    }
    matching_box(&records, plugins, picture_us, label, reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_analytics_protocol::messages::Style;

    fn boxed(label: Option<&str>, rect: [f32; 4]) -> OverlayItem {
        OverlayItem::Box {
            rect,
            label: label.map(str::to_owned),
            style: Style::default(),
            fill: false,
        }
    }

    fn drawing(plugin: &str, time_us: i64, items: Vec<OverlayItem>) -> DrawingRecord {
        DrawingRecord {
            id: time_us,
            camera_id: "cam".into(),
            plugin: plugin.into(),
            layer: "boxes".into(),
            frame: None,
            time_us,
            ttl_ms: 1000,
            on_wall: true,
            items,
        }
    }

    fn plugins() -> Vec<String> {
        vec!["detect".to_owned()]
    }

    #[test]
    fn the_drawing_nearest_the_picture_gives_the_box() {
        let records = [
            drawing(
                "detect",
                1_000_000,
                vec![boxed(Some("car"), [0.1, 0.1, 0.2, 0.2])],
            ),
            drawing(
                "detect",
                2_100_000,
                vec![boxed(Some("car"), [0.5, 0.5, 0.2, 0.2])],
            ),
            drawing(
                "detect",
                2_400_000,
                vec![boxed(Some("car"), [0.7, 0.7, 0.1, 0.1])],
            ),
        ];
        let found = matching_box(&records, &plugins(), 2_150_000, Some("car"), None);
        assert_eq!(found, Some([0.5, 0.5, 0.2, 0.2]));
        let found = matching_box(&records, &plugins(), 2_390_000, Some("car"), None);
        assert_eq!(found, Some([0.7, 0.7, 0.1, 0.1]));
    }

    #[test]
    fn a_drawing_beyond_the_tolerance_or_of_another_plugin_is_not_used() {
        let records = [
            drawing(
                "detect",
                1_000_000,
                vec![boxed(Some("car"), [0.1, 0.1, 0.2, 0.2])],
            ),
            drawing("motion", 3_000_000, vec![boxed(None, [0.3, 0.3, 0.2, 0.2])]),
        ];
        assert_eq!(
            matching_box(&records, &plugins(), 3_000_000, Some("car"), None),
            None
        );
        assert_eq!(matching_box(&[], &plugins(), 3_000_000, None, None), None);
        assert_eq!(matching_box(&records, &[], 1_000_000, None, None), None);
    }

    #[test]
    fn the_box_is_the_events_class_and_the_nearest_to_its_start_box() {
        let records = [drawing(
            "detect",
            5_000_000,
            vec![
                boxed(Some("person 91%"), [0.40, 0.40, 0.1, 0.2]),
                boxed(Some("Car 80%"), [0.10, 0.50, 0.2, 0.2]),
                boxed(Some("car 95%"), [0.60, 0.50, 0.2, 0.2]),
                OverlayItem::Text {
                    anchor: Default::default(),
                    lines: vec!["x".into()],
                    style: Style::default(),
                },
            ],
        )];
        let reference = Some([0.58, 0.5, 0.2, 0.2]);
        let found = matching_box(&records, &plugins(), 5_000_000, Some("car"), reference);
        assert_eq!(found, Some([0.60, 0.50, 0.2, 0.2]));
        // A class with no box falls back to the nearest box of any label.
        let found = matching_box(&records, &plugins(), 5_000_000, Some("truck"), reference);
        assert_eq!(found, Some([0.60, 0.50, 0.2, 0.2]));
        let found = matching_box(
            &records,
            &plugins(),
            5_000_000,
            None,
            Some([0.41, 0.4, 0.1, 0.2]),
        );
        assert_eq!(found, Some([0.40, 0.40, 0.1, 0.2]));
    }

    #[test]
    fn unusable_boxes_are_ignored() {
        let records = [drawing(
            "detect",
            1,
            vec![
                boxed(Some("car"), [0.9, 0.9, 0.5, 0.5]),
                boxed(Some("car"), [0.1, 0.1, 0.0, 0.2]),
            ],
        )];
        assert_eq!(
            matching_box(&records, &plugins(), 1, Some("car"), None),
            None
        );
    }

    #[test]
    fn lookup_reads_the_history_including_pending_drawings() {
        let db = std::sync::Arc::new(std::sync::Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        ));
        let history = History::new(db).unwrap();
        assert!(history.offer(drawing(
            "detect",
            9_000_000,
            vec![boxed(Some("car"), [0.2, 0.2, 0.2, 0.2])]
        )));
        let found = lookup(&history, "cam", &plugins(), 9_200_000, Some("car"), None);
        assert_eq!(found, Some([0.2, 0.2, 0.2, 0.2]));
        assert_eq!(
            lookup(&history, "other", &plugins(), 9_200_000, None, None),
            None
        );
    }
}
