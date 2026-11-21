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

// ---------------------------------------------------------------------------
// A scripted ONVIF camera on loopback
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Handler = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

struct FakeCamera {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeCamera {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn soap(body: &str) -> String {
    format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:tns1="http://www.onvif.org/ver10/topics"><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

async fn read_post(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let (end, length) = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while buffer.len() < end + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&buffer[end..end + length]).into_owned())
}

async fn fake_camera(make: impl FnOnce(u16) -> Handler) -> FakeCamera {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = make(port);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let Some(body) = read_post(&mut stream).await else {
                    return;
                };
                let (status, reply) = handler(&body);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    FakeCamera { port, task }
}

fn capabilities(port: u16, events: bool, pull_point: bool) -> String {
    let events = if events {
        format!(
            "<tt:Events><tt:XAddr>http://10.9.9.9:{port}/onvif/events</tt:XAddr>\
             <tt:WSPullPointSupport>{pull_point}</tt:WSPullPointSupport></tt:Events>"
        )
    } else {
        String::new()
    };
    soap(&format!(
        "<tds:GetCapabilitiesResponse><tds:Capabilities>{events}</tds:Capabilities></tds:GetCapabilitiesResponse>"
    ))
}

fn subscription_reply(port: u16) -> String {
    soap(&format!(
        "<tev:CreatePullPointSubscriptionResponse><tev:SubscriptionReference>\
         <wsa:Address>http://10.9.9.9:{port}/onvif/sub/1</wsa:Address>\
         </tev:SubscriptionReference></tev:CreatePullPointSubscriptionResponse>"
    ))
}

fn motion_notification(active: bool) -> String {
    format!(
        r#"<wsnt:NotificationMessage><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message PropertyOperation="Changed"><tt:Source><tt:SimpleItem Name="Rule" Value="Driveway"/></tt:Source><tt:Data><tt:SimpleItem Name="IsMotion" Value="{active}"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#
    )
}

fn pull_reply(notifications: &str) -> String {
    soap(&format!(
        "<tev:PullMessagesResponse>{notifications}</tev:PullMessagesResponse>"
    ))
}

/// The camera's common replies; `other` answers the rest (event service).
fn basics(
    port: u16,
    body: &str,
    events: bool,
    other: &dyn Fn(&str) -> (u16, String),
) -> (u16, String) {
    if body.contains("<tds:GetSystemDateAndTime") {
        (200, soap("<tds:GetSystemDateAndTimeResponse/>"))
    } else if body.contains("<tds:GetCapabilities>") {
        (200, capabilities(port, events, true))
    } else if body.contains("<wsnt:Unsubscribe/>") || body.contains("<wsnt:Renew>") {
        (200, soap("<wsnt:Response/>"))
    } else {
        other(body)
    }
}

fn camera_for(port: u16) -> CameraConfig {
    let mut cam = CameraConfig::new("Camera", format!("rtsp://127.0.0.1:{port}/stream"));
    cam.onvif = Some(OnvifEndpoint {
        device_url: format!("http://127.0.0.1:{port}/onvif/device_service"),
        events_url: None,
        ptz_url: None,
        allow_password_text: false,
    });
    cam
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

struct Running {
    id: String,
    hub: Arc<EventHub>,
    events: Arc<OnvifEvents>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    found: mpsc::UnboundedReceiver<(CameraId, OnvifEndpoint)>,
}

impl Running {
    fn start(cam: CameraConfig, push: Option<Arc<PushListener>>) -> Self {
        let id = cam.id.clone();
        let hub = hub();
        let events = Arc::new(OnvifEvents::new());
        let (stop, stopped) = watch::channel(false);
        let (found_tx, found) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            cam,
            Arc::clone(&hub),
            Arc::clone(&events),
            push,
            stopped,
            found_tx,
        ));
        Self {
            id,
            hub,
            events,
            stop,
            task,
            found,
        }
    }

    fn state(&self) -> Option<SubscriptionState> {
        self.events.status(&self.id).map(|s| s.state)
    }

    async fn finish(self) -> (Arc<EventHub>, Arc<OnvifEvents>) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("subscriber stops promptly")
            .unwrap();
        (self.hub, self.events)
    }
}

