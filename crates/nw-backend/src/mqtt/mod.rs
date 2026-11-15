//! MQTT and Home Assistant: NeuroWatch publishes its cameras' sensors
//! (motion, person, vehicle, animal, online), the latest event picture, a
//! recorder problem sensor and every event to an MQTT broker, and announces
//! them to Home Assistant through MQTT discovery. Topics are in [`discovery`].
//!
//! The service follows the [`EventHub`] like alerts do. The broker connection
//! runs on its own task (rumqttc's event loop) and every publish has a short
//! timeout, so a slow or missing broker never holds up events or recording.
//! Settings changes (`mqtt.set`) reconnect at once ([`MqttService::reconfigured`]).

pub mod config;
pub mod discovery;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nw_core::event::EventPhase;
use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, LastWill, MqttOptions, Packet, QoS,
    TlsConfiguration, Transport,
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, Notify, RwLock};

use crate::app_state::AppState;
use crate::events::EventHub;
pub use config::MqttConfig;
use discovery::{payload, Sensor, Sensors, Topics, OFFLINE, ONLINE};

/// A publish that cannot be queued within this is dropped (broker gone).
const PUBLISH_WAIT: Duration = Duration::from_secs(2);
/// After a failed connection, the next attempt waits this long.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How often the camera list is compared with what was announced.
const CAMERAS_EVERY: Duration = Duration::from_secs(30);
/// Event pictures can be large; the broker's own limit still applies.
const MAX_PACKET: usize = 4 * 1024 * 1024;
/// Kinds a camera reported within this long are announced at connection.
const SEEN_WITHIN_MS: i64 = 30 * 24 * 3600 * 1000;

/// `mqtt.status`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MqttStatus {
    /// `off`, `connecting`, `connected` or `error`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `host:port` while switched on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broker: Option<String>,
}

impl Default for MqttStatus {
    fn default() -> Self {
        Self {
            state: "off",
            detail: None,
            broker: None,
        }
    }
}

#[derive(Default)]
pub struct MqttService {
    status: Mutex<MqttStatus>,
    changed: Notify,
}

enum Ended {
    Shutdown,
    Changed,
}

enum Polled {
    Connected,
    Failed(String),
}

impl MqttService {
    pub fn status(&self) -> MqttStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn set_status(&self, state: &'static str, detail: Option<String>, broker: Option<String>) {
        if let Ok(mut status) = self.status.lock() {
            *status = MqttStatus {
                state,
                detail,
                broker,
            };
        }
    }

    /// The settings changed: reconnect with the new ones.
    pub fn reconfigured(&self) {
        self.changed.notify_one();
    }

    /// Follow the settings and the hub until shutdown.
    pub async fn run(
        self: Arc<Self>,
        state: Arc<RwLock<AppState>>,
        hub: Arc<EventHub>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        loop {
            let config = state.read().await.config.mqtt.clone();
            if !config.enabled || config.host.is_empty() {
                self.set_status("off", None, None);
                tokio::select! {
                    _ = shutdown.recv() => return,
                    _ = self.changed.notified() => continue,
                }
            }
            match self.session(&state, &hub, &config, &mut shutdown).await {
                Ended::Shutdown => return,
                Ended::Changed => continue,
            }
        }
    }

