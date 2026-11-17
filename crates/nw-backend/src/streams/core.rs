//! The part of a hub that does not touch GStreamer: its subscribers, the
//! fan-out of each unit to them, and the cache of the current GOP.
//!
//! A unit is handed to every subscriber by reference (`Clone` of a GStreamer
//! buffer is a reference-count increment), so fan-out copies no media. The
//! GOP cache holds the units since the last keyframe, so a subscriber that
//! joins mid-stream starts on a keyframe at once instead of waiting for the
//! camera's next one.

use super::queue::ConsumerKind;
use super::rates::{RateSample, Rates};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use super::queue::{Media, SubscriberPolicy, SubscriberQueue, Unit};

/// Most a GOP cache holds; a longer GOP is not cached (joiners then wait
/// for the next keyframe, as without a cache).
pub const GOP_CACHE_BYTES: usize = 16 * 1024 * 1024;
pub const GOP_CACHE_UNITS: usize = 1024;

pub struct HubCore<U> {
    rates: Rates,
    subscribers: Vec<(u64, Arc<SubscriberQueue<U>>)>,
    next_id: u64,
    gop: Vec<U>,
    gop_bytes: usize,
    /// The cache overflowed: stays empty until the next keyframe.
    gop_overflowed: bool,
    /// The video codec of the latest keyframe (diagnostics).
    codec: Option<&'static str>,
    /// The picture size of the latest keyframe.
    size: Option<(u32, u32)>,
}

impl<U: Unit> Default for HubCore<U> {
    fn default() -> Self {
        Self {
            rates: Rates::new(Instant::now()),
            subscribers: Vec::new(),
            next_id: 1,
            gop: Vec::new(),
            gop_bytes: 0,
            gop_overflowed: false,
            codec: None,
            size: None,
        }
    }
}

impl<U: Unit> HubCore<U> {
    /// A new subscriber; it first receives the cached GOP.
    pub fn subscribe(&mut self, policy: SubscriberPolicy) -> (u64, Arc<SubscriberQueue<U>>) {
        let queue = Arc::new(SubscriberQueue::new(policy));
        for unit in &self.gop {
            queue.push(unit.clone());
        }
        let id = self.next_id;
        self.next_id += 1;
        self.subscribers.push((id, Arc::clone(&queue)));
        (id, queue)
    }

    /// Remove a subscriber; its policy, if it was there.
    pub fn unsubscribe(&mut self, id: u64) -> Option<SubscriberPolicy> {
        let at = self.subscribers.iter().position(|(sid, _)| *sid == id)?;
        let (_, queue) = self.subscribers.remove(at);
        queue.close();
        Some(queue.policy())
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// A unit from the camera: cache it, hand it to every subscriber.
    pub fn publish(&mut self, unit: U) {
        self.rates
            .record(unit.media() == Media::Video, unit.bytes(), Instant::now());
        self.cache(&unit);
        for (_, queue) in &self.subscribers {
            queue.push(unit.clone());
        }
    }

    fn cache(&mut self, unit: &U) {
        if unit.media() == Media::Video && unit.keyframe() {
            // Once a GOP, and only a caps name: cheap on the source's thread.
            if let Some(codec) = unit.video_codec() {
                self.codec = Some(codec);
            }
            if let Some(size) = unit.video_size() {
                self.size = Some(size);
            }
            self.gop.clear();
            self.gop_bytes = 0;
            self.gop_overflowed = false;
        } else if self.gop.is_empty() || self.gop_overflowed {
            // No keyframe yet (or the GOP outgrew the cache): nothing a
            // joiner could start from.
            return;
        }
        if self.gop.len() >= GOP_CACHE_UNITS || self.gop_bytes + unit.bytes() > GOP_CACHE_BYTES {
            self.gop.clear();
            self.gop_bytes = 0;
            self.gop_overflowed = true;
            return;
        }
        self.gop_bytes += unit.bytes();
        self.gop.push(unit.clone());
    }

    /// The camera connection was lost: the cache is stale, and every
    /// subscriber learns that what follows is a new stream.
    pub fn disconnect(&mut self, reason: &str) {
        self.gop.clear();
        self.gop_bytes = 0;
        self.gop_overflowed = false;
        for (_, queue) in &self.subscribers {
            queue.push_discontinuity(reason);
        }
    }

    /// The hub closes: no more units for anyone.
    pub fn close(&mut self) {
        self.gop.clear();
        self.gop_bytes = 0;
        for (_, queue) in self.subscribers.drain(..) {
            queue.close();
        }
    }

    /// Read-only: multiple diagnostics viewers never reset each other's rates.
    pub fn metrics(&self, now: Instant) -> (BTreeMap<ConsumerKind, u32>, Option<RateSample>, u64) {
        let mut roles = BTreeMap::new();
        let mut drops = 0u64;
        for (_, queue) in &self.subscribers {
            *roles.entry(queue.policy().consumer).or_insert(0u32) += 1;
            drops = drops.saturating_add(queue.dropped());
        }
        (roles, self.rates.snapshot(now), drops)
    }

    /// Units and bytes in the GOP cache.
    pub fn cached(&self) -> (usize, usize) {
        (self.gop.len(), self.gop_bytes)
    }

    /// The video codec of the latest keyframe (`h264`, `h265`), if known.
    pub fn codec(&self) -> Option<&'static str> {
        self.codec
    }