#[tokio::test]
async fn pulled_events_open_hub_events_and_the_found_endpoint_is_reported() {
    let pulls = Arc::new(AtomicUsize::new(0));
    let unsubscribed = Arc::new(AtomicUsize::new(0));
    let camera = {
        let pulls = Arc::clone(&pulls);
        let unsubscribed = Arc::clone(&unsubscribed);
        fake_camera(move |port| {
            Arc::new(move |body| {
                if body.contains("<wsnt:Unsubscribe/>") {
                    unsubscribed.fetch_add(1, Ordering::SeqCst);
                }
                basics(port, body, true, &|body| {
                    if body.contains("<tev:CreatePullPointSubscription>") {
                        (200, subscription_reply(port))
                    } else if body.contains("<tev:PullMessages>") {
                        if pulls.fetch_add(1, Ordering::SeqCst) == 0 {
                            (200, pull_reply(&motion_notification(true)))
                        } else {
                            (200, pull_reply(""))
                        }
                    } else {
                        (500, String::new())
                    }
                })
            })
        })
        .await
    };
    let mut running = Running::start(camera_for(camera.port), None);
    eventually("a pulled motion event", || {
        running.hub.open_events().len() == 1
    })
    .await;
    let status = running.events.status(&running.id).unwrap();
    assert_eq!(status.state, SubscriptionState::Subscribed);
    assert_eq!(status.delivery, Some(Delivery::Pull));
    assert!(status.note.is_none());
    let (id, endpoint) = running.found.recv().await.unwrap();
    assert_eq!(id, running.id);
    assert_eq!(
        endpoint.events_url.as_deref(),
        Some(format!("http://127.0.0.1:{}/onvif/events", camera.port).as_str())
    );
    // Wait for a second (empty) pull so stopping lands in the idle wait or a pull.
    eventually("a further pull", || pulls.load(Ordering::SeqCst) >= 2).await;
    let (hub, _) = running.finish().await;
    assert!(
        hub.open_events().is_empty(),
        "stopping closes what was open"
    );
}

#[tokio::test]
async fn a_camera_without_an_event_service_is_unsupported() {
    let camera = fake_camera(|port| {
        Arc::new(move |body| basics(port, body, false, &|_| (500, String::new())))
    })
    .await;
    let running = Running::start(camera_for(camera.port), None);
    eventually("unsupported", || {
        running.state() == Some(SubscriptionState::Unsupported)
    })
    .await;
    let detail = running.events.status(&running.id).unwrap().detail.unwrap();
    assert!(detail.contains("no ONVIF event service"), "{detail}");
    running.finish().await;
}

#[tokio::test]
async fn pull_point_creation_failures_map_to_states() {
    // 1: refused where the camera advertises no pull points: Unsupported.
    let camera = fake_camera(|port| {
        Arc::new(move |body| {
            if body.contains("<tds:GetCapabilities>") {
                let caps = capabilities(port, true, false);
                return (200, caps);
            }
            basics(port, body, true, &|_| (500, String::new()))
        })
    })
    .await;
    let running = Running::start(camera_for(camera.port), None);
    eventually("unsupported pull points", || {
        running.state() == Some(SubscriptionState::Unsupported)
    })
    .await;
    let detail = running.events.status(&running.id).unwrap().detail.unwrap();
    assert!(detail.contains("pull points"), "{detail}");
    running.finish().await;

    // 2: refused although advertised: a plain error.
    let camera = fake_camera(|port| {
        Arc::new(move |body| basics(port, body, true, &|_| (500, String::new())))
    })
    .await;
    let running = Running::start(camera_for(camera.port), None);
    eventually("error", || {
        running.state() == Some(SubscriptionState::Error)
    })
    .await;
    running.finish().await;

    // 3: an answer without a subscription reference.
    let camera = fake_camera(|port| {
        Arc::new(move |body| basics(port, body, true, &|_| (200, soap("<tev:Nothing/>"))))
    })
    .await;
    let running = Running::start(camera_for(camera.port), None);
    eventually("unreadable subscription", || {
        running
            .events
            .status(&running.id)
            .and_then(|s| s.detail)
            .is_some_and(|d| d.contains("could not be read"))
    })
    .await;
    assert_eq!(running.state(), Some(SubscriptionState::Error));
    running.finish().await;

    // 4: not authorized.
    let camera = fake_camera(|port| {
        Arc::new(move |body| basics(port, body, true, &|_| (401, "NotAuthorized".to_owned())))
    })
    .await;
    let running = Running::start(camera_for(camera.port), None);
    eventually("unauthorized", || {
        running.state() == Some(SubscriptionState::Unauthorized)
    })
    .await;
    running.finish().await;
}

