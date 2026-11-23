//! A subscriber's bounded queue and what it does when full.
//!
//! The hub hands every unit to every subscriber's queue with [`SubscriberQueue::push`],
//! which never blocks and never waits for the consumer: a consumer that falls
//! behind loses its own units (by its [`SubscriberPolicy`]), never anyone
//! else's. Generic over the unit, so the rules are tested without GStreamer.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Picture or sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Video,
    Audio,
}

/// What the queue needs to know about a unit.
pub trait Unit: Clone + Send + 'static {
    fn media(&self) -> Media;
    /// A video keyframe: decoding (and a new recording file) can start here.
    fn keyframe(&self) -> bool;
    fn bytes(&self) -> usize;
    /// Presentation time in nanoseconds, if the unit has one.
    fn time_ns(&self) -> Option<u64>;
    /// The video codec (`h264`, `h265`), for diagnostics; read on keyframes only.
    fn video_codec(&self) -> Option<&'static str> {
        None
    }
    /// The picture size (width, height) as the parser reported it; read on
    /// keyframes only.
    fn video_size(&self) -> Option<(u32, u32)> {
        None
    }
}

/// What a full queue drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// Drop new units until a video keyframe fits: video resumes decodable,
    /// with a gap marked on the first unit after it.
    SkipToKeyframe,
    /// Drop the oldest units (sound: what is late is worthless).
    DropOldest,
}

/// Fixed roles only: never subscriber identities, account data or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerKind {
    Recording,
    Desktop,
    Browser,
    Plugin,
    Snapshot,
    Audio,
    /// A stream the recorder keeps decoding for plugins (`always_decode`).
    /// Diagnostics reports it as a flag, not a role, so portals that know
    /// only the six roles above keep accepting the report.
    AlwaysDecode,
    /// A desktop view showing an always-on decoder's pictures (it decodes
    /// nothing itself). Diagnostics reports a count, not a role (as above).
    SharedView,
    /// Cloud Backup's real-time copy (`cloud::backup`). Diagnostics reports
    /// it as a flag, not a role (as above).
    CloudBackup,
}

/// How much a subscriber may fall behind, and what happens then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriberPolicy {
    pub consumer: ConsumerKind,
    pub max_bytes: usize,
    pub max_units: usize,
    /// Longest span of queued time (newest minus oldest unit).
    pub max_span: Duration,
    pub overflow: Overflow,
    /// Deliver video only from a keyframe on: at the start, and after a
    /// connection loss or a gap.
    pub start_at_keyframe: bool,
    /// How long the connection stays open after this subscriber, the last
    /// one, leaves (so paging does not reconnect).
    pub linger: Duration,
    /// Only this media (`None`: picture and sound).
    pub media: Option<Media>,
    /// Start with the whole GOP the hub has cached, however long: the
    /// limits above bound how far a consumer falls behind, and that GOP is
    /// where it starts, not a backlog (they then count what comes after it).
    /// Without it, a subscriber that joins late in a long GOP gets only what
    /// fits, and then nothing until the camera's next keyframe.
    pub whole_gop: bool,
}

impl SubscriberPolicy {
    /// A plugin that needs continuity may retain a bounded backlog before it
    /// skips to a keyframe. Its queue never blocks another subscriber.
    pub fn plugin_reliable(max_buffer_ms: u32) -> Self {
        let mut policy = Self::plugin();
        policy.max_span = Duration::from_millis(u64::from(max_buffer_ms.clamp(100, 10_000)));
        policy.max_units = 1200;
        policy.max_bytes = 32 * 1024 * 1024;
        policy
    }
    /// A recording: up to ~10 s behind (as long as the segment writer may
    /// stall today, `builder::RECORDING_STALL_NS`), then skip to the next
    /// keyframe. It closes the connection as soon as it leaves.
    pub fn recorder() -> Self {
        Self {
            consumer: ConsumerKind::Recording,
            max_bytes: 64 * 1024 * 1024,
            max_units: 4096,
            max_span: Duration::from_secs(10),
            overflow: Overflow::SkipToKeyframe,
            start_at_keyframe: true,
            linger: Duration::ZERO,
            media: None,
            whole_gop: false,
        }
    }

