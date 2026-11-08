//! The plugins' latest drawings, per camera and layer, for whoever shows the
//! camera (`plugins.overlays`). Each `overlay` from a plugin replaces what
//! that layer showed; a drawing lapses after its time to live, so a plugin
//! that stops sending leaves nothing stale on the picture.
use nw_analytics_protocol::messages::OverlayItem;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const MAX_ITEMS: usize = 256;
const MAX_POINTS: usize = 64;
const MAX_LINES: usize = 8;
const MAX_TEXT: usize = 64;
const DEFAULT_TTL: Duration = Duration::from_secs(2);
const MAX_TTL: Duration = Duration::from_secs(10);
const MIN_TTL: Duration = Duration::from_millis(100);

/// One layer's drawing, as clients receive it.
#[derive(Debug, Clone, Serialize)]
pub struct Shown {
    pub camera_id: String,
    /// The plugin, by its id in config `plugins.servers`.
    pub plugin: String,
    pub layer: String,
    pub items: Vec<OverlayItem>,
    /// The frame it describes, when the plugin said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<u64>,
    /// Capture time resolved by the node, never supplied by the plugin.
    pub time_us: i64,
    /// Node observation clock for live Main/Sub alignment; absent on legacy frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_us: Option<i64>,
    /// How long it stays shown from now, unless replaced.
    pub ttl_ms: u64,
    /// The lifetime the plugin declared, counted from its picture: a client
    /// shows it on pictures up to this much later. Not `ttl_ms`, which
    /// shrinks in every later snapshot while the picture it describes does
    /// not get younger (matched against it, a layer re-sent unchanged went
    /// blank half way through its life: blinking live drawings).
    pub lifetime_ms: u64,
    /// The binding chooses this layer for ordinary live views. Sent with
    /// drawings so browser viewers need no access to private plugin settings.
    pub on_wall: bool,
}

struct Entry {
    items: Vec<OverlayItem>,
    frame: Option<u64>,
    time_us: i64,
    observed_us: Option<i64>,
    lifetime_ms: u64,
    until: Instant,
}

type Key = (String, String, String); // camera, plugin, layer

/// A plugin's figures end this long after its last report (it reports every
/// few seconds; a stopped one is also cleared with its session).
const FIGURES_LAST: Duration = Duration::from_secs(10);

/// One of a plugin's figures on a camera, as live viewers get it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShownFigure {
    pub plugin: String,
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<nw_analytics_protocol::messages::Style>,
}

pub struct OverlayStore {
    entries: Mutex<HashMap<Key, Entry>>,
    wall_layers: Mutex<HashSet<Key>>,
    /// Camera, plugin → its latest figures and when they end.
    figures:
        Mutex<HashMap<(String, String), (Vec<nw_analytics_protocol::messages::Figure>, Instant)>>,
    seq: watch::Sender<u64>,
    pub history: Option<Arc<super::history::History>>,
}

impl Default for OverlayStore {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            wall_layers: Mutex::default(),
            figures: Mutex::default(),
            seq: watch::channel(0).0,
            history: None,
        }
    }
}

impl OverlayStore {
    pub fn with_history(history: Arc<super::history::History>) -> Self {
        Self {
            history: Some(history),
            ..Self::default()
        }
    }

    /// Reconciled from enabled bindings and manifest defaults by the host.
    pub fn set_wall_layers(&self, next: HashSet<(String, String, String)>) {
        let changed = self.wall_layers.lock().is_ok_and(|mut current| {
            if *current == next {
                return false;
            }
            *current = next;
            true
        });
        if changed {
            self.seq.send_modify(|seq| *seq += 1);
        }
    }

    /// What `layer` of `plugin` shows on `camera_id` now (empty: nothing).
    pub fn put(
        &self,
        camera_id: &str,
        plugin: &str,
        layer: &str,
        items: Vec<OverlayItem>,
        frame: Option<u64>,
        ttl_ms: Option<u32>,
    ) {
        self.put_identified(
            camera_id,
            plugin,
            layer,
            items,
            frame.map(|seq| crate::streams::frames::Identity {
                seq,
                observed_us: 0,
                time_us: crate::events::now_ms() * 1000,
            }),
            ttl_ms,
        );
    }