#[tokio::test]
async fn a_failing_pull_and_an_unreadable_pull_end_the_session_with_an_error() {
    for reply in [(500, String::new()), (200, "not xml at all <".to_owned())] {
        let camera = fake_camera(move |port| {
            let reply = reply.clone();
            Arc::new(move |body| {
                basics(port, body, true, &|body| {
                    if body.contains("<tev:CreatePullPointSubscription>") {
                        (200, subscription_reply(port))
                    } else {
                        reply.clone()
                    }
                })
            })
        })
        .await;
        let running = Running::start(camera_for(camera.port), None);
        eventually("a failed pull", || {
            running.state() == Some(SubscriptionState::Error)
        })
        .await;
        running.finish().await;
    }
}

#[tokio::test]
async fn an_unreachable_camera_is_reported_and_disabled_onvif_is_remembered() {
    // Nothing listens on this port.
    let free = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let running = Running::start(camera_for(free), None);
    eventually("unreachable", || {
        running.state() == Some(SubscriptionState::Unreachable)
    })
    .await;
    running.finish().await;

    let camera =
        fake_camera(|_| Arc::new(|_| (500, "ONVIF integrate function is disabled".to_owned())))
            .await;
    let running = Running::start(camera_for(camera.port), None);
    eventually("disabled", || {
        running.state() == Some(SubscriptionState::Disabled)
    })
    .await;
    running.finish().await;
}

/// A camera that, once subscribed in push mode, posts `deliveries` to the
/// address it was given.
fn pushing_camera(
    deliveries: Vec<String>,
    subscribe_reply: impl Fn(u16) -> (u16, String) + Send + Sync + 'static,
    unsubscribed: Arc<AtomicUsize>,
    subscribed: Arc<AtomicUsize>,
) -> impl FnOnce(u16) -> Handler {
    move |port| {
        Arc::new(move |body| {
            if body.contains("<wsnt:Unsubscribe/>") {
                unsubscribed.fetch_add(1, Ordering::SeqCst);
            }
            basics(port, body, true, &|body| {
                if body.contains("<wsnt:Subscribe>") {
                    subscribed.fetch_add(1, Ordering::SeqCst);
                    let reply = subscribe_reply(port);
                    if reply.0 == 200 {
                        let consumer = body
                            .split("<wsa:Address>")
                            .nth(1)
                            .and_then(|rest| rest.split("</wsa:Address>").next())
                            .unwrap()
                            .to_owned();
                        let deliveries = deliveries.clone();
                        tokio::spawn(async move {
                            let (_, rest) = consumer.split_once("://").unwrap();
                            let (authority, path) = rest.split_once('/').unwrap();
                            for delivery in deliveries {
                                let Ok(mut stream) =
                                    tokio::net::TcpStream::connect(authority).await
                                else {
                                    return;
                                };
                                let request = format!(
                                    "POST /{path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{delivery}",
                                    delivery.len()
                                );
                                let _ = stream.write_all(request.as_bytes()).await;
                                let mut sink = String::new();
                                let _ = stream.read_to_string(&mut sink).await;
                            }
                        });
                    }
                    reply
                } else if body.contains("<tev:CreatePullPointSubscription>") {
                    (200, subscription_reply(port))
                } else if body.contains("<tev:PullMessages>") {
                    (200, pull_reply(""))
                } else {
                    (500, String::new())
                }
            })
        })
    }
}

