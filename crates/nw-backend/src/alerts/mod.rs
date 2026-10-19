//! Alert notifications: events that match a rule are sent to its channels
//! (this computer's desktop, a webhook, ntfy, Telegram).
//!
//! The service follows the [`EventHub`]: each event that starts (instant
//! events too) is checked against the enabled rules (kind, camera, the
//! rule's local-time windows), then each rule stays quiet for its cooldown
//! per camera. Deliveries go through one bounded queue on their own task,
//! at most [`CHANNEL_RATE`] per channel a minute, with a short timeout; a
//! slow or failing channel never holds up events or recording. The last
//! deliveries are kept for `alerts.history`. Desktop alerts wait in a short
//! numbered list the desktop app reads (`alerts.desktop`); a headless
//! recorder has no desktop and keeps none.

pub mod config;
mod deliver;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nw_core::event::{EventKind, EventPhase, EventRecord};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::app_state::AppState;
use crate::events::EventHub;
pub use config::{AlertChannel, AlertRule, AlertsConfig, ChannelTarget, KEPT_SECRET};

/// Deliveries per channel per minute; more are skipped (and listed).
pub const CHANNEL_RATE: usize = 20;
const QUEUE: usize = 64;
const HISTORY: usize = 100;
const DESKTOP: usize = 50;
/// How long a delivery waits for the event's picture to be saved.
const PICTURE_WAIT: Duration = Duration::from_secs(8);
const MAX_PICTURE_BYTES: u64 = 2 * 1024 * 1024;

/// What an alert says.
#[derive(Debug, Clone)]
pub struct Alert {
    pub title: String,
    pub message: String,
    pub event: EventRecord,
    pub camera_name: Option<String>,
    pub rule_name: Option<String>,
    pub test: bool,
}

/// One delivery, as `alerts.history` lists it (newest first).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Delivery {
    pub at_ms: i64,
    pub channel_id: String,
    pub channel_name: String,
    pub kind: &'static str,
    pub rule: Option<String>,
    pub title: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub test: bool,
}

/// A notification for this computer's desktop.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DesktopAlert {
    pub seq: u64,
    pub at_ms: i64,
    pub title: String,
    pub message: String,
    pub camera_id: Option<String>,
    pub event_id: i64,
    pub kind: &'static str,
}

struct Job {
    channel: AlertChannel,
    alert: Alert,
}

#[derive(Default)]
struct Inner {
    quiet_until: HashMap<(String, String), Instant>,
    sent: HashMap<String, VecDeque<Instant>>,
    history: VecDeque<Delivery>,
    desktop: VecDeque<DesktopAlert>,
    desktop_seq: u64,
}

pub struct AlertService {
    client: Option<reqwest::Client>,
    inner: Mutex<Inner>,
    jobs: mpsc::Sender<Job>,
    queue: Mutex<Option<mpsc::Receiver<Job>>>,
}

impl Default for AlertService {
    fn default() -> Self {
        let (jobs, queue) = mpsc::channel(QUEUE);
        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(12))
            .connect_timeout(Duration::from_secs(6))
            .user_agent(concat!("NeuroWatch/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(client) => Some(client),
            Err(_) => {
                tracing::warn!("HTTP alert client unavailable; delivery disabled");
                None
            }
        };
        Self {
            client,
            inner: Mutex::new(Inner::default()),
            jobs,
            queue: Mutex::new(Some(queue)),
        }
    }
}

pub fn label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Motion => "Motion",
        EventKind::Intrusion => "Intrusion",
        EventKind::LineCrossing => "Line crossed",
        EventKind::Tamper => "Camera tampering",
        EventKind::DigitalInput => "Alarm input",
        EventKind::CameraEvent => "Camera event",
        EventKind::CameraOffline => "Camera offline",
        EventKind::RecordingFailed => "Recording failed",
        EventKind::DiskLow => "Recording drive almost full",
        EventKind::StorageMissing => "Recording storage missing",
        EventKind::BackendStarted => "Recorder started",
        EventKind::Bookmark => "Bookmark",
    }
}