    pub fn put_identified(
        &self,
        camera_id: &str,
        plugin: &str,
        layer: &str,
        items: Vec<OverlayItem>,
        frame: Option<crate::streams::frames::Identity>,
        ttl_ms: Option<u32>,
    ) {
        let time_us = frame.map_or_else(|| crate::events::now_ms() * 1000, |frame| frame.time_us);
        let observed_us = frame
            .map(|frame| frame.observed_us)
            .filter(|time| *time > 0);
        let frame = frame.map(|frame| frame.seq);
        let key = (camera_id.to_owned(), plugin.to_owned(), layer.to_owned());
        let ttl = ttl_ms.map_or(DEFAULT_TTL, |ms| {
            Duration::from_millis(u64::from(ms)).clamp(MIN_TTL, MAX_TTL)
        });
        let record = self
            .history
            .as_ref()
            .map(|_| super::history::DrawingRecord {
                id: 0,
                camera_id: camera_id.into(),
                plugin: plugin.into(),
                layer: layer.into(),
                frame,
                time_us,
                ttl_ms: ttl.as_millis() as u64,
                on_wall: self
                    .wall_layers
                    .lock()
                    .is_ok_and(|chosen| chosen.contains(&key)),
                items: items.clone(),
            });
        if let Ok(mut entries) = self.entries.lock() {
            if items.is_empty() {
                if entries.remove(&key).is_none() {
                    return; // nothing shown, nothing changes
                }
            } else {
                entries.insert(
                    key,
                    Entry {
                        items,
                        frame,
                        time_us,
                        observed_us,
                        lifetime_ms: ttl.as_millis() as u64,
                        until: Instant::now() + ttl,
                    },
                );
            }
        }
        self.seq.send_modify(|seq| *seq += 1);
        if let (Some(history), Some(record)) = (&self.history, record) {
            if !history.offer(record) {
                tracing::debug!(plugin, camera_id, "Drawing history queue is full");
            }
        }
    }

    /// `plugin`'s figures for `camera_id`, for live viewers (none: it has none).
    pub fn put_figures(
        &self,
        camera_id: &str,
        plugin: &str,
        figures: Vec<nw_analytics_protocol::messages::Figure>,
    ) {
        let key = (camera_id.to_owned(), plugin.to_owned());
        let until = Instant::now() + FIGURES_LAST;
        let changed = self.figures.lock().is_ok_and(|mut all| {
            let changed = all
                .get(&key)
                .map_or(!figures.is_empty(), |(old, _)| *old != figures);
            if figures.is_empty() {
                all.remove(&key);
            } else {
                all.insert(key, (figures, until));
            }
            changed
        });
        if changed {
            self.seq.send_modify(|seq| *seq += 1);
        }
    }

    /// The figures `camera_id` has now, by plugin.
    pub fn figures(&self, camera_id: &str) -> Vec<ShownFigure> {
        self.figures_by_camera(Some(&[camera_id.to_owned()]))
            .remove(camera_id)
            .unwrap_or_default()
    }

    /// The figures of `cameras` (all: `None`) now, by camera, each by plugin.
    pub fn figures_by_camera(
        &self,
        cameras: Option<&[String]>,
    ) -> BTreeMap<String, Vec<ShownFigure>> {
        let now = Instant::now();
        let Ok(mut all) = self.figures.lock() else {
            return BTreeMap::new();
        };
        all.retain(|_, (_, until)| *until > now);
        let mut mine: Vec<_> = all
            .iter()
            .filter(|((camera, _), _)| cameras.map_or(true, |ids| ids.contains(camera)))
            .collect();
        mine.sort_by(|a, b| a.0.cmp(b.0));
        let mut by_camera: BTreeMap<String, Vec<ShownFigure>> = BTreeMap::new();
        for ((camera, plugin), (figures, _)) in mine {
            by_camera
                .entry(camera.clone())
                .or_default()
                .extend(figures.iter().map(|figure| ShownFigure {
                    plugin: plugin.clone(),
                    label: figure.label.clone(),
                    value: figure.value.clone(),
                    icon: figure.icon.clone(),
                    style: figure.style,
                }));
        }
        by_camera
    }

