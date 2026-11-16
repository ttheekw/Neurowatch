//! What NeuroWatch publishes over MQTT, without any I/O: topic names, Home
//! Assistant discovery documents and the on/off state of each sensor as
//! events start and end.
//!
//! Topics (`<prefix>` is `mqtt.topic_prefix`, `<camera>` the camera's id):
//! - `<prefix>/status`: `online` / `offline` (retained; the broker sends
//!   `offline` itself if the recorder disappears).
//! - `<prefix>/<camera>/<sensor>`: `ON` / `OFF` (retained), sensors
//!   `motion`, `person`, `vehicle`, `animal`, `online`.
//! - `<prefix>/<camera>/picture`: the latest event's JPEG (retained).
//! - `<prefix>/problem`: `ON` while the recorder has a problem (retained).
//! - `<prefix>/events`: each event as JSON when it starts and when it ends.

use std::collections::{BTreeMap, BTreeSet};

use nw_core::event::{EventKind, EventPhase, EventRecord};
use serde_json::{json, Value};

pub const ON: &str = "ON";
pub const OFF: &str = "OFF";
pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";

/// A camera's binary sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Sensor {
    Motion,
    Person,
    Vehicle,
    Animal,
    /// The camera is streaming (ON) or offline (OFF).
    Online,
}

impl Sensor {
    pub const ALL: [Sensor; 5] = [
        Sensor::Motion,
        Sensor::Person,
        Sensor::Vehicle,
        Sensor::Animal,
        Sensor::Online,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Sensor::Motion => "motion",
            Sensor::Person => "person",
            Sensor::Vehicle => "vehicle",
            Sensor::Animal => "animal",
            Sensor::Online => "online",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Sensor::Motion => "Motion",
            Sensor::Person => "Person",
            Sensor::Vehicle => "Vehicle",
            Sensor::Animal => "Animal",
            Sensor::Online => "Online",
        }
    }

    /// Home Assistant's device class, and an icon where it has none.
    fn class(self) -> (Option<&'static str>, Option<&'static str>) {
        match self {
            Sensor::Motion => (Some("motion"), None),
            Sensor::Person => (Some("occupancy"), Some("mdi:account")),
            Sensor::Vehicle => (None, Some("mdi:car")),
            Sensor::Animal => (None, Some("mdi:paw")),
            Sensor::Online => (Some("connectivity"), None),
        }
    }

    /// Announced for every camera; the others once the camera has reported them.
    pub fn always(self) -> bool {
        matches!(self, Sensor::Motion | Sensor::Online)
    }

    /// The sensor an event kind turns on (`Online` is turned off by an
    /// offline camera).
    pub fn of(kind: EventKind) -> Option<Sensor> {
        Some(match kind {
            EventKind::Motion | EventKind::Intrusion | EventKind::LineCrossing => Sensor::Motion,
            EventKind::Person => Sensor::Person,
            EventKind::Vehicle => Sensor::Vehicle,
            EventKind::Animal => Sensor::Animal,
            EventKind::CameraOffline => Sensor::Online,
            _ => return None,
        })
    }

    pub fn from_kind_name(name: &str) -> Option<Sensor> {
        EventKind::parse(name).and_then(Sensor::of)
    }
}

/// Kinds that make the recorder's `problem` sensor ON.
pub fn is_problem(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::RecordingFailed
            | EventKind::DiskLow
            | EventKind::StorageMissing
            | EventKind::PluginOffline
            | EventKind::BackupFailed
            | EventKind::PluginLagging
    )
}

/// Letters, digits, `_` and `-` only: safe in a topic level and an id.
pub fn slug(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "_".into()
    } else {
        slug
    }
}

/// Topic names and discovery documents for one broker configuration.
#[derive(Debug, Clone)]
pub struct Topics {
    prefix: String,
    discovery_prefix: String,
    /// The recorder's id in Home Assistant (stable for a prefix).
    node: String,
}