    async fn session(
        &self,
        state: &Arc<RwLock<AppState>>,
        hub: &Arc<EventHub>,
        config: &MqttConfig,
        shutdown: &mut broadcast::Receiver<()>,
    ) -> Ended {
        let broker = Some(format!("{}:{}", config.host, config.port));
        let topics = Topics::new(&config.topic_prefix, &config.discovery_prefix);
        let options = match options(config, &topics, "") {
            Ok(options) => options,
            Err(error) => {
                self.set_status("error", Some(error), broker);
                return tokio::select! {
                    _ = shutdown.recv() => Ended::Shutdown,
                    _ = self.changed.notified() => Ended::Changed,
                };
            }
        };
        self.set_status("connecting", None, broker.clone());
        let (client, mut eventloop) = AsyncClient::new(options, 256);
        let (tx, mut polled) = mpsc::channel(8);
        let poller = tokio::spawn(async move {
            loop {
                let message = match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => Polled::Connected,
                    Ok(_) => continue,
                    Err(error) => Polled::Failed(describe(&error)),
                };
                let failed = matches!(message, Polled::Failed(_));
                if tx.send(message).await.is_err() {
                    return;
                }
                if failed {
                    tokio::time::sleep(RETRY_AFTER).await;
                }
            }
        });
        let mut publisher = Publisher {
            client: client.clone(),
            topics,
            discovery: config.discovery,
            sensors: Sensors::default(),
            announced: BTreeSet::new(),
            cameras: BTreeMap::new(),
        };
        let mut notices = hub.subscribe();
        let mut connected = false;
        let mut cameras = tokio::time::interval(CAMERAS_EVERY);
        let ended = loop {
            tokio::select! {
                _ = shutdown.recv() => break Ended::Shutdown,
                _ = self.changed.notified() => break Ended::Changed,
                message = polled.recv() => match message {
                    Some(Polled::Connected) => {
                        connected = true;
                        self.set_status("connected", None, broker.clone());
                        tracing::info!(broker = ?broker, "MQTT connected");
                        publisher.announce(state, hub).await;
                    }
                    Some(Polled::Failed(detail)) => {
                        if connected {
                            tracing::warn!(%detail, "MQTT connection lost; reconnecting");
                        }
                        connected = false;
                        self.set_status("error", Some(detail), broker.clone());
                    }
                    None => break Ended::Changed,
                },
                notice = notices.recv() => match notice {
                    Ok(notice) if connected => publisher.event(state, &notice.event, notice.phase).await,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "MQTT fell behind the events; sensors are refreshed");
                        if connected {
                            publisher.announce(state, hub).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break Ended::Shutdown,
                },
                _ = cameras.tick() => if connected {
                    publisher.cameras_changed(state).await;
                },
            }
        };
        if connected {
            // Say goodbye: a clean stop is not a lost recorder.
            send(&client, publisher.topics.availability(), true, OFFLINE).await;
            let _ = tokio::time::timeout(PUBLISH_WAIT, client.disconnect()).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        poller.abort();
        ended
    }

    /// Try to connect with these settings (`mqtt.test`), without disturbing
    /// the running connection.
    pub async fn test(config: &MqttConfig) -> Result<(), String> {
        let topics = Topics::new(&config.topic_prefix, &config.discovery_prefix);
        let options = options(config, &topics, "-test")?;
        let (client, mut eventloop) = AsyncClient::new(options, 4);
        let result = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => return Ok(()),
                    Ok(_) => {}
                    Err(error) => return Err(describe(&error)),
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err("The broker did not answer within 8 seconds".into()));
        if result.is_ok() {
            let _ = client.try_disconnect();
            let _ = tokio::time::timeout(Duration::from_millis(500), eventloop.poll()).await;
        }
        result
    }
}

/// What has been published on this connection, to publish only changes.
struct Publisher {
    client: AsyncClient,
    topics: Topics,
    discovery: bool,
    sensors: Sensors,
    announced: BTreeSet<(String, Sensor)>,
    /// Camera id → name, as announced.
    cameras: BTreeMap<String, String>,
}

impl Publisher {
    /// Everything, after a (re)connection: availability, discovery, states.
    async fn announce(&mut self, state: &Arc<RwLock<AppState>>, hub: &Arc<EventHub>) {
        let (cameras, site, db) = {
            let st = state.read().await;
            let cameras: BTreeMap<String, String> = st
                .config
                .cameras
                .iter()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect();
            (cameras, st.config.site_name.clone(), Arc::clone(&st.db))
        };
        self.sensors = Sensors::from_open(&hub.open_events());
        let seen = seen_sensors(db).await;
        send(&self.client, self.topics.availability(), true, ONLINE).await;
        if self.discovery {
            let (topic, document) = self
                .topics
                .recorder_config(&site, env!("CARGO_PKG_VERSION"));
            send(&self.client, topic, true, document.to_string()).await;
        }
        send(
            &self.client,
            self.topics.problem(),
            true,
            payload(self.sensors.problem()),
        )
        .await;
        self.announced.clear();
        self.cameras.clear();
        for (camera, name) in &cameras {
            self.camera(camera, name, &seen).await;
        }
        self.cameras = cameras;
    }

    /// A camera's sensors (always-on ones, and those it has reported), its
    /// picture entity and their states.
    async fn camera(&mut self, camera: &str, name: &str, seen: &BTreeSet<(String, Sensor)>) {
        if self.discovery {
            let (topic, document) = self.topics.picture_config(camera, name);
            send(&self.client, topic, true, document.to_string()).await;
        }
        for sensor in Sensor::ALL {
            let wanted = sensor.always()
                || seen.contains(&(camera.to_owned(), sensor))
                || self.sensors.is_on(camera, sensor) && sensor != Sensor::Online;
            if wanted {
                self.sensor(camera, name, sensor).await;
            }
        }
    }

