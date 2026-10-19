//! Turning a camera's ONVIF notifications into events.
//!
//! Most events are properties: a state (`IsMotion`, `State`, `IsInside`,
//! `LogicalState`) that turns true and later false. Many cameras report
//! the same motion under two topics (`RuleEngine/CellMotionDetector/Motion`
//! and `VideoSource/MotionAlarm`), so all motion of a camera is one event,
//! open while any of its motion states is true. Notifications without a
//! state (a line crossed) are events without duration.

use std::collections::{BTreeSet, HashMap};

use nw_core::event::EventKind;

use super::parse::{Notification, Operation};

/// What the camera's topic means, or `None` for topics that are not events
/// worth keeping (processor load, configuration changes, …).
pub fn classify(topic: &str) -> Option<EventKind> {
    let has = |part: &str| topic.contains(part);
    if has("CellMotionDetector")
        || has("MotionAlarm")
        || has("MotionDetector")
        || has("MotionRegionDetector")
    {
        return Some(EventKind::Motion);
    }
    if has("FieldDetector") || has("Intrusion") || has("RegionEntrance") {
        return Some(EventKind::Intrusion);
    }
    if has("LineDetector") || has("LineCrossing") || has("Crossed") {
        return Some(EventKind::LineCrossing);
    }
    if has("Tamper") || has("GlobalSceneChange") || has("ImageToo") {
        return Some(EventKind::Tamper);
    }
    if has("DigitalInput") {
        return Some(EventKind::DigitalInput);
    }
    const EVENT_AREAS: &[&str] = &[
        "RuleEngine/",
        "VideoAnalytics/",
        "VideoSource/",
        "AudioAnalytics/",
        "AudioSource/",
        "Device/Trigger/",
        "Device/IO/",
    ];
    EVENT_AREAS
        .iter()
        .any(|area| topic.starts_with(area))
        .then_some(EventKind::CameraEvent)
}

/// Data items that carry an event's state, in order of preference.
const STATE_ITEMS: &[&str] = &[
    "IsMotion",
    "State",
    "IsInside",
    "IsTamper",
    "LogicalState",
    "Active",
    "IsActive",
];

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "active" | "on" => Some(true),
        "false" | "0" | "inactive" | "off" => Some(false),
        _ => None,
    }
}

/// The state a notification reports, if it is a property with one.
fn state(notification: &Notification) -> Option<bool> {
    if notification.operation == Operation::None {
        return None;
    }
    let named = STATE_ITEMS.iter().find_map(|name| {
        notification
            .data
            .iter()
            .find(|(item, _)| item == name)
            .and_then(|(_, value)| parse_bool(value))
    });
    named.or_else(|| {
        // A single true/false item under another name.
        let mut booleans = notification.data.iter().filter_map(|(_, value)| {
            matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "false")
                .then(|| parse_bool(value))
                .flatten()
        });
        let first = booleans.next();
        booleans.next().is_none().then_some(first).flatten()
    })
}

/// The rule or input that fired, when the camera names it. Other source
/// items (video source tokens) mean nothing to a person.
fn detail(notification: &Notification) -> Option<String> {
    const NAMED: &[&str] = &["Rule", "RuleName", "InputToken", "Name"];
    NAMED.iter().find_map(|name| {
        notification
            .source
            .iter()
            .find(|(item, value)| item == name && !value.is_empty())
            .map(|(_, value)| value.clone())
    })
}

/// What the subscriber should do with the hub.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Start {
        key: String,
        kind: EventKind,
        topic: String,
        detail: Option<String>,
    },
    End {
        key: String,
    },
    Instant {
        kind: EventKind,
        topic: String,
        detail: Option<String>,
    },
}

#[derive(Default)]
pub struct Tracker {
    /// Open event key → the states keeping it open (topic + source).
    active: HashMap<String, BTreeSet<String>>,
    /// Topics seen, for `motion.status` (bounded).
    pub seen: BTreeSet<String>,
}

const MAX_SEEN_TOPICS: usize = 64;

impl Tracker {
    pub fn observe(&mut self, notification: &Notification) -> Vec<Action> {
        let Some(kind) = classify(&notification.topic) else {
            return Vec::new();
        };
        if self.seen.len() < MAX_SEEN_TOPICS {
            self.seen.insert(notification.topic.clone());
        }
        let source = notification
            .source
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let state_key = format!("{}?{source}", notification.topic);
        let key = if kind == EventKind::Motion {
            "motion".to_owned()
        } else {
            state_key.clone()
        };
        let Some(on) = (match notification.operation {
            Operation::Deleted => Some(false),
            _ => state(notification),
        }) else {
            return if notification.operation == Operation::Initialized {
                Vec::new()
            } else {
                vec![Action::Instant {
                    kind,
                    topic: notification.topic.clone(),
                    detail: detail(notification),
                }]
            };
        };
        if on {
            let states = self.active.entry(key.clone()).or_default();
            let opened = states.is_empty();
            states.insert(state_key);
            if opened {
                return vec![Action::Start {
                    key,
                    kind,
                    topic: notification.topic.clone(),
                    detail: detail(notification),
                }];
            }
        } else if let Some(states) = self.active.get_mut(&key) {
            states.remove(&state_key);
            if states.is_empty() {
                self.active.remove(&key);
                return vec![Action::End { key }];
            }
        }
        Vec::new()
    }