    /// Remove `plugin`'s drawings and figures: on one camera, or everywhere.
    pub fn clear(&self, plugin: &str, camera_id: Option<&str>) {
        let figures_removed = self.figures.lock().is_ok_and(|mut all| {
            let before = all.len();
            all.retain(|(camera, owner), _| {
                owner != plugin || camera_id.is_some_and(|id| id != camera)
            });
            all.len() != before
        });
        if figures_removed {
            self.seq.send_modify(|seq| *seq += 1);
        }
        let removed = self
            .entries
            .lock()
            .ok()
            .map(|mut entries| {
                let keys: Vec<Key> = entries
                    .keys()
                    .filter(|(camera, owner, _)| {
                        owner == plugin && camera_id.map_or(true, |id| id == camera)
                    })
                    .cloned()
                    .collect();
                for key in &keys {
                    entries.remove(key);
                }
                keys
            })
            .unwrap_or_default();
        if !removed.is_empty() {
            self.seq.send_modify(|seq| *seq += 1);
        }
        if let Some(history) = &self.history {
            for (camera, plugin, layer) in removed {
                let _ = history.offer(super::history::DrawingRecord {
                    id: 0,
                    camera_id: camera,
                    plugin,
                    layer,
                    frame: None,
                    time_us: crate::events::now_ms() * 1000,
                    ttl_ms: 0,
                    on_wall: false,
                    items: vec![],
                });
            }
        }
    }

    pub fn seq(&self) -> u64 {
        *self.seq.borrow()
    }

    /// Everything shown now, on `cameras` (`None`: every camera).
    pub fn snapshot(&self, cameras: Option<&[String]>) -> Vec<Shown> {
        let now = Instant::now();
        let Ok(mut entries) = self.entries.lock() else {
            return vec![];
        };
        entries.retain(|_, entry| entry.until > now);
        let wall_layers = self.wall_layers.lock().ok();
        let mut shown: Vec<Shown> = entries
            .iter()
            .filter(|((camera, _, _), _)| cameras.map_or(true, |ids| ids.contains(camera)))
            .map(|((camera, plugin, layer), entry)| Shown {
                camera_id: camera.clone(),
                plugin: plugin.clone(),
                layer: layer.clone(),
                items: entry.items.clone(),
                frame: entry.frame,
                time_us: entry.time_us,
                observed_us: entry.observed_us,
                ttl_ms: entry.until.saturating_duration_since(now).as_millis() as u64,
                lifetime_ms: entry.lifetime_ms,
                on_wall: wall_layers.as_ref().is_some_and(|chosen| {
                    chosen.contains(&(camera.clone(), plugin.clone(), layer.clone()))
                }),
            })
            .collect();
        shown.sort_by(|a, b| {
            (&a.camera_id, &a.plugin, &a.layer).cmp(&(&b.camera_id, &b.plugin, &b.layer))
        });
        shown
    }

    /// Wait until something changed after `after`, or `timeout`; the
    /// current sequence number.
    pub async fn wait(&self, after: u64, timeout: Duration) -> u64 {
        let mut changes = self.seq.subscribe();
        let _ = tokio::time::timeout(timeout, changes.wait_for(|seq| *seq > after)).await;
        self.seq()
    }
}

/// A plugin's drawing made safe to show: coordinates inside the picture,
/// bounded counts and text. Items that cannot be made sense of are dropped.
pub fn sanitize(items: Vec<OverlayItem>) -> Vec<OverlayItem> {
    items
        .into_iter()
        .take(MAX_ITEMS)
        .filter_map(|item| match item {
            OverlayItem::Box {
                rect,
                label,
                style,
                fill,
            } => {
                let [x, y, w, h] = rect.map(unit);
                let rect = [x, y, w.min(1.0 - x), h.min(1.0 - y)];
                (rect[2] > 0.0 && rect[3] > 0.0).then(|| OverlayItem::Box {
                    rect,
                    label: label.map(clip),
                    style,
                    fill,
                })
            }
            OverlayItem::Polygon {
                points,
                label,
                style,
                fill,
            } => {
                let points = points_of(points);
                (points.len() >= 3).then(|| OverlayItem::Polygon {
                    points,
                    label: label.map(clip),
                    style,
                    fill,
                })
            }
            OverlayItem::Polyline { points, style } => {
                let points = points_of(points);
                (points.len() >= 2).then_some(OverlayItem::Polyline { points, style })
            }
            OverlayItem::Text {
                anchor,
                lines,
                style,
            } => {
                let lines: Vec<String> = lines.into_iter().take(MAX_LINES).map(clip).collect();
                (!lines.is_empty()).then_some(OverlayItem::Text {
                    anchor,
                    lines,
                    style,
                })
            }
        })
        .collect()
}

