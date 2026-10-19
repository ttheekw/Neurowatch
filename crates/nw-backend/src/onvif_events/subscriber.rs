//! One camera's event subscription, kept up until it is stopped.
//!
//! Push first: the camera is asked to send its events to NeuroWatch
//! ([`super::push`]). Cameras send their current state as soon as they are
//! subscribed, so if nothing arrives within [`PUSH_PROBE`] the camera cannot
//! reach this computer (a firewall, another network) or does not push: the
//! subscription is cancelled and the camera is asked for its events instead
//! (pull points), from then on for this subscriber.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nw_core::camera::{CameraConfig, CameraId, OnvifEndpoint};
use nw_core::event::EventSource;
use tokio::sync::{mpsc, watch};

use super::push::PushListener;
use super::tracker::{Action, Tracker};
use super::{detect, parse, Delivery, OnvifEvents, SubscriptionState};
use crate::discovery::onvif::{CallOptions, Credentials, OnvifError, SoapClient};
use crate::events::{now_ms, EventHub, NewEvent};

const CREATE: &str = "<tev:CreatePullPointSubscription>\
    <tev:InitialTerminationTime>PT120S</tev:InitialTerminationTime>\
    </tev:CreatePullPointSubscription>";
/// The device holds a pull open this long when nothing happens.
const PULL: &str = "<tev:PullMessages><tev:Timeout>PT10S</tev:Timeout>\
    <tev:MessageLimit>64</tev:MessageLimit></tev:PullMessages>";
const PULL_TIMEOUT: Duration = Duration::from_secs(25);
const RENEW: &str = "<wsnt:Renew><wsnt:TerminationTime>PT120S</wsnt:TerminationTime></wsnt:Renew>";
const RENEW_EVERY: Duration = Duration::from_secs(60);
const UNSUBSCRIBE: &str = "<wsnt:Unsubscribe/>";
/// How long to wait for a pushed camera's first delivery (its current state).
pub(super) const PUSH_PROBE: Duration = Duration::from_secs(8);

fn subscribe_body(consumer: &str) -> String {
    format!(
        "<wsnt:Subscribe><wsnt:ConsumerReference><wsa:Address>{consumer}</wsa:Address>\
         </wsnt:ConsumerReference><wsnt:InitialTerminationTime>PT120S\
         </wsnt:InitialTerminationTime></wsnt:Subscribe>"
    )
}

/// What came of trying push.
enum Push {
    /// Push worked for a while (or was stopped): this is the session's end.
    Ran(Outcome),
    /// Push is not possible with this camera: ask for events instead.
    Fallback(String),
}
/// A device that answers an empty pull at once is not held open; don't spin.
const FAST_EMPTY_PULL: Duration = Duration::from_millis(500);
const BACKOFF: [u64; 5] = [5, 10, 30, 60, 120];
/// Settings the camera refuses do not change by themselves.
const LONG_BACKOFF: Duration = Duration::from_secs(600);

enum Outcome {
    Stopped,
    Failed {
        state: SubscriptionState,
        detail: String,
        /// It worked for a while, so retry soon.
        was_subscribed: bool,
    },
}

fn failed(error: OnvifError, was_subscribed: bool) -> Outcome {
    let state = match error {
        OnvifError::Unauthorized => SubscriptionState::Unauthorized,
        OnvifError::Disabled => SubscriptionState::Disabled,
        OnvifError::Unreachable(_) => SubscriptionState::Unreachable,
        OnvifError::Invalid(_) => SubscriptionState::Error,
    };
    Outcome::Failed {
        state,
        detail: error.to_string(),
        was_subscribed,
    }
}

struct Session<'a> {
    cam: &'a CameraConfig,
    hub: &'a EventHub,
    events: &'a OnvifEvents,
    tracker: Tracker,
    open: HashMap<String, i64>,
    /// Where pushed events arrive; `None`: pull only.
    push: Option<Arc<PushListener>>,
    /// Why push was given up for this camera, once it was.
    push_failed: Option<String>,
}

