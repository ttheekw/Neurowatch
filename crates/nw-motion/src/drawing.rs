//! What the plugin draws over the camera's picture: the moving area while
//! there is motion (`detections`), and what the detector sees (`debug`):
//! the blocks that changed, the zones and the score.
use crate::detector::{Sample, COLUMNS, ROWS};
use crate::settings::{MotionSettings, ZoneKind};
use nw_analytics_protocol::messages::{Anchor, OverlayItem, Style};

/// The moving area, labelled with the score, while motion is reported.
pub fn detections(sample: &Sample) -> Vec<OverlayItem> {
    match sample.bbox {
        Some(rect) if sample.active => vec![OverlayItem::Box {
            rect,
            label: Some(format!("Motion {:.0}%", sample.score * 100.0)),
            style: Style::Warning,
            fill: false,
        }],
        _ => vec![],
    }
}

/// The changed blocks, the zones and a line of numbers.
pub fn debug(sample: &Sample, settings: &MotionSettings, fps: f32) -> Vec<OverlayItem> {
    let (width, height) = (1.0 / COLUMNS as f32, 1.0 / ROWS as f32);
    let mut items: Vec<OverlayItem> = sample
        .blocks
        .iter()
        .map(|&block| {
            let (column, row) = (usize::from(block) % COLUMNS, usize::from(block) / COLUMNS);
            OverlayItem::Box {
                rect: [column as f32 * width, row as f32 * height, width, height],
                label: None,
                style: if sample.active {
                    Style::Warning
                } else {
                    Style::Muted
                },
                fill: true,
            }
        })
        .collect();
    items.extend(settings.zones.iter().filter(|z| z.is_valid()).map(|zone| {
        let watched = zone.kind == ZoneKind::Include;
        OverlayItem::Polygon {
            points: zone.points.clone(),
            label: Some(if zone.name.is_empty() {
                zone.id.clone()
            } else {
                zone.name.clone()
            }),
            style: if watched {
                Style::Accent
            } else {
                Style::Danger
            },
            fill: !watched,
        }
    }));
    let state = if sample.active { "motion" } else { "quiet" };
    items.push(OverlayItem::Text {
        anchor: Anchor::BottomLeft,
        lines: vec![
            format!("Score {:.0}% · {state}", sample.score * 100.0),
            format!("{} blocks changed · {fps:.1} fps", sample.blocks.len()),
        ],
        style: if sample.active {
            Style::Warning
        } else {
            Style::Muted
        },
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Zone;

    fn sample(active: bool) -> Sample {
        Sample {
            active,
            score: 0.25,
            bbox: Some([0.1, 0.2, 0.3, 0.4]),
            zones: vec![],
            blocks: vec![0, (COLUMNS + 1) as u16],
        }
    }

    #[test]
    fn the_moving_area_is_drawn_only_while_motion_is_reported() {
        let drawn = detections(&sample(true));
        assert_eq!(drawn.len(), 1);
        assert!(
            matches!(&drawn[0], OverlayItem::Box { rect, label: Some(label), .. }
            if *rect == [0.1, 0.2, 0.3, 0.4] && label == "Motion 25%")
        );
        assert!(detections(&sample(false)).is_empty());
    }

    #[test]
    fn debug_drawings_show_blocks_zones_and_numbers() {
        let settings = MotionSettings {
            zones: vec![
                Zone {
                    id: "door".into(),
                    name: "Door".into(),
                    kind: ZoneKind::Include,
                    points: vec![[0.0, 0.0], [0.5, 0.0], [0.5, 0.5]],
                },
                Zone {
                    id: "road".into(),
                    name: String::new(),
                    kind: ZoneKind::Exclude,
                    points: vec![[0.5, 0.5], [1.0, 0.5], [1.0, 1.0]],
                },
            ],
            ..MotionSettings::default()
        };
        let drawn = debug(&sample(false), &settings, 5.0);
        assert_eq!(drawn.len(), 5);
        let (w, h) = (1.0 / COLUMNS as f32, 1.0 / ROWS as f32);
        assert!(
            matches!(&drawn[1], OverlayItem::Box { rect, fill: true, style: Style::Muted, .. }
            if (rect[0] - w).abs() < 1e-6 && (rect[1] - h).abs() < 1e-6)
        );
        assert!(
            matches!(&drawn[2], OverlayItem::Polygon { label: Some(l), style: Style::Accent, .. } if l == "Door")
        );
        assert!(
            matches!(&drawn[3], OverlayItem::Polygon { label: Some(l), style: Style::Danger, fill: true, .. } if l == "road")
        );
        assert!(matches!(&drawn[4], OverlayItem::Text { lines, .. }
            if lines[0] == "Score 25% · quiet" && lines[1] == "2 blocks changed · 5.0 fps"));
    }
}