    async fn sensor(&mut self, camera: &str, name: &str, sensor: Sensor) {
        if self.discovery {
            let (topic, document) = self.topics.sensor_config(camera, name, sensor);
            send(&self.client, topic, true, document.to_string()).await;
        }
        let on = self.sensors.is_on(camera, sensor);
        send(
            &self.client,
            self.topics.state(camera, sensor),
            true,
            payload(on),
        )
        .await;
        self.announced.insert((camera.to_owned(), sensor));
    }

    async fn event(
        &mut self,
        state: &Arc<RwLock<AppState>>,
        event: &nw_core::event::EventRecord,
        phase: EventPhase,
    ) {
        if phase == EventPhase::Update {
            return;
        }
        let camera_name = match event.camera_id.as_deref() {
            Some(camera) => self.cameras.get(camera).cloned(),
            None => None,
        };
        let message = discovery::event_message(event, phase, camera_name.as_deref());
        send(
            &self.client,
            self.topics.events(),
            false,
            message.to_string(),
        )
        .await;
        for (sensor, on) in self.sensors.apply(event, phase) {
            match sensor {
                None => send(&self.client, self.topics.problem(), true, payload(on)).await,
                Some((camera, sensor)) => {
                    if !self.announced.contains(&(camera.clone(), sensor)) {
                        let name = camera_name.clone().unwrap_or_else(|| camera.clone());
                        // Announced the first time the camera reports it.
                        self.sensor(&camera, &name, sensor).await;
                    } else {
                        let on = self.sensors.is_on(&camera, sensor);
                        send(
                            &self.client,
                            self.topics.state(&camera, sensor),
                            true,
                            payload(on),
                        )
                        .await;
                    }
                }
            }
        }
        if phase == EventPhase::Start && event.kind.is_activity() {
            if let Some(camera) = event.camera_id.clone() {
                let (client, topic, state, id) = (
                    self.client.clone(),
                    self.topics.picture(&camera),
                    Arc::clone(state),
                    event.id,
                );
                tokio::spawn(async move {
                    if let Some(jpeg) = crate::alerts::picture(&state, id).await {
                        send(&client, topic, true, jpeg).await;
                    }
                });
            }
        }
    }

    /// Announce cameras added or renamed, and remove deleted ones from Home
    /// Assistant.
    async fn cameras_changed(&mut self, state: &Arc<RwLock<AppState>>) {
        let current: BTreeMap<String, String> = state
            .read()
            .await
            .config
            .cameras
            .iter()
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();
        if current == self.cameras {
            return;
        }
        let db = Arc::clone(&state.read().await.db);
        let seen = seen_sensors(db).await;
        for (camera, name) in &current {
            if self.cameras.get(camera) != Some(name) {
                self.announced.retain(|(c, _)| c != camera);
                self.camera(camera, name, &seen).await;
            }
        }
        for camera in self.cameras.keys().filter(|c| !current.contains_key(*c)) {
            if self.discovery {
                for topic in self.topics.camera_config_topics(camera) {
                    send(&self.client, topic, true, Vec::<u8>::new()).await;
                }
            }
            for sensor in Sensor::ALL {
                send(
                    &self.client,
                    self.topics.state(camera, sensor),
                    true,
                    Vec::<u8>::new(),
                )
                .await;
            }
            send(
                &self.client,
                self.topics.picture(camera),
                true,
                Vec::<u8>::new(),
            )
            .await;
        }
        self.announced.retain(|(c, _)| current.contains_key(c));
        self.cameras = current;
    }
}