    /// A live viewer's pictures (browser live view, a desktop tile): a
    /// couple of seconds behind at most, then skip to the next keyframe.
    /// The connection stays open a while after the last viewer leaves, so
    /// paging and grid changes do not reconnect.
    pub fn live_video() -> Self {
        Self {
            consumer: ConsumerKind::Browser,
            max_bytes: 16 * 1024 * 1024,
            max_units: 256,
            max_span: Duration::from_secs(2),
            overflow: Overflow::SkipToKeyframe,
            start_at_keyframe: true,
            linger: LIVE_LINGER,
            media: None,
            whole_gop: false,
        }
    }

    /// A desktop decoder's pictures (a wall tile, a native overlay): pictures
    /// only, at most about a second behind before it skips to the next
    /// keyframe, so a slow decoder shows the present rather than a backlog.
    pub fn desktop_view() -> Self {
        Self {
            consumer: ConsumerKind::Desktop,
            max_bytes: 16 * 1024 * 1024,
            max_units: 64,
            max_span: Duration::from_secs(1),
            overflow: Overflow::SkipToKeyframe,
            start_at_keyframe: true,
            linger: LIVE_LINGER,
            media: Some(Media::Video),
            whole_gop: false,
        }
    }

    /// A desktop view of a stream the recorder decodes all the time: it only
    /// follows the connection's state (its units are discarded, not decoded).
    pub fn shared_view() -> Self {
        Self {
            consumer: ConsumerKind::SharedView,
            ..Self::desktop_view()
        }
    }

    /// Cloud Backup's copy of a camera stream: as a recording's (a backup
    /// skips to a keyframe only when ~10 s behind, and tells the ingest);
    /// sound too when the camera's plan includes it.
    pub fn cloud_backup(sound: bool) -> Self {
        Self {
            consumer: ConsumerKind::CloudBackup,
            media: if sound { None } else { Some(Media::Video) },
            ..Self::recorder()
        }
    }

    /// A camera stream the recorder keeps decoding (`CameraConfig::always_decode`):
    /// as a desktop decoder's, under its own role.
    pub fn always_decode() -> Self {
        Self {
            consumer: ConsumerKind::AlwaysDecode,
            ..Self::desktop_view()
        }
    }

    /// An analytics plugin's compressed pictures (`crate::plugin_host`): as a
    /// desktop decoder's, so a plugin that falls behind skips to the next
    /// keyframe (the plugin is told: a `gap`).
    pub fn plugin() -> Self {
        Self {
            consumer: ConsumerKind::Plugin,
            ..Self::desktop_view()
        }
    }

    /// One picture, with no connection left running after the request.
    pub fn snapshot() -> Self {
        Self {
            consumer: ConsumerKind::Snapshot,
            max_units: 1,
            linger: Duration::ZERO,
            ..Self::desktop_view()
        }
    }

    /// Sound to listen to: what is late is dropped first.
    pub fn live_sound() -> Self {
        Self {
            consumer: ConsumerKind::Audio,
            max_bytes: 1024 * 1024,
            max_units: 256,
            max_span: Duration::from_secs(1),
            overflow: Overflow::DropOldest,
            start_at_keyframe: false,
            linger: LIVE_LINGER,
            media: Some(Media::Audio),
            whole_gop: false,
        }
    }
}

/// How long a viewer's connection stays open after the last viewer left.
pub const LIVE_LINGER: Duration = Duration::from_secs(10);

/// One item a subscriber receives.
#[derive(Debug, Clone)]
pub enum Delivery<U> {
    /// A unit. `after_gap`: units before it were dropped because this
    /// subscriber fell behind.
    Unit { unit: U, after_gap: bool },
    /// The camera connection was lost: what follows comes from a new
    /// connection (new timestamps, possibly new caps).
    Discontinuity { reason: String },
}

struct Inner<U> {
    items: VecDeque<Delivery<U>>,
    bytes: usize,
    /// Items at the front, and their bytes, that the limits do not count:
    /// the GOP a `whole_gop` subscriber started with.
    exempt: usize,
    exempt_bytes: usize,
    policy: SubscriberPolicy,
    /// Dropping until a video keyframe fits.
    skipping: bool,
    /// The next delivered unit follows dropped units.
    gap: bool,
    dropped: u64,
    closed: bool,
}

/// A subscriber's queue: the hub pushes, the consumer pops.
pub struct SubscriberQueue<U> {
    inner: Mutex<Inner<U>>,
    ready: Condvar,
}