/// "17 Oct 11:07:32" on the recorder's local clock.
pub fn local_time(ms: i64) -> String {
    let utc = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let local = local_offset(utc).map_or(utc, |offset| utc.to_offset(offset));
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {} {:02}:{:02}:{:02}",
        local.day(),
        months[usize::from(u8::from(local.month())) - 1],
        local.hour(),
        local.minute(),
        local.second()
    )
}

#[cfg(unix)]
fn local_offset(at: time::OffsetDateTime) -> Option<time::UtcOffset> {
    #[allow(clippy::useless_conversion)]
    let when: libc::time_t = at.unix_timestamp().try_into().ok()?;
    let mut calendar = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: valid time_t and writable tm storage; localtime_r initializes
    // tm on success and keeps no shared static buffer.
    if unsafe { libc::localtime_r(&when, calendar.as_mut_ptr()) }.is_null() {
        return None;
    }
    let calendar = unsafe { calendar.assume_init() };
    time::UtcOffset::from_whole_seconds(i32::try_from(calendar.tm_gmtoff).ok()?).ok()
}

#[cfg(not(unix))]
fn local_offset(_at: time::OffsetDateTime) -> Option<time::UtcOffset> {
    time::UtcOffset::current_local_offset().ok()
}

/// The alert an event sends.
pub fn alert_for(event: &EventRecord, camera_name: Option<&str>, rule: Option<&str>) -> Alert {
    let what = label(event.kind);
    let place = camera_name.unwrap_or("Recorder");
    let mut message = format!("{what} on {place} at {}", local_time(event.start_ms));
    if let Some(detail) = event
        .detail
        .as_deref()
        .filter(|d| !d.is_empty() && Some(*d) != camera_name)
    {
        message.push_str(&format!(": {detail}"));
    }
    Alert {
        title: format!("{what} · {place}"),
        message,
        event: event.clone(),
        camera_name: camera_name.map(str::to_owned),
        rule_name: rule.map(str::to_owned),
        test: false,
    }
}

fn now_ms() -> i64 {
    crate::events::now_ms()
}