/// Sensors each camera reported recently (person, vehicle, animal): those
/// are announced at connection even before their next event.
async fn seen_sensors(
    db: Arc<std::sync::Mutex<rusqlite::Connection>>,
) -> BTreeSet<(String, Sensor)> {
    let since = crate::events::now_ms() - SEEN_WITHIN_MS;
    tokio::task::spawn_blocking(move || {
        let Ok(conn) = db.lock() else {
            return BTreeSet::new();
        };
        let Ok(mut statement) = conn.prepare(
            "SELECT DISTINCT camera_id, event_type FROM events \
             WHERE camera_id IS NOT NULL AND event_type IN ('person','vehicle','animal') AND ts >= ?1",
        ) else {
            return BTreeSet::new();
        };
        let rows = statement.query_map([since], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        rows.map(|rows| {
            rows.filter_map(Result::ok)
                .filter_map(|(camera, kind)| Some((camera, Sensor::from_kind_name(&kind)?)))
                .collect()
        })
        .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

async fn send(client: &AsyncClient, topic: String, retain: bool, payload: impl Into<Vec<u8>>) {
    let publish = client.publish(topic, QoS::AtLeastOnce, retain, payload.into());
    match tokio::time::timeout(PUBLISH_WAIT, publish).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "MQTT publish failed"),
        Err(_) => tracing::debug!("MQTT publish dropped: the broker is not keeping up"),
    }
}

fn options(config: &MqttConfig, topics: &Topics, suffix: &str) -> Result<MqttOptions, String> {
    let host = config
        .host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
        ^ std::process::id();
    let client_id = format!(
        "{}-{:06x}{suffix}",
        discovery::slug(&config.topic_prefix),
        nonce & 0xff_ffff
    );
    let mut options = MqttOptions::new(client_id, host, config.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_max_packet_size(MAX_PACKET, MAX_PACKET);
    options.set_last_will(LastWill::new(
        topics.availability(),
        OFFLINE,
        QoS::AtLeastOnce,
        true,
    ));
    if !config.username.is_empty() {
        options.set_credentials(config.username.clone(), config.password.clone());
    }
    if config.tls {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS could not be set up".to_owned())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        options.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
            Arc::new(tls),
        )));
    }
    Ok(options)
}