fn unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn points_of(points: Vec<[f32; 2]>) -> Vec<[f32; 2]> {
    points
        .into_iter()
        .take(MAX_POINTS)
        .map(|point| point.map(unit))
        .collect()
}

fn clip(mut text: String) -> String {
    if text.chars().count() > MAX_TEXT {
        text = text.chars().take(MAX_TEXT).collect();
    }
    text
}

#[cfg(test)]
mod tests {
    #[test]
    fn figures_are_per_camera_replaced_whole_and_cleared_with_their_plugin() {
        use nw_analytics_protocol::messages::Figure;
        let figure = |label: &str, value: &str| Figure {
            label: label.into(),
            value: value.into(),
            icon: None,
            style: None,
        };
        let store = OverlayStore::default();
        let before = store.seq();
        store.put_figures("c1", "vcount", vec![figure("Total today", "4")]);
        assert!(store.seq() > before, "viewers are told");
        store.put_figures("c2", "vcount", vec![figure("Total today", "9")]);
        store.put_figures("c1", "other", vec![figure("Rate", "1")]);
        let shown = store.figures("c1");
        assert_eq!(
            shown.iter().map(|f| f.plugin.as_str()).collect::<Vec<_>>(),
            ["other", "vcount"]
        );
        assert_eq!(shown[1].value, "4");

        let unchanged = store.seq();
        store.put_figures("c1", "vcount", vec![figure("Total today", "4")]);
        assert_eq!(store.seq(), unchanged, "the same figures tell nobody");
        store.put_figures("c1", "vcount", vec![figure("Total today", "5")]);
        assert_eq!(store.figures("c1")[1].value, "5");
        assert!(store.seq() > unchanged);

        store.clear("vcount", Some("c1"));
        assert_eq!(store.figures("c1").len(), 1, "another plugin's stay");
        assert_eq!(store.figures("c2").len(), 1, "another camera's stay");
        store.clear("vcount", None);
        assert!(store.figures("c2").is_empty());
        store.put_figures("c1", "other", vec![]);
        assert!(store.figures("c1").is_empty());
    }

    use super::*;
    use nw_analytics_protocol::messages::{Anchor, Style};

    fn boxed(x: f32) -> OverlayItem {
        OverlayItem::Box {
            rect: [x, 0.1, 0.2, 0.2],
            label: None,
            style: Style::Accent,
            fill: false,
        }
    }

