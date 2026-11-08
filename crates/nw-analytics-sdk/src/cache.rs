//! Compressed fallback while an already-decoded desktop picture is available.
//! Keeping one bounded GOP lets a plugin resume its decoder immediately when
//! that picture source is withdrawn, without asking the node to decode.
use nw_analytics_protocol::frame::MediaUnit;
use std::sync::Arc;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_UNITS: usize = 1024;

#[derive(Default)]
pub struct GopCache {
    units: Vec<Arc<MediaUnit>>,
    bytes: usize,
    overflowed: bool,
}

impl GopCache {
    pub fn push(&mut self, unit: Arc<MediaUnit>) {
        if unit.discontinuity() {
            self.clear();
        }
        if unit.keyframe() {
            self.clear();
        } else if self.units.is_empty() || self.overflowed {
            return;
        }
        if self.units.len() >= MAX_UNITS || unit.data.len() > MAX_BYTES.saturating_sub(self.bytes) {
            self.clear();
            self.overflowed = true;
            return;
        }
        self.bytes += unit.data.len();
        self.units.push(unit);
    }
    pub fn clear(&mut self) {
        self.units.clear();
        self.bytes = 0;
        self.overflowed = false;
    }
    pub fn replay(&self) -> Vec<Arc<MediaUnit>> {
        self.units.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_analytics_protocol::frame::{flags, MediaKind};
    fn frame(seq: u64, key: bool) -> Arc<MediaUnit> {
        Arc::new(MediaUnit {
            binding: 1,
            media: MediaKind::Video,
            seq,
            flags: if key { flags::KEYFRAME } else { 0 },
            time_us: 0,
            pts_ns: None,
            dts_ns: None,
            data: vec![1],
        })
    }
    #[test]
    fn fallback_restarts_at_a_keyframe_and_retains_payloads_by_reference() {
        let mut cache = GopCache::default();
        cache.push(frame(0, false));
        assert!(cache.replay().is_empty());
        let first = frame(1, true);
        cache.push(Arc::clone(&first));
        cache.push(frame(2, false));
        let replay = cache.replay();
        assert_eq!(replay.len(), 2);
        assert!(Arc::ptr_eq(&replay[0], &first));
        cache.push(frame(3, true));
        assert_eq!(cache.replay()[0].seq, 3);
        cache.clear();
        assert!(cache.replay().is_empty());
    }
    #[test]
    fn an_oversized_gop_is_dropped_until_the_next_keyframe() {
        let mut cache = GopCache::default();
        cache.push(frame(0, true));
        for seq in 1..=MAX_UNITS {
            cache.push(frame(seq as u64, false));
        }
        assert!(cache.replay().is_empty());
        cache.push(frame(2000, false));
        assert!(cache.replay().is_empty());
        cache.push(frame(2001, true));
        assert_eq!(cache.replay().len(), 1);
    }
}