/// A connection failure in words a person can act on.
fn describe(error: &ConnectionError) -> String {
    match error {
        ConnectionError::ConnectionRefused(code) => match code {
            ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized => {
                "The broker refused the user name or password".into()
            }
            other => format!("The broker refused the connection ({other:?})"),
        },
        ConnectionError::Io(error) => format!("Could not reach the broker: {error}"),
        ConnectionError::Tls(error) => format!("Secure connection failed: {error}"),
        ConnectionError::NetworkTimeout => "The broker did not answer".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod test_broker;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_broker_that_is_not_there_is_reported_plainly() {
        let config = MqttConfig {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 1, // nothing listens there
            ..MqttConfig::default()
        };
        let error = MqttService::test(&config).await.unwrap_err();
        assert!(error.starts_with("Could not reach the broker"), "{error}");
    }

    /// Against a real broker (`NW_MQTT_TEST_BROKER=127.0.0.1:1883`, e.g.
    /// `docker run --rm -p 127.0.0.1:1883:1883 eclipse-mosquitto:2 mosquitto -c /mosquitto-no-auth.conf`):
    /// discovery, availability, a person event on and off, the event stream,
    /// and `offline` on a clean stop.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_real_broker_receives_discovery_states_events_and_goodbye() {
        let Ok(broker) = std::env::var("NW_MQTT_TEST_BROKER") else {
            eprintln!("NW_MQTT_TEST_BROKER is not set: skipped");
            return;
        };
        let (host, port) = broker.rsplit_once(':').unwrap();
        discovery_states_events_and_goodbye(host, port).await;
    }

    /// The same, against the tests' own small broker (`test_broker.rs`), so
    /// it always runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_broker_receives_discovery_states_events_and_goodbye() {
        let port = super::test_broker::start().await;
        discovery_states_events_and_goodbye("127.0.0.1", &port.to_string()).await;
    }

    #[tokio::test]
    async fn testing_the_settings_against_a_broker_succeeds() {
        let port = super::test_broker::start().await;
        let config = MqttConfig {
            enabled: true,
            host: "127.0.0.1".into(),
            port,
            ..MqttConfig::default()
        };
        MqttService::test(&config).await.unwrap();
    }

    async fn discovery_states_events_and_goodbye(host: &str, port: &str) {
        let prefix = format!("nwtest{}", std::process::id());
        let mut config = crate::config::AppConfig::default();
        let mut camera =
            nw_core::camera::CameraConfig::new("Front door", "rtsp://example.invalid/s");
        camera.id = "cam-1".into();
        config.cameras.push(camera);
        config.mqtt = MqttConfig {
            enabled: true,
            host: host.into(),
            port: port.parse().unwrap(),
            topic_prefix: prefix.clone(),
            discovery_prefix: format!("{prefix}-ha"),
            ..MqttConfig::default()
        };
        let path = std::env::temp_dir().join(format!("{prefix}.json"));
        let state = Arc::new(RwLock::new(AppState::with_database(
            config,
            path.clone(),
            crate::pipeline::hwaccel::HwAccelKind::Software,
            crate::db::connection::open_in_memory().unwrap(),
        )));
        let (hub, service) = {
            let st = state.read().await;
            (Arc::clone(&st.events), Arc::clone(&st.mqtt))
        };

        // A listener, as Home Assistant would be.
        let mut options =
            MqttOptions::new(format!("{prefix}-listener"), host, port.parse().unwrap());
        options.set_max_packet_size(MAX_PACKET, MAX_PACKET);
        let (listener, mut incoming) = AsyncClient::new(options, 16);
        listener
            .subscribe(format!("{prefix}/#"), QoS::AtLeastOnce)
            .await
            .unwrap();
        listener
            .subscribe(format!("{prefix}-ha/#"), QoS::AtLeastOnce)
            .await
            .unwrap();
        let (seen_tx, mut seen) = mpsc::unbounded_channel::<(String, String)>();
        tokio::spawn(async move {
            while let Ok(event) = incoming.poll().await {
                if let Event::Incoming(Packet::Publish(publish)) = event {
                    let _ = seen_tx.send((
                        publish.topic.clone(),
                        String::from_utf8_lossy(&publish.payload).into_owned(),
                    ));
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (shutdown, _) = broadcast::channel(1);
        let running = tokio::spawn(Arc::clone(&service).run(
            Arc::clone(&state),
            Arc::clone(&hub),
            shutdown.subscribe(),
        ));
        let mut log: Vec<(String, String)> = Vec::new();
        async fn until(
            seen: &mut mpsc::UnboundedReceiver<(String, String)>,
            log: &mut Vec<(String, String)>,
            topic: &str,
            test: impl Fn(&str) -> bool,
        ) -> String {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some((_, p)) = log.iter().find(|(t, p)| t == topic && test(p)) {
                    return p.clone();
                }
                match tokio::time::timeout_at(deadline, seen.recv()).await {
                    Ok(Some(message)) => log.push(message),
                    _ => panic!("nothing on {topic}; saw {log:?}"),
                }
            }
        }

        until(&mut seen, &mut log, &format!("{prefix}/status"), |p| {
            p == ONLINE
        })
        .await;
        let config_topic =
            format!("{prefix}-ha/binary_sensor/neurowatch_{prefix}_cam-1_motion/config");
        let document = until(&mut seen, &mut log, &config_topic, |_| true).await;
        let document: serde_json::Value = serde_json::from_str(&document).unwrap();
        assert_eq!(document["device"]["name"], "Front door");
        until(
            &mut seen,
            &mut log,
            &format!("{prefix}/cam-1/online"),
            |p| p == "ON",
        )
        .await;
        assert_eq!(service.status().state, "connected");

        let event = hub.start(crate::events::NewEvent::new(
            Some("cam-1".into()),
            nw_core::event::EventKind::Person,
            nw_core::event::EventSource::Plugin,
        ));
        until(
            &mut seen,
            &mut log,
            &format!("{prefix}/cam-1/person"),
            |p| p == "ON",
        )
        .await;
        let person_config =
            format!("{prefix}-ha/binary_sensor/neurowatch_{prefix}_cam-1_person/config");
        until(&mut seen, &mut log, &person_config, |_| true).await;
        let started = until(&mut seen, &mut log, &format!("{prefix}/events"), |p| {
            p.contains("\"start\"")
        })
        .await;
        assert!(
            started.contains("\"person\"") && started.contains("Front door"),
            "{started}"
        );
        hub.end(event.id);
        until(
            &mut seen,
            &mut log,
            &format!("{prefix}/cam-1/person"),
            |p| p == "OFF",
        )
        .await;

        shutdown.send(()).unwrap();
        running.await.unwrap();
        until(&mut seen, &mut log, &format!("{prefix}/status"), |p| {
            p == OFFLINE
        })
        .await;

        // Clear the retained messages this test left on the broker.
        let mut retained: BTreeSet<String> = log.iter().map(|(t, _)| t.clone()).collect();
        retained.remove(&format!("{prefix}/events"));
        for topic in retained {
            listener
                .publish(topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = std::fs::remove_file(path);
    }
}