    #[test]
    fn drawings_replace_clear_and_lapse() {
        let store = OverlayStore::default();
        store.put(
            "c1",
            "motion",
            "detections",
            vec![boxed(0.1)],
            Some(7),
            None,
        );
        store.put(
            "c1",
            "motion",
            "detections",
            vec![boxed(0.5)],
            Some(8),
            None,
        );
        store.put("c2", "motion", "debug", vec![boxed(0.1)], None, Some(100));
        assert_eq!(store.seq(), 3);
        let shown = store.snapshot(Some(&["c1".to_owned()]));
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].frame, Some(8));
        assert!(shown[0].ttl_ms > 1500);
        assert_eq!(
            shown[0].lifetime_ms, 2000,
            "the declared lifetime, not the time left"
        );
        assert!(matches!(shown[0].items[0], OverlayItem::Box { rect, .. } if rect[0] == 0.5));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(store.snapshot(None).len(), 1, "c2's drawing lapsed");
        store.put("c1", "motion", "detections", vec![], None, None);
        assert!(store.snapshot(None).is_empty());
        store.put("c1", "motion", "detections", vec![], None, None);
        assert_eq!(store.seq(), 4, "clearing nothing changes nothing");
        store.put("c1", "motion", "a", vec![boxed(0.1)], None, None);
        store.put("c2", "motion", "a", vec![boxed(0.1)], None, None);
        store.clear("motion", Some("c1"));
        assert_eq!(store.snapshot(None)[0].camera_id, "c2");
        store.clear("motion", None);
        assert!(store.snapshot(None).is_empty());
    }

    #[test]
    fn live_alignment_preserves_both_node_clocks_and_normalized_geometry() {
        let store = OverlayStore::default();
        store.put_identified(
            "c1",
            "p",
            "boxes",
            vec![boxed(0.25)],
            Some(crate::streams::frames::Identity {
                seq: 9,
                time_us: 90_000_000,
                observed_us: 10_000_000,
            }),
            None,
        );
        let shown = store.snapshot(None);
        assert_eq!(shown[0].frame, Some(9));
        assert_eq!(
            shown[0].time_us, 90_000_000,
            "recording time is not rewritten"
        );
        assert_eq!(shown[0].observed_us, Some(10_000_000));
        assert!(
            matches!(shown[0].items[0], OverlayItem::Box { rect, .. } if rect == [0.25, 0.1, 0.2, 0.2])
        );
        let wire = serde_json::to_value(&shown[0]).unwrap();
        assert_eq!(wire["observed_us"], 10_000_000);
    }

    #[test]
    fn live_drawings_include_wall_choices_without_plugin_settings() {
        let store = OverlayStore::default();
        store.put("c1", "motion", "detections", vec![boxed(0.1)], None, None);
        store.put("c1", "motion", "debug", vec![boxed(0.1)], None, None);
        store.set_wall_layers([("c1".into(), "motion".into(), "detections".into())].into());
        let shown = store.snapshot(None);
        assert!(!shown[0].on_wall, "debug is not chosen");
        assert!(shown[1].on_wall, "detections are chosen");
        let seq = store.seq();
        store.set_wall_layers([("c1".into(), "motion".into(), "detections".into())].into());
        assert_eq!(
            store.seq(),
            seq,
            "unchanged preferences do not publish again"
        );
        store.set_wall_layers(Default::default());
        assert!(store.seq() > seq);
        assert!(store.snapshot(None).iter().all(|d| !d.on_wall));
    }

    #[tokio::test]
    async fn waiting_returns_on_a_change_or_the_timeout() {
        let store = std::sync::Arc::new(OverlayStore::default());
        assert_eq!(store.wait(0, Duration::from_millis(20)).await, 0);
        let writer = std::sync::Arc::clone(&store);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer.put("c1", "p", "l", vec![boxed(0.1)], None, None);
        });
        assert_eq!(store.wait(0, Duration::from_secs(5)).await, 1);
    }

    #[test]
    fn plugin_drawings_are_bounded() {
        let mut items: Vec<OverlayItem> = (0..300).map(|_| boxed(0.1)).collect();
        items[0] = OverlayItem::Box {
            rect: [0.9, f32::NAN, 0.5, 2.0],
            label: Some("x".repeat(500)),
            style: Style::Danger,
            fill: true,
        };
        items[1] = OverlayItem::Polygon {
            points: vec![[0.0, 0.0], [1.0, 1.0]],
            label: None,
            style: Style::Muted,
            fill: false,
        };
        items[2] = OverlayItem::Text {
            anchor: Anchor::TopRight,
            lines: (0..20).map(|i| i.to_string()).collect(),
            style: Style::Muted,
        };
        items[3] = OverlayItem::Box {
            rect: [1.0, 0.0, 0.2, 0.2],
            label: None,
            style: Style::Accent,
            fill: false,
        };
        let safe = sanitize(items);
        assert_eq!(
            safe.len(),
            MAX_ITEMS - 2,
            "the 2-point polygon and the empty box are dropped"
        );
        match &safe[0] {
            OverlayItem::Box { rect, label, .. } => {
                assert!((rect[0] - 0.9).abs() < 1e-6 && rect[1] == 0.0);
                assert!((rect[2] - 0.1).abs() < 1e-6 && rect[3] == 1.0);
                assert_eq!(label.as_ref().unwrap().len(), MAX_TEXT);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&safe[1], OverlayItem::Text { lines, .. } if lines.len() == MAX_LINES));
    }
}