#[tokio::test]
async fn pushed_events_open_hub_events_and_stopping_unsubscribes() {
    let unsubscribed = Arc::new(AtomicUsize::new(0));
    let subscribed = Arc::new(AtomicUsize::new(0));
    let camera = fake_camera(pushing_camera(
        vec![
            "<garbage".to_owned(),
            pull_reply(&motion_notification(true)),
        ],
        |port| (200, subscription_reply(port)),
        Arc::clone(&unsubscribed),
        Arc::clone(&subscribed),
    ))
    .await;
    let listener = PushListener::start(0).await.unwrap();
    let running = Running::start(camera_for(camera.port), Some(listener));
    eventually("a pushed motion event", || {
        running.hub.open_events().len() == 1
    })
    .await;
    let status = running.events.status(&running.id).unwrap();
    assert_eq!(status.state, SubscriptionState::Subscribed);
    assert_eq!(status.delivery, Some(Delivery::Push));
    running.finish().await;
    assert_eq!(subscribed.load(Ordering::SeqCst), 1);
    assert_eq!(unsubscribed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn push_that_the_camera_refuses_falls_back_to_pulling_with_a_note() {
    for (reply, reason) in [
        ((500, String::new()), "does not send its events"),
        ((200, soap("<tev:Nothing/>")), "could not be read"),
    ] {
        let camera = fake_camera(pushing_camera(
            vec![],
            move |_| reply.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        ))
        .await;
        let listener = PushListener::start(0).await.unwrap();
        let running = Running::start(camera_for(camera.port), Some(listener));
        eventually("pulling after a refused push", || {
            running
                .events
                .status(&running.id)
                .is_some_and(|s| s.delivery == Some(Delivery::Pull))
        })
        .await;
        let note = running.events.status(&running.id).unwrap().note.unwrap();
        assert!(note.contains(reason), "{note}");
        running.finish().await;
    }
}

#[tokio::test]
async fn push_failures_that_are_not_refusals_end_the_session() {
    let camera = fake_camera(|port| {
        Arc::new(move |body| basics(port, body, true, &|_| (401, "NotAuthorized".to_owned())))
    })
    .await;
    let listener = PushListener::start(0).await.unwrap();
    let running = Running::start(camera_for(camera.port), Some(listener));
    eventually("unauthorized push", || {
        running.state() == Some(SubscriptionState::Unauthorized)
    })
    .await;
    running.finish().await;
}

#[tokio::test]
async fn stopping_while_waiting_for_a_pushed_first_delivery_unsubscribes() {
    let unsubscribed = Arc::new(AtomicUsize::new(0));
    let subscribed = Arc::new(AtomicUsize::new(0));
    let camera = fake_camera(pushing_camera(
        vec![],
        |port| (200, subscription_reply(port)),
        Arc::clone(&unsubscribed),
        Arc::clone(&subscribed),
    ))
    .await;
    let listener = PushListener::start(0).await.unwrap();
    let running = Running::start(camera_for(camera.port), Some(listener));
    eventually("the push subscription", || {
        subscribed.load(Ordering::SeqCst) == 1
    })
    .await;
    running.finish().await;
    assert_eq!(unsubscribed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_manager_starts_subscribers_stores_the_found_endpoint_and_forgets_removed_cameras() {
    use crate::app_state::AppState;
    use crate::pipeline::hwaccel::HwAccelKind;
    let camera = fake_camera(|port| {
        Arc::new(move |body| {
            basics(port, body, true, &|body| {
                if body.contains("<tev:CreatePullPointSubscription>") {
                    (200, subscription_reply(port))
                } else {
                    (200, pull_reply(""))
                }
            })
        })
    })
    .await;
    let dir = std::env::temp_dir().join(format!("nw-onvif-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cam = camera_for(camera.port);
    cam.motion.source = nw_core::camera::MotionSource::Onvif;
    cam.id = "cam-onvif".into();
    let mut config = crate::config::AppConfig::default();
    config.events.push_port = 0;
    config.cameras.push(cam);
    // A camera that does not use ONVIF is left alone.
    let mut other = CameraConfig::new("Other", "rtsp://127.0.0.1:1/x");
    other.id = "cam-other".into();
    other.motion.source = nw_core::camera::MotionSource::Off;
    config.cameras.push(other);
    let state = Arc::new(tokio::sync::RwLock::new(AppState::with_database(
        config,
        dir.join("config.json"),
        HwAccelKind::Software,
        crate::db::connection::open_in_memory().unwrap(),
    )));
    let events = Arc::new(OnvifEvents::new());
    let (shutdown, shutdown_rx) = tokio::sync::broadcast::channel(1);
    let task = tokio::spawn(Arc::clone(&events).run(Arc::clone(&state), hub(), shutdown_rx));
    eventually("the subscription", || {
        events
            .status("cam-onvif")
            .is_some_and(|s| s.state == SubscriptionState::Subscribed)
    })
    .await;
    assert!(events.status("cam-other").is_none());
    eventually("the stored endpoint", || {
        state
            .try_read()
            .map(|st| {
                st.config.cameras[0]
                    .onvif
                    .as_ref()
                    .is_some_and(|endpoint| endpoint.events_url.is_some())
            })
            .unwrap_or(false)
    })
    .await;
    assert!(dir.join("config.json").exists(), "the endpoint was saved");

    // Turning the source off stops the subscriber at the next reconcile.
    state.write().await.config.cameras[0].motion.source = nw_core::camera::MotionSource::Off;
    events.kick();
    eventually("the subscriber to be forgotten", || {
        events.status("cam-onvif").is_none()
    })
    .await;
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
