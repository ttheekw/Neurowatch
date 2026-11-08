//! Picture identities assigned once, before hub fan-out. The timestamp is
//! derived from the camera's presentation clock on the recording timeline;
//! plugins can refer to a picture but cannot supply their own capture time.
//! A separate node receipt clock aligns live views across independent streams.
pub use nw_core::frame::FrameIdentity as Identity;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const KEEP: Duration = Duration::from_secs(60);
const MAX_FRAMES: usize = 8192;
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct History {
    origin: Option<(u64, i64)>,
    entries: VecDeque<(Identity, Instant)>,
}

impl History {
    pub fn record(&mut self, pts: Option<u64>, dts: Option<u64>, arrived_us: i64) -> Identity {
        self.record_at(pts, dts, arrived_us, Instant::now())
    }

    fn record_at(
        &mut self,
        pts: Option<u64>,
        dts: Option<u64>,
        arrived_us: i64,
        now: Instant,
    ) -> Identity {
        let presentation = pts.or(dts);
        let time_us = match presentation {
            Some(pts) => {
                let base = dts.unwrap_or(pts);
                let lead = pts.saturating_sub(base) / 1000;
                let (origin, epoch) = *self.origin.get_or_insert((
                    base,
                    arrived_us.saturating_sub(i64::try_from(lead).unwrap_or(i64::MAX)),
                ));
                let delta = (i128::from(pts) - i128::from(origin)) / 1000;
                (i128::from(epoch) + delta).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
            }
            None => arrived_us,
        };
        let identity = Identity {
            seq: NEXT.fetch_add(1, Ordering::Relaxed),
            time_us,
            observed_us: arrived_us,
        };
        self.remember_at(identity, now);
        identity
    }

    pub fn remember(&mut self, identity: Identity) {
        self.remember_at(identity, Instant::now());
    }

    fn remember_at(&mut self, identity: Identity, now: Instant) {
        self.entries.push_back((identity, now));
        while self.entries.len() > MAX_FRAMES
            || self
                .entries
                .front()
                .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= KEEP)
        {
            self.entries.pop_front();
        }
    }

    pub fn get(&self, seq: u64) -> Option<Identity> {
        self.entries
            .iter()
            .rev()
            .find(|(frame, at)| frame.seq == seq && at.elapsed() < KEEP)
            .map(|(frame, _)| *frame)
    }

    /// Reconnecting changes the camera's clock, not the identity counter.
    pub fn restart(&mut self) {
        self.origin = None;
    }
}

/// A decoder preserves PTS; this small map relates its rebased timestamps
/// to the hub's identities. Each decoder owns its own map (two views may
/// rebase the same camera differently).
#[derive(Default)]
pub struct DecoderFrames {
    entries: VecDeque<(u64, Identity)>,
}

impl DecoderFrames {
    pub fn put(&mut self, pts: u64, identity: Identity) {
        self.entries.push_back((pts, identity));
        while self.entries.len() > MAX_FRAMES {
            self.entries.pop_front();
        }
    }
    pub fn get(&self, pts: u64) -> Option<Identity> {
        self.entries
            .iter()
            .rev()
            .find(|(time, _)| *time == pts)
            .map(|(_, identity)| *identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_time_matches_recording_including_b_frames_and_reconnects() {
        let mut history = History::default();
        let first = history.record(
            Some(5_080_000_000),
            Some(5_000_000_000),
            1_700_000_000_000_000,
        );
        let later = history.record(
            Some(5_160_000_000),
            Some(5_040_000_000),
            1_700_000_000_050_000,
        );
        let reordered = history.record(
            Some(5_120_000_000),
            Some(5_080_000_000),
            1_700_000_000_090_000,
        );
        assert_eq!(first.time_us, 1_700_000_000_000_000);
        assert_eq!(later.time_us - first.time_us, 80_000);
        assert_eq!(reordered.time_us - first.time_us, 40_000);
        assert!(reordered.seq > later.seq && later.seq > first.seq);
        history.restart();
        let new = history.record(Some(100_000_000), None, 1_700_000_001_000_000);
        assert!(new.seq > reordered.seq);
        assert_eq!(new.time_us, 1_700_000_001_000_000);
        assert_eq!(history.get(first.seq), Some(first));
        assert_eq!(history.get(u64::MAX), None);
    }

    #[test]
    fn history_is_bounded_and_does_not_reuse_identifiers_across_hubs() {
        let mut a = History::default();
        let mut b = History::default();
        let first = a.record(None, None, 1);
        let second = b.record(None, None, 1);
        assert_ne!(first.seq, second.seq);
        for _ in 0..MAX_FRAMES {
            a.record(None, None, 2);
        }
        assert_eq!(a.get(first.seq), None);
        assert_eq!(a.entries.len(), MAX_FRAMES);
        let expired = a.record_at(None, None, 3, Instant::now() - KEEP);
        assert_eq!(a.get(expired.seq), None);
    }

    #[test]
    fn independent_stream_timelines_share_the_node_observation_clock() {
        let mut sub = History::default();
        let mut main = History::default();
        sub.record(Some(0), None, 1_000_000);
        main.record(Some(0), None, 5_000_000);
        let analyzed = sub.record(Some(2_000_000_000), None, 10_000_000);
        let shown = main.record(Some(2_000_000_000), None, 10_040_000);
        assert_ne!(analyzed.time_us, shown.time_us);
        assert_ne!(analyzed.seq, shown.seq);
        assert_eq!(shown.observed_us - analyzed.observed_us, 40_000);
        assert_eq!(sub.get(analyzed.seq), Some(analyzed));
    }

    #[test]
    fn each_decoder_matches_by_pts_instead_of_decode_order() {
        let mut frames = DecoderFrames::default();
        let a = Identity {
            seq: 1,
            time_us: 100,
            observed_us: 110,
        };
        let b = Identity {
            seq: 2,
            time_us: 90,
            observed_us: 120,
        };
        frames.put(50, a);
        frames.put(40, b);
        assert_eq!(frames.get(40), Some(b));
        assert_eq!(frames.get(50), Some(a));
        assert_eq!(frames.get(0), None);
    }
}