    /// Re-aiming invalidates motion analytics, but never closes alarm inputs
    /// or tamper events that happen to be open on the same camera.
    pub fn reset_activity(&mut self) -> Vec<Action> {
        let keys: Vec<_> = self
            .active
            .keys()
            .filter(|key| {
                key.as_str() == "motion"
                    || matches!(
                        classify(key.split('?').next().unwrap_or(key)),
                        Some(EventKind::Intrusion | EventKind::LineCrossing)
                    )
            })
            .cloned()
            .collect();
        keys.into_iter()
            .map(|key| {
                self.active.remove(&key);
                Action::End { key }
            })
            .collect()
    }

    /// The subscription ended: nothing is known to be going on any more.
    pub fn reset(&mut self) -> Vec<Action> {
        self.active
            .drain()
            .map(|(key, _)| Action::End { key })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(topic: &str, operation: Operation, data: &[(&str, &str)]) -> Notification {
        Notification {
            topic: topic.into(),
            operation,
            source: vec![("VideoSourceConfigurationToken".into(), "V1".into())],
            data: data
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    const CELL: &str = "RuleEngine/CellMotionDetector/Motion";
    const ALARM: &str = "VideoSource/MotionAlarm";

    #[test]
    fn topics_are_classified() {
        assert_eq!(classify(CELL), Some(EventKind::Motion));
        assert_eq!(classify(ALARM), Some(EventKind::Motion));
        assert_eq!(
            classify("RuleEngine/FieldDetector/ObjectsInside"),
            Some(EventKind::Intrusion)
        );
        assert_eq!(
            classify("RuleEngine/LineDetector/Crossed"),
            Some(EventKind::LineCrossing)
        );
        assert_eq!(
            classify("RuleEngine/TamperDetector/Tamper"),
            Some(EventKind::Tamper)
        );
        assert_eq!(
            classify("Device/Trigger/DigitalInput"),
            Some(EventKind::DigitalInput)
        );
        assert_eq!(
            classify("VideoSource/SignalLoss"),
            Some(EventKind::CameraEvent)
        );
        assert_eq!(classify("Monitoring/ProcessorUsage"), None);
        assert_eq!(classify("Media/ProfileChanged"), None);
    }

    #[test]
    fn two_motion_topics_make_one_event() {
        let mut t = Tracker::default();
        let starts = t.observe(&note(CELL, Operation::Changed, &[("IsMotion", "true")]));
        assert!(matches!(
            starts[..],
            [Action::Start {
                kind: EventKind::Motion,
                ..
            }]
        ));
        assert!(t
            .observe(&note(ALARM, Operation::Changed, &[("State", "true")]))
            .is_empty());
        assert!(t
            .observe(&note(CELL, Operation::Changed, &[("IsMotion", "false")]))
            .is_empty());
        assert_eq!(
            t.observe(&note(ALARM, Operation::Changed, &[("State", "false")])),
            [Action::End {
                key: "motion".into()
            }]
        );
    }

    #[test]
    fn initialized_false_is_quiet_and_initialized_true_starts() {
        let mut t = Tracker::default();
        assert!(t
            .observe(&note(
                CELL,
                Operation::Initialized,
                &[("IsMotion", "false")]
            ))
            .is_empty());
        assert_eq!(
            t.observe(&note(CELL, Operation::Initialized, &[("IsMotion", "true")]))
                .len(),
            1
        );
        assert_eq!(
            t.reset(),
            [Action::End {
                key: "motion".into()
            }]
        );
        assert!(t.reset().is_empty());
    }

    #[test]
    fn notifications_without_state_are_instant() {
        let mut t = Tracker::default();
        let actions = t.observe(&note(
            "RuleEngine/LineDetector/Crossed",
            Operation::None,
            &[("ObjectId", "3")],
        ));
        assert!(matches!(
            actions[..],
            [Action::Instant {
                kind: EventKind::LineCrossing,
                ..
            }]
        ));
    }

    #[test]
    fn a_digital_input_is_its_own_event_per_input() {
        let mut t = Tracker::default();
        let input = |token: &str, value: &str| Notification {
            topic: "Device/Trigger/DigitalInput".into(),
            operation: Operation::Changed,
            source: vec![("InputToken".into(), token.into())],
            data: vec![("LogicalState".into(), value.into())],
        };
        assert_eq!(t.observe(&input("DI1", "true")).len(), 1);
        assert_eq!(t.observe(&input("DI2", "true")).len(), 1);
        let ended = t.observe(&input("DI1", "false"));
        assert!(matches!(&ended[..], [Action::End { key }] if key.contains("DI1")));
        match &t.observe(&input("DI1", "true"))[..] {
            [Action::Start { detail, .. }] => assert_eq!(detail.as_deref(), Some("DI1")),
            other => panic!("{other:?}"),
        }
    }
}