impl AlertService {
    async fn send_http(
        &self,
        target: &ChannelTarget,
        alert: &Alert,
        picture: Option<&[u8]>,
    ) -> Result<(), String> {
        let client = self
            .client
            .as_ref()
            .ok_or("HTTP alert client is unavailable")?;
        deliver::deliver(client, target, alert, picture).await
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record(&self, delivery: Delivery) {
        if !delivery.ok {
            tracing::warn!(channel = %delivery.channel_name, error = ?delivery.error, "Alert not delivered");
        }
        let mut inner = self.lock();
        inner.history.push_front(delivery);
        inner.history.truncate(HISTORY);
    }

    fn delivery(channel: &AlertChannel, alert: &Alert, result: Result<(), String>) -> Delivery {
        Delivery {
            at_ms: now_ms(),
            channel_id: channel.id.clone(),
            channel_name: channel.name.clone(),
            kind: channel.target.kind(),
            rule: alert.rule_name.clone(),
            title: alert.title.clone(),
            ok: result.is_ok(),
            error: result.err(),
            test: alert.test,
        }
    }

    fn push_desktop(&self, alert: &Alert) {
        let mut inner = self.lock();
        inner.desktop_seq += 1;
        let entry = DesktopAlert {
            seq: inner.desktop_seq,
            at_ms: now_ms(),
            title: alert.title.clone(),
            message: alert.message.clone(),
            camera_id: alert.event.camera_id.clone(),
            event_id: alert.event.id,
            kind: alert.event.kind.as_str(),
        };
        inner.desktop.push_back(entry);
        while inner.desktop.len() > DESKTOP {
            inner.desktop.pop_front();
        }
    }

    /// Desktop alerts after `after` (none without it), and the latest number.
    pub fn desktop_since(&self, after: Option<u64>) -> (u64, Vec<DesktopAlert>) {
        let inner = self.lock();
        let alerts = match after {
            Some(after) => inner
                .desktop
                .iter()
                .filter(|a| a.seq > after)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        (inner.desktop_seq, alerts)
    }

    pub fn history(&self, limit: usize) -> Vec<Delivery> {
        self.lock().history.iter().take(limit).cloned().collect()
    }

    /// Whether the rule may fire for this camera now (and starts its quiet time).
    fn take_turn(&self, rule: &AlertRule, camera: &str, now: Instant) -> bool {
        let mut inner = self.lock();
        let key = (rule.id.clone(), camera.to_owned());
        if inner
            .quiet_until
            .get(&key)
            .is_some_and(|until| *until > now)
        {
            return false;
        }
        inner
            .quiet_until
            .insert(key, now + Duration::from_secs(u64::from(rule.cooldown_sec)));
        true
    }

    /// Whether the channel may send another this minute (and counts it).
    fn within_rate(&self, channel: &str, now: Instant) -> bool {
        let mut inner = self.lock();
        let sent = inner.sent.entry(channel.to_owned()).or_default();
        while sent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            sent.pop_front();
        }
        if sent.len() >= CHANNEL_RATE {
            return false;
        }
        sent.push_back(now);
        true
    }

    /// The alerts an event sends: (channel, alert) for every matching rule.
    pub fn route(
        &self,
        config: &AlertsConfig,
        names: &HashMap<String, String>,
        event: &EventRecord,
        local: Option<(u8, usize)>,
        now: Instant,
    ) -> Vec<(AlertChannel, Alert)> {
        let camera = event.camera_id.as_deref();
        let mut out: Vec<(AlertChannel, Alert)> = Vec::new();
        for rule in &config.rules {
            if !rule.fires_for(event.kind, camera) {
                continue;
            }
            // A rule with hours needs the clock; without it, stay quiet.
            let active = match local {
                Some((day, minute)) => rule.active_at(day, minute),
                None => rule.schedule.is_empty(),
            };
            if !active || !self.take_turn(rule, camera.unwrap_or(""), now) {
                continue;
            }
            let name = camera.and_then(|id| names.get(id)).map(String::as_str);
            let alert = alert_for(event, name.or(camera), Some(&rule.name));
            for channel in config
                .channels
                .iter()
                .filter(|c| c.enabled && rule.channels.contains(&c.id))
            {
                // Two rules matching one event send each channel one alert.
                if out.iter().any(|(sent, _)| sent.id == channel.id) {
                    continue;
                }
                out.push((channel.clone(), alert.clone()));
            }
        }
        out
    }

    fn dispatch(&self, channel: AlertChannel, alert: Alert, headless: bool) {
        if matches!(channel.target, ChannelTarget::Desktop) {
            let result = if headless {
                Err("this recorder runs without a desktop".to_owned())
            } else {
                self.push_desktop(&alert);
                Ok(())
            };
            self.record(Self::delivery(&channel, &alert, result));
            return;
        }
        if let Err(error) = self.jobs.try_send(Job { channel, alert }) {
            let job = match error {
                mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job) => {
                    job
                }
            };
            self.record(Self::delivery(
                &job.channel,
                &job.alert,
                Err("too many alerts waiting; skipped".into()),
            ));
        }
    }