impl Topics {
    pub fn new(prefix: &str, discovery_prefix: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            discovery_prefix: discovery_prefix.to_owned(),
            node: format!("neurowatch_{}", slug(prefix)),
        }
    }

    pub fn availability(&self) -> String {
        format!("{}/status", self.prefix)
    }
    pub fn events(&self) -> String {
        format!("{}/events", self.prefix)
    }
    pub fn problem(&self) -> String {
        format!("{}/problem", self.prefix)
    }
    pub fn state(&self, camera: &str, sensor: Sensor) -> String {
        format!("{}/{}/{}", self.prefix, slug(camera), sensor.id())
    }
    pub fn picture(&self, camera: &str) -> String {
        format!("{}/{}/picture", self.prefix, slug(camera))
    }

    fn camera_device(&self, camera: &str, name: &str) -> Value {
        json!({
            "identifiers": [format!("{}_{}", self.node, slug(camera))],
            "name": name,
            "manufacturer": "NeuroWatch",
            "model": "Camera",
            "via_device": self.node,
        })
    }

    fn availability_fields(&self) -> Value {
        json!({
            "availability_topic": self.availability(),
            "payload_available": ONLINE,
            "payload_not_available": OFFLINE,
        })
    }

    fn config_topic(&self, component: &str, unique_id: &str) -> String {
        format!("{}/{component}/{unique_id}/config", self.discovery_prefix)
    }

    /// A camera sensor's discovery topic and document.
    pub fn sensor_config(&self, camera: &str, name: &str, sensor: Sensor) -> (String, Value) {
        let unique_id = format!("{}_{}_{}", self.node, slug(camera), sensor.id());
        let (class, icon) = sensor.class();
        let mut document = json!({
            "name": sensor.name(),
            "unique_id": unique_id,
            "state_topic": self.state(camera, sensor),
            "payload_on": ON,
            "payload_off": OFF,
            "device": self.camera_device(camera, name),
        });
        merge(&mut document, self.availability_fields());
        if let Some(class) = class {
            document["device_class"] = json!(class);
        }
        if let Some(icon) = icon {
            document["icon"] = json!(icon);
        }
        if sensor == Sensor::Online {
            document["entity_category"] = json!("diagnostic");
        }
        (self.config_topic("binary_sensor", &unique_id), document)
    }

    /// A camera's latest event picture, as a Home Assistant image entity.
    pub fn picture_config(&self, camera: &str, name: &str) -> (String, Value) {
        let unique_id = format!("{}_{}_picture", self.node, slug(camera));
        let mut document = json!({
            "name": "Last event",
            "unique_id": unique_id,
            "image_topic": self.picture(camera),
            "content_type": "image/jpeg",
            "device": self.camera_device(camera, name),
        });
        merge(&mut document, self.availability_fields());
        (self.config_topic("image", &unique_id), document)
    }

    /// The recorder itself: a `problem` sensor (recording failed, drive
    /// almost full or missing, a plugin or Cloud Backup failing).
    pub fn recorder_config(&self, site: &str, version: &str) -> (String, Value) {
        let unique_id = format!("{}_problem", self.node);
        let mut document = json!({
            "name": "Problem",
            "unique_id": unique_id,
            "state_topic": self.problem(),
            "payload_on": ON,
            "payload_off": OFF,
            "device_class": "problem",
            "device": {
                "identifiers": [self.node],
                "name": if site.trim().is_empty() { "NeuroWatch" } else { site },
                "manufacturer": "NeuroWatch",
                "model": "Recorder",
                "sw_version": version,
            },
        });
        merge(&mut document, self.availability_fields());
        (self.config_topic("binary_sensor", &unique_id), document)
    }

    /// The discovery topics of a camera, to remove it from Home Assistant.
    pub fn camera_config_topics(&self, camera: &str) -> Vec<String> {
        let mut topics: Vec<String> = Sensor::ALL
            .into_iter()
            .map(|sensor| {
                self.config_topic(
                    "binary_sensor",
                    &format!("{}_{}_{}", self.node, slug(camera), sensor.id()),
                )
            })
            .collect();
        topics.push(self.config_topic("image", &format!("{}_{}_picture", self.node, slug(camera))));
        topics
    }
}

fn merge(into: &mut Value, from: Value) {
    if let (Some(into), Value::Object(from)) = (into.as_object_mut(), from) {
        into.extend(from);
    }
}

/// An event as `<prefix>/events` carries it.
pub fn event_message(event: &EventRecord, phase: EventPhase, camera_name: Option<&str>) -> Value {
    json!({
        "phase": match phase { EventPhase::Start => "start", EventPhase::Update => "update", EventPhase::End => "end" },
        "id": event.id,
        "kind": event.kind.as_str(),
        "camera_id": event.camera_id,
        "camera_name": camera_name,
        "source": event.source.as_str(),
        "topic": event.topic,
        "start_ms": event.start_ms,
        "end_ms": event.end_ms,
        "score": event.score,
        "zones": event.zones,
        "detail": event.detail,
    })
}

/// Which events hold each sensor on, so overlapping events (the camera's own
/// motion and a plugin's) keep it on until the last one ends.
#[derive(Debug, Default)]
pub struct Sensors {
    open: BTreeMap<(String, Sensor), BTreeSet<i64>>,
    problems: BTreeSet<i64>,
}