impl Session<'_> {
    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Start {
                    key,
                    kind,
                    topic,
                    detail,
                } => {
                    let mut event =
                        NewEvent::new(Some(self.cam.id.clone()), kind, EventSource::Onvif)
                            .topic(topic);
                    event.detail = detail;
                    let record = self.hub.start(event);
                    self.open.insert(key, record.id);
                    self.mark_event();
                }
                Action::End { key } => {
                    if let Some(id) = self.open.remove(&key) {
                        self.hub.end(id);
                    }
                }
                Action::Instant {
                    kind,
                    topic,
                    detail,
                } => {
                    let mut event =
                        NewEvent::new(Some(self.cam.id.clone()), kind, EventSource::Onvif)
                            .topic(topic);
                    event.detail = detail;
                    self.hub.instant(event, now_ms());
                    self.mark_event();
                }
            }
        }
    }

    fn mark_event(&self) {
        let now = now_ms();
        self.events
            .update(&self.cam.id, |status| status.last_event_ms = Some(now));
    }

    /// Nothing is known to be going on once the subscription is gone.
    fn close_all(&mut self) {
        let actions = self.tracker.reset();
        self.apply(actions);
        for (_, id) in self.open.drain() {
            self.hub.end(id);
        }
    }

    fn take(&mut self, notifications: &[parse::Notification]) {
        if self.hub.activity_paused(&self.cam.id) {
            let ended = self.tracker.reset_activity();
            self.apply(ended);
        }
        for notification in notifications {
            if self.hub.activity_paused(&self.cam.id)
                && matches!(
                    super::tracker::classify(&notification.topic),
                    Some(
                        nw_core::event::EventKind::Motion
                            | nw_core::event::EventKind::Intrusion
                            | nw_core::event::EventKind::LineCrossing
                    )
                )
            {
                continue;
            }
            let actions = self.tracker.observe(notification);
            self.apply(actions);
        }
        let topics = self.tracker.seen.clone();
        self.events
            .update(&self.cam.id, |status| status.topics = topics);
    }

    fn subscribed(&self, delivery: Delivery) {
        tracing::info!(cam_id = %self.cam.id, ?delivery, "Camera events: subscribed");
        let note = self.push_failed.clone();
        self.events.update(&self.cam.id, |status| {
            status.state = SubscriptionState::Subscribed;
            status.detail = None;
            status.delivery = Some(delivery);
            status.note = note;
        });
    }

    /// Ask the camera to push its events here; see the module comment.
    async fn push_session(
        &mut self,
        listener: &Arc<PushListener>,
        client: &SoapClient,
        events_url: &str,
        creds: Option<&Credentials>,
        address: Ipv4Addr,
        stop: &mut watch::Receiver<bool>,
    ) -> Push {
        let Some(mut route) = listener.route(IpAddr::V4(address)) else {
            return Push::Fallback("no network address of this computer reaches the camera".into());
        };
        let body = subscribe_body(&route.url);
        let reply = tokio::select! {
            reply = client.call(events_url, &body, creds) => reply,
            _ = stop.changed() => return Push::Ran(Outcome::Stopped),
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(
                error @ (OnvifError::Unauthorized
                | OnvifError::Disabled
                | OnvifError::Unreachable(_)),
            ) => return Push::Ran(failed(error, false)),
            Err(OnvifError::Invalid(detail)) => {
                return Push::Fallback(format!(
                    "the camera does not send its events to other computers ({detail})"
                ))
            }
        };
        let Some(subscription) = parse::subscription(&reply, address) else {
            return Push::Fallback(
                "the camera's answer to sending events could not be read".into(),
            );
        };
        let options = CallOptions {
            addressing: Some(subscription.addressing.clone()),
            timeout: Some(Duration::from_secs(5)),
        };
        let unsubscribe = || async {
            let quick = CallOptions {
                timeout: Some(Duration::from_secs(2)),
                ..options.clone()
            };
            let _ = client
                .call_with(&subscription.addressing.to, UNSUBSCRIBE, creds, &quick)
                .await;
        };
        // The camera sends its current state at once when it can reach us.
        let first = tokio::select! {
            first = tokio::time::timeout(PUSH_PROBE, route.deliveries.recv()) => first,
            _ = stop.changed() => {
                unsubscribe().await;
                return Push::Ran(Outcome::Stopped);
            }
        };
        let Ok(Some(first)) = first else {
            unsubscribe().await;
            return Push::Fallback(format!(
                "nothing arrived from the camera within {} s: it may not reach this computer \
                 (allow TCP port {} through the firewall to receive events directly)",
                PUSH_PROBE.as_secs(),
                listener.port()
            ));
        };
        self.subscribed(Delivery::Push);
        let mut delivery = Some(first);
        let mut renew_at = Instant::now() + RENEW_EVERY;
        loop {
            if let Some(body) = delivery.take() {
                match parse::notifications(&body) {
                    Ok(notifications) => self.take(&notifications),
                    Err(detail) => {
                        tracing::debug!(cam_id = %self.cam.id, %detail, "Camera events: unreadable delivery")
                    }
                }
            }
            tokio::select! {
                next = route.deliveries.recv() => match next {
                    Some(body) => delivery = Some(body),
                    None => return Push::Ran(failed(OnvifError::Invalid("event deliveries stopped".into()), true)),
                },
                _ = tokio::time::sleep_until(renew_at.into()) => {
                    if let Err(error) = client
                        .call_with(&subscription.addressing.to, RENEW, creds, &options)
                        .await
                    {
                        tracing::info!(cam_id = %self.cam.id, error = %error, "Camera events: renewing failed; subscribing again");
                        return Push::Ran(failed(error, true));
                    }
                    renew_at = Instant::now() + RENEW_EVERY;
                }
                _ = stop.changed() => {
                    unsubscribe().await;
                    return Push::Ran(Outcome::Stopped);
                }
            }
        }
    }

    async fn subscribe(
        &mut self,
        stop: &mut watch::Receiver<bool>,
        found: &mpsc::UnboundedSender<(CameraId, OnvifEndpoint)>,
    ) -> Outcome {
        let detected = tokio::select! {
            detected = detect(self.cam) => detected,
            _ = stop.changed() => return Outcome::Stopped,
        };
        let detected = match detected {
            Ok(detected) => detected,
            Err(error) => return failed(error, false),
        };
        if self.cam.onvif.as_ref() != Some(&detected.endpoint) {
            let _ = found.send((self.cam.id.clone(), detected.endpoint.clone()));
        }
        let Some(events_url) = detected.services.events_url.clone() else {
            return Outcome::Failed {
                state: SubscriptionState::Unsupported,
                detail: "the camera has no ONVIF event service".into(),
                was_subscribed: false,
            };
        };
        let creds = super::credentials(self.cam);
        let client = detected.client;
        if self.push_failed.is_none() {
            if let Some(listener) = self.push.clone() {
                match self
                    .push_session(
                        &listener,
                        &client,
                        &events_url,
                        creds.as_ref(),
                        detected.address,
                        stop,
                    )
                    .await
                {
                    Push::Ran(outcome) => return outcome,
                    Push::Fallback(reason) => {
                        tracing::info!(cam_id = %self.cam.id, %reason, "Camera events: push not possible; asking the camera for events instead");
                        self.push_failed = Some(reason);
                    }
                }
            }
        }
        let created = tokio::select! {
            created = client.call(&events_url, CREATE, creds.as_ref()) => created,
            _ = stop.changed() => return Outcome::Stopped,
        };
        let reply = match created {
            Ok(reply) => reply,
            Err(OnvifError::Invalid(detail)) if !detected.services.pull_point => {
                return Outcome::Failed {
                    state: SubscriptionState::Unsupported,
                    detail: format!("the camera does not offer event pull points ({detail})"),
                    was_subscribed: false,
                }
            }
            Err(error) => return failed(error, false),
        };
        let Some(subscription) = parse::subscription(&reply, detected.address) else {
            return Outcome::Failed {
                state: SubscriptionState::Error,
                detail: "the camera's event subscription could not be read".into(),
                was_subscribed: false,
            };
        };
        self.subscribed(Delivery::Pull);
        let options = CallOptions {
            addressing: Some(subscription.addressing.clone()),
            timeout: Some(PULL_TIMEOUT),
        };
        let mut renew_at = Instant::now() + RENEW_EVERY;
        loop {
            let started = Instant::now();
            let pulled = tokio::select! {
                pulled = client.call_with(&subscription.addressing.to, PULL, creds.as_ref(), &options) => pulled,
                _ = stop.changed() => {
                    let quick = CallOptions { timeout: Some(Duration::from_secs(2)), ..options.clone() };
                    let _ = client
                        .call_with(&subscription.addressing.to, UNSUBSCRIBE, creds.as_ref(), &quick)
                        .await;
                    return Outcome::Stopped;
                }
            };
            let notifications = match pulled.map(|reply| parse::notifications(&reply)) {
                Ok(Ok(notifications)) => notifications,
                Ok(Err(detail)) => return failed(OnvifError::Invalid(detail), true),
                Err(error) => return failed(error, true),
            };
            let empty = notifications.is_empty();
            self.take(&notifications);
            if Instant::now() >= renew_at {
                if let Err(error) = client
                    .call_with(&subscription.addressing.to, RENEW, creds.as_ref(), &options)
                    .await
                {
                    tracing::info!(cam_id = %self.cam.id, error = %error, "Camera events: renewing failed; subscribing again");
                    return failed(error, true);
                }
                renew_at = Instant::now() + RENEW_EVERY;
            }
            if empty && started.elapsed() < FAST_EMPTY_PULL {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = stop.changed() => return Outcome::Stopped,
                }
            }
        }
    }
}

