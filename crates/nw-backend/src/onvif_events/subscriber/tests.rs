use super::*;
use nw_core::event::{EventKind, EventPhase};
use std::sync::Mutex;

fn hub() -> Arc<EventHub> {
    Arc::new(EventHub::new(Arc::new(Mutex::new(
        crate::db::connection::open_in_memory().unwrap(),
    ))))
}

fn session<'a>(cam: &'a CameraConfig, hub: &'a EventHub, events: &'a OnvifEvents) -> Session<'a> {
    Session {
        cam,
        hub,
        events,
        tracker: Tracker::default(),
        open: HashMap::new(),
        push: None,
        push_failed: None,
    }
}

fn notification(topic: &str, active: bool) -> parse::Notification {
    parse::Notification {
        topic: topic.into(),
        operation: parse::Operation::Changed,
        source: vec![("RuleName".into(), "Driveway".into())],
        data: vec![("State".into(), active.to_string())],
    }
}

#[test]
fn received_properties_open_and_close_hub_events_and_update_camera_status() {
    let cam = CameraConfig::new("Camera", "unused");
    let hub = hub();
    let events = OnvifEvents::new();
    let mut session = session(&cam, &hub, &events);
    let motion = "RuleEngine/CellMotionDetector/Motion";
    session.take(&[notification(motion, true)]);

    let open = hub.open_events();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].kind, EventKind::Motion);
    assert_eq!(open[0].source, EventSource::Onvif);
    assert_eq!(open[0].camera_id.as_deref(), Some(cam.id.as_str()));
    assert_eq!(open[0].detail.as_deref(), Some("Driveway"));
    let status = events.status(&cam.id).unwrap();
    assert!(status.last_event_ms.is_some());
    assert!(status.topics.contains(motion));

    session.take(&[notification(motion, true)]);
    assert_eq!(
        hub.open_events().len(),
        1,
        "repeated true must not duplicate motion"
    );
    session.take(&[notification(motion, false)]);
    assert!(hub.open_events().is_empty());
    assert!(session.open.is_empty());
    assert_eq!(hub.since(0).notices.last().unwrap().phase, EventPhase::End);

    let mut crossed = notification("RuleEngine/LineDetector/Crossed", true);
    crossed.operation = parse::Operation::None;
    session.take(&[crossed]);
    assert!(
        hub.open_events().is_empty(),
        "a line crossing is instantaneous"
    );
    assert_eq!(
        hub.since(0).notices.last().unwrap().event.kind,
        EventKind::LineCrossing
    );
}

#[test]
fn ptz_pause_suppresses_activity_but_preserves_tamper_events() {
    let cam = CameraConfig::new("Camera", "unused");
    let hub = hub();
    let events = OnvifEvents::new();
    let mut session = session(&cam, &hub, &events);
    session.take(&[notification("RuleEngine/CellMotionDetector/Motion", true)]);
    hub.pause_activity(&cam.id, Duration::from_secs(10));
    session.take(&[
        notification("RuleEngine/CellMotionDetector/Motion", true),
        notification("RuleEngine/FieldDetector/Intrusion", true),
        notification("RuleEngine/LineDetector/Crossed", true),
        notification("VideoSource/Tamper", true),
    ]);

    let open = hub.open_events();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].kind, EventKind::Tamper);
    session.close_all();
    assert!(hub.open_events().is_empty());
    assert!(session.open.is_empty());

    // Cleanup must close an event even if it is no longer in the tracker.
    let orphan = hub.start(NewEvent::new(
        Some(cam.id.clone()),
        EventKind::Tamper,
        EventSource::Onvif,
    ));
    session.open.insert("orphan".into(), orphan.id);
    session.close_all();
    assert!(hub.open_events().is_empty());
    session.close_all();
    assert!(session.open.is_empty());
}

#[test]
fn delivery_status_keeps_the_push_fallback_reason_and_clears_previous_errors() {
    let cam = CameraConfig::new("Camera", "unused");
    let hub = hub();
    let events = OnvifEvents::new();
    events.update(&cam.id, |status| status.detail = Some("old failure".into()));
    let mut session = session(&cam, &hub, &events);
    session.push_failed = Some("camera cannot reach the listener".into());
    session.subscribed(Delivery::Pull);
    let status = events.status(&cam.id).unwrap();
    assert_eq!(status.state, SubscriptionState::Subscribed);
    assert_eq!(status.delivery, Some(Delivery::Pull));
    assert_eq!(
        status.note.as_deref(),
        Some("camera cannot reach the listener")
    );
    assert_eq!(status.detail, None);
    assert!(subscribe_body("http://127.0.0.1/events")
        .contains("<wsa:Address>http://127.0.0.1/events</wsa:Address>"));
}

#[test]
fn subscription_failures_preserve_the_reason_and_whether_a_session_was_active() {
    for (error, state) in [
        (OnvifError::Unauthorized, SubscriptionState::Unauthorized),
        (OnvifError::Disabled, SubscriptionState::Disabled),
        (
            OnvifError::Unreachable("offline".into()),
            SubscriptionState::Unreachable,
        ),
        (
            OnvifError::Invalid("invalid response".into()),
            SubscriptionState::Error,
        ),
    ] {
        for active in [false, true] {
            match failed(error.clone(), active) {
                Outcome::Failed {
                    state: actual,
                    detail,
                    was_subscribed,
                } => {
                    assert_eq!(actual, state);
                    assert_eq!(detail, error.to_string());
                    assert_eq!(was_subscribed, active);
                }
                Outcome::Stopped => panic!("a failure is not a stop"),
            }
        }
    }
}

#[tokio::test]
async fn a_bad_camera_address_reports_failure_and_can_stop_during_backoff() {
    let cam = CameraConfig::new("Camera", "");
    let id = cam.id.clone();
    let hub = hub();
    let events = Arc::new(OnvifEvents::new());
    let (stop, stopped) = watch::channel(false);
    let (found, mut discovered) = mpsc::unbounded_channel();
    let task = tokio::spawn(run(
        cam,
        Arc::clone(&hub),
        Arc::clone(&events),
        None,
        stopped,
        found,
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if events
                .status(&id)
                .is_some_and(|status| status.state == SubscriptionState::Error)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("invalid addresses must fail without contacting a camera");
    assert!(events
        .status(&id)
        .unwrap()
        .detail
        .unwrap()
        .contains("address"));
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(hub.open_events().is_empty());
    assert!(discovered.try_recv().is_err());
}
