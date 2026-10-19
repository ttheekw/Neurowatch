//! Shared camera streams: one camera connection per stream, however many
//! parts of the app use it (`docs/plans/2026-10-17-shared-camera-streams.md`).
//!
//! A [`StreamHubs`] registry keeps one hub per resolved source (camera, URL,
//! credentials, transport). A hub receives the camera's compressed video
//! (and sound) on one connection ([`source`]) and hands each unit, by
//! reference, to every [`Subscription`]. Each subscriber has its own bounded
//! queue and overflow rule ([`SubscriberPolicy`]), so one that falls behind
//! loses only its own units. A hub opens its connection with the first
//! subscriber, reconnects on its own with backoff while anyone subscribes,
//! and closes when the last subscriber left and its linger passed.
//!
//! Nothing here decodes: decoding stays with desktop viewing and opt-in
//! analysis (see the performance rules in `CLAUDE.md`).
//!
//! Subscribers so far: the recorder (`pipeline::ingest`), and through
//! [`crate::app_state::AppState::subscribe_stream`] extensions (the cloud's
//! browser live view). [`annexb`] turns video units into what WebRTC sends,
//! [`sound`] decodes sound units for listening.

pub mod annexb;
pub mod core;
pub(crate) mod decoded;
pub mod feed;
pub mod queue;
pub mod sound;
mod source;
#[cfg(test)]
pub(crate) mod testing;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use nw_core::camera::{CameraConfig, Transport};

use crate::pipeline::reachability::Reachability;
pub use queue::{Delivery, Media, Overflow, SubscriberPolicy, Unit};

/// Whether camera media goes through shared connections. On unless
/// `NW_STREAM_HUB=0`: every user then opens a connection of its own, as
/// before (kept as a fallback while the shared one is qualified).
pub fn enabled() -> bool {
    std::env::var("NW_STREAM_HUB").map_or(true, |value| value.trim() != "0")
}

/// Whether `camera`'s source can be shared (it has compressed video).
pub fn shareable(camera: &CameraConfig) -> bool {
    source::description(camera).is_some()
}

/// One compressed unit from a camera: a video access unit or an audio frame,
/// as parsed (never decoded). Cloning is a reference-count increment.
#[derive(Clone)]
pub struct SharedUnit {
    pub media: Media,
    pub buffer: gstreamer::Buffer,
    pub caps: gstreamer::Caps,
    /// When it reached the hub, µs since the Unix epoch.
    pub arrived_us: i64,
}

impl Unit for SharedUnit {
    fn media(&self) -> Media {
        self.media
    }
    fn keyframe(&self) -> bool {
        self.media == Media::Video
            && !self
                .buffer
                .flags()
                .contains(gstreamer::BufferFlags::DELTA_UNIT)
    }
    fn bytes(&self) -> usize {
        self.buffer.size()
    }
    fn time_ns(&self) -> Option<u64> {
        self.buffer
            .pts()
            .or_else(|| self.buffer.dts())
            .map(gstreamer::ClockTime::nseconds)
    }
}

/// Where a hub's connection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Streaming,
    /// The connection failed; waiting out a backoff before the next attempt.
    Retrying,
    Closed,
}

/// A hub's connection state, as its subscribers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubState {
    pub phase: Phase,
    /// Consecutive failed attempts (reset by an attempt that delivered video).
    pub failures: u32,
    /// Why the last attempt failed.
    pub reason: Option<String>,
    /// Whether that attempt had delivered video before failing.
    pub delivered: bool,
    /// Increases with every change, so a subscriber can tell one apart.
    pub generation: u64,
}

/// What identifies a camera connection. Not `Debug`: it holds credentials.
#[derive(Clone, PartialEq, Eq, Hash)]
struct SourceKey {
    camera_id: String,
    url: String,
    username: Option<String>,
    password_b64: Option<String>,
    tcp: bool,
}

impl SourceKey {
    fn of(camera: &CameraConfig) -> Self {
        Self {
            camera_id: camera.id.clone(),
            url: camera.url.clone(),
            username: camera.username.clone(),
            password_b64: camera.password_b64.clone(),
            tcp: matches!(camera.transport, Transport::Tcp),
        }
    }
}

type HubMap = Mutex<HashMap<SourceKey, Arc<Hub>>>;