/// A retained state to publish: topic and payload.
pub type Change = (String, &'static str);

impl Sensors {
    /// Rebuild from the events open now (after a connection).
    pub fn from_open(events: &[EventRecord]) -> Self {
        let mut sensors = Self::default();
        for event in events {
            sensors.apply(event, EventPhase::Start);
        }
        sensors
    }

    /// Note an event; the sensors whose state changed, as `(camera, sensor, on)`
    /// or the recorder's problem as `None`.
    pub fn apply(
        &mut self,
        event: &EventRecord,
        phase: EventPhase,
    ) -> Vec<(Option<(String, Sensor)>, bool)> {
        let open = phase != EventPhase::End;
        let mut changed = Vec::new();
        if is_problem(event.kind) {
            let before = !self.problems.is_empty();
            if open {
                self.problems.insert(event.id);
            } else {
                self.problems.remove(&event.id);
            }
            if before == self.problems.is_empty() {
                changed.push((None, !before));
            }
        }
        let (Some(camera), Some(sensor)) = (event.camera_id.as_deref(), Sensor::of(event.kind))
        else {
            return changed;
        };
        let ids = self.open.entry((camera.to_owned(), sensor)).or_default();
        let before = !ids.is_empty();
        if open {
            ids.insert(event.id);
        } else {
            ids.remove(&event.id);
        }
        let after = !ids.is_empty();
        if before != after {
            changed.push((Some((camera.to_owned(), sensor)), after));
        }
        changed
    }

    /// Whether a sensor is ON. `Online` is ON unless an offline event is open.
    pub fn is_on(&self, camera: &str, sensor: Sensor) -> bool {
        let open = self
            .open
            .get(&(camera.to_owned(), sensor))
            .is_some_and(|ids| !ids.is_empty());
        if sensor == Sensor::Online {
            !open
        } else {
            open
        }
    }

    pub fn problem(&self) -> bool {
        !self.problems.is_empty()
    }
}

/// The payload of a sensor's state.
pub fn payload(on: bool) -> &'static str {
    if on {
        ON
    } else {
        OFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::event::EventSource;

    fn event(id: i64, kind: EventKind, camera: Option<&str>) -> EventRecord {
        EventRecord {
            id,
            camera_id: camera.map(Into::into),
            kind,
            source: EventSource::Plugin,
            start_ms: 1000,
            end_ms: None,
            topic: Some("nw.detect".into()),
            score: Some(0.8),
            zones: vec![],
            bbox: None,
            thumbnail: None,
            detail: None,
            acknowledged: false,
            kept: false,
        }
    }

    #[test]
    fn topics_are_safe_and_stable() {
        let topics = Topics::new("site-a/neurowatch", "homeassistant");
        assert_eq!(topics.availability(), "site-a/neurowatch/status");
        assert_eq!(
            topics.state("Front Door#1", Sensor::Person),
            "site-a/neurowatch/front_door_1/person"
        );
        let (topic, document) = topics.sensor_config("cam-1", "Front door", Sensor::Person);
        assert_eq!(
            topic,
            "homeassistant/binary_sensor/neurowatch_site-a_neurowatch_cam-1_person/config"
        );
        assert_eq!(document["state_topic"], "site-a/neurowatch/cam-1/person");
        assert_eq!(document["device_class"], "occupancy");
        assert_eq!(document["device"]["name"], "Front door");
        assert_eq!(
            document["device"]["via_device"],
            "neurowatch_site-a_neurowatch"
        );
        assert_eq!(document["availability_topic"], "site-a/neurowatch/status");
        let (_, picture) = topics.picture_config("cam-1", "Front door");
        assert_eq!(picture["image_topic"], "site-a/neurowatch/cam-1/picture");
        assert_eq!(topics.camera_config_topics("cam-1").len(), 6);
    }

    #[test]
    fn a_sensor_stays_on_until_the_last_of_its_events_ends() {
        let mut sensors = Sensors::default();
        let motion = event(1, EventKind::Motion, Some("cam"));
        let crossing = event(2, EventKind::LineCrossing, Some("cam"));
        let on = Some(("cam".to_owned(), Sensor::Motion));
        assert_eq!(
            sensors.apply(&motion, EventPhase::Start),
            vec![(on.clone(), true)]
        );
        assert!(sensors.apply(&crossing, EventPhase::Start).is_empty());
        assert!(sensors.apply(&motion, EventPhase::End).is_empty());
        assert!(sensors.is_on("cam", Sensor::Motion));
        assert_eq!(sensors.apply(&crossing, EventPhase::End), vec![(on, false)]);
        assert!(!sensors.is_on("cam", Sensor::Motion));
    }

    #[test]
    fn an_offline_camera_turns_online_off_and_problems_reach_the_recorder() {
        let offline = event(3, EventKind::CameraOffline, Some("cam"));
        let sensors = Sensors::from_open(&[offline]);
        assert!(!sensors.is_on("cam", Sensor::Online));
        assert!(sensors.is_on("other", Sensor::Online));
        let mut sensors = Sensors::default();
        let disk = event(4, EventKind::DiskLow, None);
        assert_eq!(sensors.apply(&disk, EventPhase::Start), vec![(None, true)]);
        assert!(sensors.problem());
        assert_eq!(sensors.apply(&disk, EventPhase::End), vec![(None, false)]);
    }

    #[test]
    fn events_are_published_with_their_camera_and_phase() {
        let message = event_message(
            &event(5, EventKind::Person, Some("cam")),
            EventPhase::Start,
            Some("Gate"),
        );
        assert_eq!(message["kind"], "person");
        assert_eq!(message["phase"], "start");
        assert_eq!(message["camera_name"], "Gate");
        assert_eq!(message["topic"], "nw.detect");
    }
}