    /// The picture size of the latest keyframe, if the parser reported it.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.size
    }

    /// Video bits a second over the last minute of streaming, once measured.
    pub fn average_video_bps(&self) -> Option<u64> {
        self.rates.average_video_bps()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::queue::tests::{audio, video, Fake};
    use super::super::queue::{Delivery, Overflow};
    use super::*;

    /// A unit that names its codec, as camera units do.
    #[derive(Clone)]
    struct Coded(Fake, Option<&'static str>);
    impl Unit for Coded {
        fn media(&self) -> Media {
            self.0.media()
        }
        fn keyframe(&self) -> bool {
            self.0.keyframe()
        }
        fn bytes(&self) -> usize {
            self.0.bytes()
        }
        fn time_ns(&self) -> Option<u64> {
            self.0.time_ns()
        }
        fn video_codec(&self) -> Option<&'static str> {
            self.1
        }
        fn video_size(&self) -> Option<(u32, u32)> {
            // The codec stands in for a size: h265 pictures are 4K here.
            self.1.map(|codec| {
                if codec == "h265" {
                    (3840, 2160)
                } else {
                    (1920, 1080)
                }
            })
        }
    }

    #[test]
    fn the_codec_is_the_latest_keyframes() {
        let mut core = HubCore::<Coded>::default();
        assert_eq!(core.codec(), None);
        core.publish(Coded(video(0, true), Some("h264")));
        assert_eq!(core.codec(), Some("h264"));
        // Read on keyframes only; a delta unit never changes it.
        core.publish(Coded(video(40, false), Some("h265")));
        assert_eq!(core.codec(), Some("h264"));
        core.publish(Coded(video(80, true), Some("h265")));
        assert_eq!(core.codec(), Some("h265"), "the camera changed codec");
        core.publish(Coded(audio(90), None));
        assert_eq!(core.codec(), Some("h265"));
    }

    #[test]
    fn the_size_is_the_latest_keyframes() {
        let mut core = HubCore::<Coded>::default();
        assert_eq!(core.size(), None);
        core.publish(Coded(video(0, true), Some("h264")));
        assert_eq!(core.size(), Some((1920, 1080)));
        core.publish(Coded(video(40, false), Some("h265")));
        assert_eq!(core.size(), Some((1920, 1080)), "read on keyframes only");
        core.publish(Coded(video(80, true), Some("h265")));
        assert_eq!(
            core.size(),
            Some((3840, 2160)),
            "the camera changed resolution"
        );
    }

    fn viewer() -> SubscriberPolicy {
        SubscriberPolicy {
            consumer: ConsumerKind::Desktop,
            max_bytes: 1_000_000,
            max_units: 3,
            max_span: Duration::from_secs(10),
            overflow: Overflow::SkipToKeyframe,
            start_at_keyframe: true,
            linger: Duration::from_secs(10),
            media: None,
        }
    }

    fn units(queue: &SubscriberQueue<Fake>) -> Vec<u64> {
        std::iter::from_fn(|| queue.pop(Duration::ZERO))
            .filter_map(|item| match item {
                Delivery::Unit { unit, .. } => Some(unit.ms),
                Delivery::Discontinuity { .. } => None,
            })
            .collect()
    }

    #[test]
    fn diagnostics_count_subscription_roles_without_changing_delivery() {
        let mut hub = HubCore::default();
        let (recording, recorder) = hub.subscribe(SubscriberPolicy::recorder());
        let (plugin, _) = hub.subscribe(SubscriberPolicy::plugin());
        let (browser, _) = hub.subscribe(SubscriberPolicy::live_video());
        hub.publish(video(0, true));
        let (roles, _, drops) = hub.metrics(Instant::now());
        assert_eq!(roles[&ConsumerKind::Recording], 1);
        assert_eq!(roles[&ConsumerKind::Plugin], 1);
        assert_eq!(roles[&ConsumerKind::Browser], 1);
        assert_eq!(drops, 0);
        assert_eq!(hub.cached(), (1, 100));
        assert_eq!(
            units(&recorder),
            vec![0],
            "diagnostics never drain subscriber queues"
        );
        hub.unsubscribe(plugin);
        hub.unsubscribe(browser);
        hub.unsubscribe(recording);
        assert!(hub.metrics(Instant::now()).0.is_empty());
        assert_eq!(
            SubscriberPolicy::snapshot().consumer,
            ConsumerKind::Snapshot
        );
        assert_eq!(SubscriberPolicy::live_sound().consumer, ConsumerKind::Audio);
        assert_eq!(
            SubscriberPolicy::plugin_reliable(1000).consumer,
            ConsumerKind::Plugin
        );
    }

    #[test]
    fn a_late_joiner_starts_at_the_cached_keyframe() {
        let mut hub = HubCore::default();
        hub.publish(video(0, false)); // before any keyframe: not cached
        hub.publish(video(40, true));
        hub.publish(audio(50));
        hub.publish(video(80, false));
        let (_, late) = hub.subscribe(SubscriberPolicy::recorder());
        hub.publish(video(120, false));
        assert_eq!(units(&late), vec![40, 50, 80, 120]);
    }

    #[test]
    fn the_cache_restarts_at_each_keyframe_and_is_bounded() {
        let mut hub = HubCore::default();
        hub.publish(video(0, true));
        hub.publish(video(40, false));
        hub.publish(video(80, true));
        assert_eq!(hub.cached(), (1, 100), "only the current GOP");
        for i in 0..GOP_CACHE_UNITS as u64 {
            hub.publish(video(100 + i, false));
        }
        assert_eq!(hub.cached(), (0, 0), "a GOP too long to cache is dropped");
        hub.publish(video(5000, false));
        assert_eq!(
            hub.cached(),
            (0, 0),
            "and stays out until the next keyframe"
        );
        hub.publish(video(6000, true));
        assert_eq!(hub.cached(), (1, 100));
    }

    #[test]
    fn a_slow_viewer_loses_its_own_units_while_the_recorder_gets_everything() {
        let mut hub = HubCore::default();
        let (_, recorder) = hub.subscribe(SubscriberPolicy::recorder());
        let (_, viewer) = hub.subscribe(viewer());
        let sent: Vec<u64> = (0..20).map(|i| i * 40).collect();
        for &ms in &sent {
            hub.publish(video(ms, ms % 400 == 0));
        }
        assert_eq!(units(&recorder), sent, "nothing lost");
        assert_eq!(
            units(&viewer),
            vec![0, 40, 80],
            "the viewer never read: full"
        );
        assert!(viewer.dropped() > 0);
        assert_eq!(recorder.dropped(), 0);
    }

    #[test]
    fn a_disconnect_clears_the_cache_and_tells_every_subscriber() {
        let mut hub = HubCore::default();
        let (_, a) = hub.subscribe(viewer());
        hub.publish(video(0, true));
        hub.disconnect("Stream ended (EOS)");
        assert_eq!(hub.cached(), (0, 0));
        let (_, b) = hub.subscribe(viewer());
        hub.publish(video(10, false));
        hub.publish(video(50, true));
        let first: Vec<_> = std::iter::from_fn(|| a.pop(Duration::ZERO)).collect();
        assert!(matches!(first[1], Delivery::Discontinuity { .. }));
        assert_eq!(
            units(&b),
            vec![50],
            "the new joiner waits for the new stream's keyframe"
        );
    }

    #[test]
    fn unsubscribing_closes_that_queue_only() {
        let mut hub: HubCore<Fake> = HubCore::default();
        let (a_id, a) = hub.subscribe(viewer());
        let (_, b) = hub.subscribe(SubscriberPolicy::recorder());
        assert_eq!(hub.unsubscribe(a_id), Some(viewer()));
        assert_eq!(hub.unsubscribe(a_id), None);
        assert!(a.is_closed() && !b.is_closed());
        assert_eq!(hub.subscriber_count(), 1);
        hub.close();
        assert!(b.is_closed());
        assert_eq!(hub.subscriber_count(), 0);
    }
}