pub(super) async fn run(
    cam: CameraConfig,
    hub: Arc<EventHub>,
    events: Arc<OnvifEvents>,
    push: Option<Arc<PushListener>>,
    mut stop: watch::Receiver<bool>,
    found: mpsc::UnboundedSender<(CameraId, OnvifEndpoint)>,
) {
    let mut session = Session {
        cam: &cam,
        hub: &hub,
        events: &events,
        tracker: Tracker::default(),
        open: HashMap::new(),
        push,
        push_failed: None,
    };
    let mut failures: usize = 0;
    loop {
        if *stop.borrow() {
            break;
        }
        events.update(&cam.id, |status| {
            if status.state != SubscriptionState::Subscribed {
                status.state = SubscriptionState::Connecting;
            }
        });
        let outcome = session.subscribe(&mut stop, &found).await;
        session.close_all();
        let wait = match outcome {
            Outcome::Stopped => break,
            Outcome::Failed {
                state,
                detail,
                was_subscribed,
            } => {
                if was_subscribed {
                    failures = 0;
                }
                let wait = match state {
                    SubscriptionState::Unsupported
                    | SubscriptionState::Unauthorized
                    | SubscriptionState::Disabled => LONG_BACKOFF,
                    _ => Duration::from_secs(BACKOFF[failures.min(BACKOFF.len() - 1)]),
                };
                failures += 1;
                tracing::warn!(cam_id = %cam.id, state = ?state, detail = %detail, retry_in_s = wait.as_secs(), "Camera events: no subscription");
                events.update(&cam.id, |status| {
                    status.state = state;
                    status.detail = Some(detail);
                });
                wait
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.changed() => break,
        }
    }
    session.close_all();
}