    /// Send a test alert to `channel` now, and say how it went.
    pub async fn test(
        &self,
        channel: &AlertChannel,
        site: &str,
        headless: bool,
    ) -> Result<(), String> {
        let event = EventRecord {
            id: 0,
            camera_id: None,
            kind: EventKind::BackendStarted,
            source: nw_core::event::EventSource::System,
            start_ms: now_ms(),
            end_ms: Some(now_ms()),
            topic: None,
            score: None,
            zones: Vec::new(),
            bbox: None,
            thumbnail: None,
            detail: None,
            acknowledged: false,
            kept: false,
        };
        let alert = Alert {
            title: format!("Test alert · {site}"),
            message: format!(
                "NeuroWatch can reach {} at {}.",
                channel.name,
                local_time(event.start_ms)
            ),
            event,
            camera_name: None,
            rule_name: None,
            test: true,
        };
        let result = match &channel.target {
            ChannelTarget::Desktop if headless => {
                Err("this recorder runs without a desktop".to_owned())
            }
            ChannelTarget::Desktop => {
                self.push_desktop(&alert);
                Ok(())
            }
            target => self.send_http(target, &alert, None).await,
        };
        self.record(Self::delivery(channel, &alert, result.clone()));
        result
    }

    /// Follow the hub until shutdown: route new events, deliver queued alerts.
    pub async fn run(
        self: Arc<Self>,
        state: Arc<RwLock<AppState>>,
        hub: Arc<EventHub>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        let Some(mut queue) = self.queue.lock().ok().and_then(|mut q| q.take()) else {
            return;
        };
        let worker = {
            let service = Arc::clone(&self);
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                while let Some(job) = queue.recv().await {
                    service.send(&state, job).await;
                }
            })
        };
        let mut notices = hub.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                notice = notices.recv() => match notice {
                    Ok(notice) if notice.phase == EventPhase::Start => {
                        let (config, names, headless) = {
                            let st = state.read().await;
                            let names = st.config.cameras.iter().map(|c| (c.id.clone(), c.name.clone())).collect();
                            (st.config.alerts.clone(), names, st.headless())
                        };
                        if config.rules.is_empty() {
                            continue;
                        }
                        let local = crate::recording::policy::local_calendar();
                        for (channel, alert) in self.route(&config, &names, &notice.event, local, Instant::now()) {
                            self.dispatch(channel, alert, headless);
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "Alerts fell behind the events; some were not checked");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        worker.abort();
    }

    async fn send(&self, state: &Arc<RwLock<AppState>>, job: Job) {
        if !self.within_rate(&job.channel.id, Instant::now()) {
            self.record(Self::delivery(
                &job.channel,
                &job.alert,
                Err(format!("more than {CHANNEL_RATE} alerts a minute; skipped")),
            ));
            return;
        }
        let picture = if job.channel.attach_picture && job.alert.event.camera_id.is_some() {
            picture(state, job.alert.event.id).await
        } else {
            None
        };
        let result = self
            .send_http(&job.channel.target, &job.alert, picture.as_deref())
            .await;
        self.record(Self::delivery(&job.channel, &job.alert, result));
    }
}