/// One camera connection and its subscribers.
pub struct Hub {
    key: SourceKey,
    camera: CameraConfig,
    registry: Weak<HubMap>,
    core: Mutex<core::HubCore<SharedUnit>>,
    decoded: Arc<decoded::Channel>,
    state: Mutex<HubState>,
    /// The camera's audio track label once described (`Some(None)`: none).
    audio_label: Mutex<Option<Option<String>>>,
    /// Since when nobody subscribes, and how long to stay open then.
    idle: Mutex<Option<(Instant, Duration)>>,
    closed: AtomicBool,
    /// The registry is shutting down: close at once.
    shutdown: Arc<AtomicBool>,
    reachability: Option<Arc<Reachability>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl Hub {
    pub fn camera_id(&self) -> &str {
        &self.key.camera_id
    }

    pub fn state(&self) -> HubState {
        lock(&self.state).clone()
    }

    fn set_state(&self, phase: Phase, failures: u32, reason: Option<String>, delivered: bool) {
        let mut state = lock(&self.state);
        let generation = state.generation + 1;
        *state = HubState {
            phase,
            failures,
            reason,
            delivered,
            generation,
        };
    }

    fn publish(&self, unit: SharedUnit) {
        lock(&self.core).publish(unit);
    }

    fn leave(&self, id: u64) {
        let mut core = lock(&self.core);
        if let Some(policy) = core.unsubscribe(id) {
            if core.subscriber_count() == 0 {
                *lock(&self.idle) = Some((Instant::now(), policy.linger));
            }
        }
    }

    /// Close if nobody subscribes and the linger passed (or the registry
    /// shuts down). Decided under the registry lock, so a subscriber that
    /// arrives at the same moment gets a new hub instead of a closed one.
    fn try_close(self: &Arc<Self>) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return true;
        }
        let Some(registry) = self.registry.upgrade() else {
            self.close_now();
            return true;
        };
        let mut hubs = lock(&registry);
        if !self.shutdown.load(Ordering::SeqCst) {
            if lock(&self.core).subscriber_count() > 0 {
                return false;
            }
            if let Some((since, linger)) = *lock(&self.idle) {
                if since.elapsed() < linger {
                    return false;
                }
            }
        }
        if hubs
            .get(&self.key)
            .is_some_and(|hub| Arc::ptr_eq(hub, self))
        {
            hubs.remove(&self.key);
        }
        drop(hubs);
        self.close_now();
        true
    }

    fn close_now(&self) {
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.core).close();
        let failures = lock(&self.state).failures;
        self.set_state(Phase::Closed, failures, None, false);
    }
}

/// A subscriber's handle; dropping it unsubscribes.
pub struct Subscription {
    hub: Arc<Hub>,
    id: u64,
    queue: Arc<queue::SubscriberQueue<SharedUnit>>,
}

impl Subscription {
    /// The next unit or discontinuity, waiting up to `timeout`; `None` when
    /// nothing came (or the hub closed: see [`Subscription::state`]).
    pub fn recv(&self, timeout: Duration) -> Option<Delivery<SharedUnit>> {
        self.queue.pop(timeout)
    }

    pub fn state(&self) -> HubState {
        self.hub.state()
    }

    pub(crate) fn decoded(&self) -> Arc<decoded::Channel> {
        Arc::clone(&self.hub.decoded)
    }

    /// Resume decoding from the cached GOP rather than an old, full queue.
    pub(crate) fn restart_video(&mut self) {
        let policy = self.queue.policy();
        let (id, queue) = lock(&self.hub.core).subscribe(policy);
        self.hub.leave(self.id);
        self.id = id;
        self.queue = queue;
    }

    /// The camera's audio track label, once its stream was described.
    pub fn audio_label(&self) -> Option<Option<String>> {
        lock(&self.hub.audio_label).clone()
    }

    /// Units this subscriber lost by falling behind.
    pub fn dropped(&self) -> u64 {
        self.queue.dropped()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.leave(self.id);
    }
}

/// Every open camera connection, one per resolved source.
#[derive(Default)]
pub struct StreamHubs {
    hubs: Arc<HubMap>,
    shutdown: Arc<AtomicBool>,
}

impl StreamHubs {
    /// Subscribe to `camera`'s stream (its `url`), opening the connection if
    /// none is open. `reachability` lets a hub waiting out a backoff retry at
    /// once when the camera answered elsewhere (its first subscriber's is used).
    pub fn subscribe(
        &self,
        camera: &CameraConfig,
        policy: SubscriberPolicy,
        reachability: Option<Arc<Reachability>>,
    ) -> Result<Subscription, String> {
        let key = SourceKey::of(camera);
        let mut hubs = lock(&self.hubs);
        if self.shutdown.load(Ordering::SeqCst) {
            return Err("shutting down".into());
        }
        let hub = match hubs.get(&key) {
            Some(hub) if !hub.closed.load(Ordering::SeqCst) => Arc::clone(hub),
            _ => {
                let hub = Arc::new(Hub {
                    key: key.clone(),
                    camera: camera.clone(),
                    registry: Arc::downgrade(&self.hubs),
                    core: Mutex::default(),
                    decoded: Arc::default(),
                    state: Mutex::new(HubState {
                        phase: Phase::Connecting,
                        failures: 0,
                        reason: None,
                        delivered: false,
                        generation: 0,
                    }),
                    audio_label: Mutex::new(None),
                    idle: Mutex::new(None),
                    closed: AtomicBool::new(false),
                    shutdown: Arc::clone(&self.shutdown),
                    reachability,
                });
                let runner = Arc::clone(&hub);
                std::thread::Builder::new()
                    .name(format!("hub-{}", camera.id))
                    .spawn(move || source::run(runner))
                    .map_err(|error| format!("could not start the camera connection: {error}"))?;
                hubs.insert(key, Arc::clone(&hub));
                tracing::debug!(cam_id = %camera.id, "Camera connection opened for its first subscriber");
                hub
            }
        };
        let (id, queue) = lock(&hub.core).subscribe(policy);
        *lock(&hub.idle) = None;
        drop(hubs);
        Ok(Subscription { hub, id, queue })
    }

    /// Open camera connections.
    pub fn open_count(&self) -> usize {
        lock(&self.hubs).len()
    }

    /// Open connections and their subscribers, per camera.
    pub fn summary(&self) -> Vec<(String, usize)> {
        lock(&self.hubs)
            .values()
            .map(|hub| {
                (
                    hub.camera_id().to_owned(),
                    lock(&hub.core).subscriber_count(),
                )
            })
            .collect()
    }

    /// Close every connection now (backend shutdown).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let hubs: Vec<_> = lock(&self.hubs).drain().map(|(_, hub)| hub).collect();
        for hub in hubs {
            hub.close_now();
        }
    }
}