impl<U: Unit> SubscriberQueue<U> {
    pub fn new(policy: SubscriberPolicy) -> Self {
        Self {
            inner: Mutex::new(Inner {
                items: VecDeque::new(),
                bytes: 0,
                exempt: 0,
                exempt_bytes: 0,
                policy,
                skipping: policy.start_at_keyframe,
                gap: false,
                dropped: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<U>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn policy(&self) -> SubscriberPolicy {
        self.lock().policy
    }

    /// Hand `unit` to this subscriber. Never blocks.
    pub fn push(&self, unit: U) {
        self.push_as(unit, false);
    }

    /// Hand this subscriber a unit of the GOP it starts with (the hub's
    /// cache, in order, before any live unit): for a `whole_gop` policy it
    /// is queued whatever the limits, and they do not count it.
    pub fn push_start(&self, unit: U) {
        self.push_as(unit, true);
    }

    fn push_as(&self, unit: U, start: bool) {
        let mut inner = self.lock();
        if inner.closed
            || inner
                .policy
                .media
                .is_some_and(|media| media != unit.media())
        {
            return;
        }
        let policy = inner.policy;
        let exempt = start && policy.whole_gop && inner.exempt == inner.items.len();
        if inner.skipping {
            let starts = unit.media() == Media::Video && unit.keyframe();
            if !starts || !(exempt || fits(&inner, &unit)) {
                // Waiting for the first keyframe is no loss; skipping after
                // falling behind is.
                if inner.gap {
                    inner.dropped += 1;
                }
                return;
            }
            inner.skipping = false;
        } else if !exempt && !fits(&inner, &unit) {
            match policy.overflow {
                Overflow::SkipToKeyframe => {
                    inner.skipping = true;
                    inner.gap = true;
                    inner.dropped += 1;
                    return;
                }
                Overflow::DropOldest => {
                    while !inner.items.is_empty() && !fits(&inner, &unit) {
                        if let Some(Delivery::Unit { unit: old, .. }) = inner.items.pop_front() {
                            inner.bytes = inner.bytes.saturating_sub(old.bytes());
                            inner.dropped += 1;
                        }
                    }
                    // The gap is before what is now the oldest unit.
                    if let Some(Delivery::Unit { after_gap, .. }) = inner.items.front_mut() {
                        *after_gap = true;
                    } else {
                        inner.gap = true;
                    }
                }
            }
        }
        let after_gap = std::mem::take(&mut inner.gap);
        inner.bytes += unit.bytes();
        if exempt {
            inner.exempt += 1;
            inner.exempt_bytes += unit.bytes();
        }
        inner.items.push_back(Delivery::Unit { unit, after_gap });
        drop(inner);
        self.ready.notify_one();
    }

    /// The connection was lost; always queued (it is tiny), and what comes
    /// next starts at a keyframe again when the policy asks for it.
    pub fn push_discontinuity(&self, reason: &str) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        inner.skipping = inner.policy.start_at_keyframe;
        inner.gap = false;
        inner.items.push_back(Delivery::Discontinuity {
            reason: reason.to_owned(),
        });
        drop(inner);
        self.ready.notify_one();
    }

    /// The next item, waiting up to `timeout`.
    pub fn pop(&self, timeout: Duration) -> Option<Delivery<U>> {
        let mut inner = self.lock();
        if inner.items.is_empty() && !inner.closed && !timeout.is_zero() {
            inner = self
                .ready
                .wait_timeout_while(inner, timeout, |inner| {
                    inner.items.is_empty() && !inner.closed
                })
                .map(|(guard, _)| guard)
                .unwrap_or_else(|p| p.into_inner().0);
        }
        let item = inner.items.pop_front()?;
        let was_exempt = inner.exempt > 0;
        inner.exempt = inner.exempt.saturating_sub(1);
        if let Delivery::Unit { unit, .. } = &item {
            inner.bytes = inner.bytes.saturating_sub(unit.bytes());
            if was_exempt {
                inner.exempt_bytes = inner.exempt_bytes.saturating_sub(unit.bytes());
            }
        }
        Some(item)
    }

    /// Units this subscriber lost by falling behind.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Queued units and their bytes.
    pub fn level(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.items.len(), inner.bytes)
    }

    /// No more items will come (the hub closed); wakes a waiting consumer.
    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

/// Whether `unit` fits next to what `inner` holds.
fn fits<U: Unit>(inner: &Inner<U>, unit: &U) -> bool {
    let policy = &inner.policy;
    // What the subscriber started with (`whole_gop`) is not counted.
    let counted = inner.items.len() - inner.exempt.min(inner.items.len());
    if counted == 0 {
        return true;
    }
    if counted >= policy.max_units
        || inner.bytes.saturating_sub(inner.exempt_bytes) + unit.bytes() > policy.max_bytes
    {
        return false;
    }
    let oldest = inner
        .items
        .iter()
        .skip(inner.exempt)
        .find_map(|item| match item {
            Delivery::Unit { unit, .. } => unit.time_ns(),
            Delivery::Discontinuity { .. } => None,
        });
    match (oldest, unit.time_ns()) {
        (Some(oldest), Some(newest)) => {
            newest.saturating_sub(oldest) <= policy.max_span.as_nanos() as u64
        }
        _ => true,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in for a camera unit: no GStreamer.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Fake {
        pub media: Media,
        pub key: bool,
        pub size: usize,
        pub ms: u64,
    }

    impl Unit for Fake {
        fn media(&self) -> Media {
            self.media
        }
        fn keyframe(&self) -> bool {
            self.key
        }
        fn bytes(&self) -> usize {
            self.size
        }
        fn time_ns(&self) -> Option<u64> {
            Some(self.ms * 1_000_000)
        }
    }

    pub(crate) fn video(ms: u64, key: bool) -> Fake {
        Fake {
            media: Media::Video,
            key,
            size: 100,
            ms,
        }
    }

    pub(crate) fn audio(ms: u64) -> Fake {
        Fake {
            media: Media::Audio,
            key: false,
            size: 10,
            ms,
        }
    }

    fn small(overflow: Overflow) -> SubscriberPolicy {
        SubscriberPolicy {
            consumer: ConsumerKind::Desktop,
            max_bytes: 1_000_000,
            max_units: 3,
            max_span: Duration::from_secs(10),
            overflow,
            start_at_keyframe: true,
            linger: Duration::ZERO,
            media: None,
            whole_gop: false,
        }
    }

    fn drain(queue: &SubscriberQueue<Fake>) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        while let Some(item) = queue.pop(Duration::ZERO) {
            if let Delivery::Unit { unit, after_gap } = item {
                out.push((unit.ms, after_gap));
            }
        }
        out
    }

    #[test]
    fn a_whole_gop_start_is_kept_whatever_the_limits_which_then_count_what_follows() {
        // Live view starting 8 s into a camera's GOP: the limits (3 units,
        // 2 s) are for falling behind, and without the whole GOP the viewer
        // waits for the camera's next keyframe.
        let policy = SubscriberPolicy {
            max_span: Duration::from_secs(2),
            whole_gop: true,
            ..small(Overflow::SkipToKeyframe)
        };
        let queue = SubscriberQueue::new(policy);
        for i in 0..9u64 {
            queue.push_start(video(i * 1000, i == 0));
        }
        assert_eq!(queue.level().0, 9, "all of it, 8 s and 9 units");
        // Live units after it are counted from the first of them.
        queue.push(video(9000, false));
        queue.push(video(10_000, false));
        queue.push(video(11_000, false));
        assert_eq!(queue.dropped(), 0, "2 s and 3 units of live video fit");
        queue.push(video(12_000, false));
        assert_eq!(
            queue.dropped(),
            1,
            "a fourth, 3 s after the first, does not"
        );
        let got: Vec<u64> = drain(&queue).into_iter().map(|(ms, _)| ms).collect();
        assert_eq!(got, (0..12).map(|i| i * 1000).collect::<Vec<_>>());
        // Read, the start no longer exempts anything.
        queue.push(video(13_000, true));
        for i in 0..3u64 {
            queue.push(video(13_100 + i, false));
        }
        assert_eq!(queue.level().0, 3, "the limits apply as ever");
    }

    #[test]
    fn a_start_is_limited_like_any_unit_without_whole_gop() {
        let queue = SubscriberQueue::new(SubscriberPolicy {
            max_span: Duration::from_secs(2),
            ..small(Overflow::SkipToKeyframe)
        });
        for i in 0..9u64 {
            queue.push_start(video(i * 1000, i == 0));
        }
        assert_eq!(queue.level().0, 3, "what fits, as before");
        // And a start is only the units before any live one.
        let queue = SubscriberQueue::new(SubscriberPolicy {
            whole_gop: true,
            ..small(Overflow::SkipToKeyframe)
        });
        queue.push_start(video(0, true));
        queue.push(video(10, false));
        for i in 0..5u64 {
            queue.push_start(video(20 + i, false));
        }
        assert_eq!(queue.level().0, 4, "a keyframe, then three counted units");
    }

    #[test]
    fn video_starts_at_a_keyframe() {
        let queue = SubscriberQueue::new(small(Overflow::SkipToKeyframe));
        queue.push(video(0, false));
        queue.push(audio(5));
        queue.push(video(40, true));
        queue.push(audio(45));
        assert_eq!(drain(&queue), vec![(40, false), (45, false)]);
        assert_eq!(
            queue.dropped(),
            0,
            "waiting for the first keyframe is no loss"
        );
    }

    #[test]
    fn a_full_queue_skips_to_the_next_keyframe_and_marks_the_gap() {
        let queue = SubscriberQueue::new(small(Overflow::SkipToKeyframe));
        for (ms, key) in [(0, true), (40, false), (80, false)] {
            queue.push(video(ms, key));
        }
        queue.push(video(120, false)); // full: dropped
        queue.push(video(160, true)); // a keyframe, but still full: dropped
        assert_eq!(queue.dropped(), 2);
        assert_eq!(drain(&queue), vec![(0, false), (40, false), (80, false)]);
        queue.push(video(200, false)); // room again, but no keyframe yet
        queue.push(video(240, true));
        queue.push(video(280, false));
        assert_eq!(drain(&queue), vec![(240, true), (280, false)]);
        assert_eq!(queue.dropped(), 3);
    }

    #[test]
    fn a_sound_subscriber_never_queues_pictures() {
        let queue = SubscriberQueue::new(SubscriberPolicy::live_sound());
        queue.push(video(0, true));
        queue.push(audio(5));
        queue.push(video(40, false));
        queue.push(audio(25));
        assert_eq!(drain(&queue), vec![(5, false), (25, false)]);
        assert_eq!(queue.dropped(), 0);
    }

    #[test]
    fn sound_drops_its_oldest_units() {
        let mut policy = small(Overflow::DropOldest);
        policy.start_at_keyframe = false;
        let queue = SubscriberQueue::new(policy);
        for ms in [0, 20, 40, 60, 80] {
            queue.push(audio(ms));
        }
        assert_eq!(drain(&queue), vec![(40, true), (60, false), (80, false)]);
        assert_eq!(queue.dropped(), 2);
    }

    #[test]
    fn the_queued_span_is_bounded_in_time() {
        let mut policy = small(Overflow::SkipToKeyframe);
        policy.max_units = 1000;
        policy.max_span = Duration::from_millis(100);
        let queue = SubscriberQueue::new(policy);
        queue.push(video(0, true));
        queue.push(video(100, false));
        queue.push(video(140, false)); // 140 ms after the oldest: too far behind
        assert_eq!(drain(&queue), vec![(0, false), (100, false)]);
        assert_eq!(queue.dropped(), 1);
    }

    #[test]
    fn after_a_connection_loss_video_starts_at_a_keyframe_again() {
        let queue = SubscriberQueue::new(small(Overflow::SkipToKeyframe));
        queue.push(video(0, true));
        queue.push_discontinuity("camera closed the stream");
        queue.push(video(10, false));
        queue.push(video(50, true));
        let items: Vec<_> = std::iter::from_fn(|| queue.pop(Duration::ZERO)).collect();
        assert!(matches!(items[0], Delivery::Unit { ref unit, .. } if unit.ms == 0));
        assert!(
            matches!(items[1], Delivery::Discontinuity { ref reason } if reason == "camera closed the stream")
        );
        assert!(matches!(items[2], Delivery::Unit { ref unit, after_gap: false } if unit.ms == 50));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn a_waiting_consumer_wakes_for_a_unit_or_a_close() {
        let queue = std::sync::Arc::new(SubscriberQueue::new(small(Overflow::SkipToKeyframe)));
        let pusher = {
            let queue = std::sync::Arc::clone(&queue);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                queue.push(video(0, true));
                std::thread::sleep(Duration::from_millis(50));
                queue.close();
            })
        };
        let started = std::time::Instant::now();
        assert!(queue.pop(Duration::from_secs(5)).is_some());
        assert!(queue.pop(Duration::from_secs(5)).is_none());
        assert!(queue.is_closed());
        assert!(started.elapsed() < Duration::from_secs(2));
        pusher.join().unwrap();
    }
}