/// The event's saved picture, waiting a little for it to be written.
async fn picture(state: &Arc<RwLock<AppState>>, event_id: i64) -> Option<Vec<u8>> {
    let deadline = Instant::now() + PICTURE_WAIT;
    loop {
        let (db, hub) = {
            let st = state.read().await;
            (Arc::clone(&st.db), Arc::clone(&st.events))
        };
        let _ = hub.flush_async().await;
        let thumbnail = tokio::task::spawn_blocking(move || {
            let conn = db.lock().ok()?;
            crate::events::store::get(&conn, event_id).ok()??.thumbnail
        })
        .await
        .ok()
        .flatten();
        if let Some(name) = thumbnail {
            let path = crate::events::events_dir().join(name);
            let size = tokio::fs::metadata(&path).await.ok()?.len();
            if size == 0 || size > MAX_PICTURE_BYTES {
                return None;
            }
            return tokio::fs::read(path).await.ok();
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    include!("redirect_tests.rs");
    use super::*;
    use nw_core::event::EventSource;
    use serde_json::json;

    fn event(kind: EventKind, camera: Option<&str>) -> EventRecord {
        EventRecord {
            id: 7,
            camera_id: camera.map(str::to_owned),
            kind,
            source: EventSource::Builtin,
            start_ms: 1_000,
            end_ms: None,
            topic: None,
            score: None,
            zones: Vec::new(),
            bbox: None,
            thumbnail: None,
            detail: None,
            acknowledged: false,
            kept: false,
        }
    }

    fn config() -> AlertsConfig {
        serde_json::from_value(json!({
            "channels": [
                {"id": "desk", "name": "This computer", "kind": "desktop"},
                {"id": "phone", "name": "Phone", "kind": "ntfy", "topic": "t"},
                {"id": "off", "name": "Off", "kind": "desktop", "enabled": false}
            ],
            "rules": [
                {"id": "gate", "name": "Gate motion", "kinds": ["motion"], "cameras": ["cam-1"],
                 "channels": ["desk", "phone", "off"], "cooldown_sec": 60},
                {"id": "any", "name": "Problems", "kinds": ["camera_offline", "motion"],
                 "channels": ["phone"], "cooldown_sec": 0,
                 "schedule": [{"days": [0], "start": "08:00", "end": "09:00"}]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn events_reach_each_matching_channel_once_then_the_rule_stays_quiet() {
        let service = AlertService::default();
        let names = HashMap::from([("cam-1".to_owned(), "Gate".to_owned())]);
        let now = Instant::now();
        let monday_8 = Some((0, 8 * 60 + 30));
        let routed = service.route(
            &config(),
            &names,
            &event(EventKind::Motion, Some("cam-1")),
            monday_8,
            now,
        );
        let channels: Vec<_> = routed.iter().map(|(c, _)| c.id.as_str()).collect();
        assert_eq!(
            channels,
            ["desk", "phone"],
            "disabled channels and repeats are left out"
        );
        assert_eq!(routed[0].1.title, "Motion · Gate");
        assert!(routed[0].1.message.starts_with("Motion on Gate at "));
        // Within the gate rule's quiet minute only `any` (no cooldown) fires.
        let again = service.route(
            &config(),
            &names,
            &event(EventKind::Motion, Some("cam-1")),
            monday_8,
            now,
        );
        assert_eq!(
            again.iter().map(|(c, _)| c.id.as_str()).collect::<Vec<_>>(),
            ["phone"]
        );
        let later = now + Duration::from_secs(61);
        assert_eq!(
            service
                .route(
                    &config(),
                    &names,
                    &event(EventKind::Motion, Some("cam-1")),
                    Some((0, 12 * 60)),
                    later
                )
                .len(),
            2
        );
        // Outside its hours, and without a clock, a scheduled rule stays quiet.
        assert!(service
            .route(
                &config(),
                &names,
                &event(EventKind::CameraOffline, Some("cam-2")),
                Some((1, 8 * 60 + 30)),
                later
            )
            .is_empty());
        assert!(service
            .route(
                &config(),
                &names,
                &event(EventKind::CameraOffline, Some("cam-2")),
                None,
                later
            )
            .is_empty());
    }

    #[tokio::test]
    async fn a_new_event_on_the_hub_reaches_the_desktop() {
        use crate::config::AppConfig;
        use crate::events::NewEvent;
        let path = std::env::temp_dir().join(format!("nw-alert-run-{}.json", uuid::Uuid::new_v4()));
        let mut config = AppConfig::default();
        let mut camera = nw_core::camera::CameraConfig::new("Gate", "rtsp://cam.invalid/1");
        camera.id = "cam-1".into();
        config.cameras.push(camera);
        config.alerts = serde_json::from_value(json!({
            "channels": [{"id": "desk", "name": "Desktop", "kind": "desktop"}],
            "rules": [{"id": "r", "name": "Motion", "kinds": ["motion"], "channels": ["desk"]}]
        }))
        .unwrap();
        let state = Arc::new(RwLock::new(AppState::with_database(
            config,
            path,
            crate::pipeline::hwaccel::HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )));
        let (hub, service) = {
            let st = state.read().await;
            (Arc::clone(&st.events), Arc::clone(&st.alerts))
        };
        let (stop, stopped) = broadcast::channel(1);
        let running =
            tokio::spawn(Arc::clone(&service).run(Arc::clone(&state), Arc::clone(&hub), stopped));
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub.instant(
            NewEvent::new(Some("cam-1".into()), EventKind::Tamper, EventSource::Onvif),
            5,
        );
        hub.start(NewEvent::new(
            Some("cam-1".into()),
            EventKind::Motion,
            EventSource::Builtin,
        ));
        let mut alerts = Vec::new();
        for _ in 0..50 {
            alerts = service.desktop_since(Some(0)).1;
            if !alerts.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(alerts.len(), 1, "tamper has no rule; motion does");
        assert_eq!(alerts[0].title, "Motion · Gate");
        let _ = stop.send(());
        running.await.unwrap();
    }

    #[test]
    fn desktop_alerts_are_numbered_and_a_headless_recorder_keeps_none() {
        let service = AlertService::default();
        let alert = alert_for(
            &event(EventKind::Motion, Some("cam-1")),
            Some("Gate"),
            Some("Gate motion"),
        );
        let desk = config().channels[0].clone();
        assert_eq!(service.desktop_since(None), (0, vec![]));
        service.dispatch(desk.clone(), alert.clone(), false);
        let (seq, alerts) = service.desktop_since(Some(0));
        assert_eq!((seq, alerts.len()), (1, 1));
        assert_eq!(alerts[0].title, "Motion · Gate");
        assert!(service.desktop_since(Some(1)).1.is_empty());
        service.dispatch(desk, alert, true);
        assert_eq!(service.desktop_since(None).0, 1);
        let history = service.history(10);
        assert!(
            !history[0].ok
                && history[0]
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("without a desktop")
        );
        assert!(history[1].ok);
    }

    #[test]
    fn a_channel_sends_at_most_its_rate_a_minute() {
        let service = AlertService::default();
        let now = Instant::now();
        for _ in 0..CHANNEL_RATE {
            assert!(service.within_rate("phone", now));
        }
        assert!(!service.within_rate("phone", now));
        assert!(service.within_rate("other", now));
        assert!(service.within_rate("phone", now + Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn a_failing_channel_is_reported_without_its_secret() {
        let service = AlertService::default();
        let channel: AlertChannel = serde_json::from_value(json!({
            "id": "bot", "name": "Chat", "kind": "webhook",
            "url": "http://127.0.0.1:9/hook", "secret": "s3cret"
        }))
        .unwrap();
        let result = service.test(&channel, "Site", false).await;
        let error = result.unwrap_err();
        assert!(
            !error.contains("s3cret") && !error.contains("127.0.0.1"),
            "{error}"
        );
        let history = service.history(1);
        assert!(history[0].test && !history[0].ok);
    }

    #[tokio::test]
    async fn a_webhook_receives_the_alert_as_json_with_its_token() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                received.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&received);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&received).into_owned()
        });
        let channel: AlertChannel = serde_json::from_value(json!({
            "id": "hook", "name": "Hook", "kind": "webhook",
            "url": format!("http://127.0.0.1:{port}/hook"), "secret": "tok"
        }))
        .unwrap();
        let alert = alert_for(
            &event(EventKind::Motion, Some("cam-1")),
            Some("Gate"),
            Some("Gate motion"),
        );
        deliver::deliver(
            AlertService::default().client.as_ref().unwrap(),
            &channel.target,
            &alert,
            None,
        )
        .await
        .unwrap();
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /hook "));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-neurowatch-token: tok"));
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["event"]["kind"], "motion");
        assert_eq!(body["event"]["camera_name"], "Gate");
        assert_eq!(body["rule"], "Gate motion");
    }
}
