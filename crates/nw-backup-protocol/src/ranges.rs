//! Sets of unit numbers as inclusive ranges: what an `Ack` carries.
//!
//! Live units and a backlog sent after an outage arrive interleaved, so what
//! the ingest holds of a track is a few ranges rather than one number.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Most ranges an `Ack` carries: the newest ones, when there are more (the
/// recorder resends what an older, unreported range held).
pub const MAX_WIRE_RANGES: usize = 64;

/// Disjoint, non-adjacent inclusive ranges of unit numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeqRanges {
    /// start → end (inclusive).
    ranges: BTreeMap<u64, u64>,
}

impl SeqRanges {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of separate ranges.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn contains(&self, seq: u64) -> bool {
        self.ranges
            .range(..=seq)
            .next_back()
            .is_some_and(|(_, &end)| end >= seq)
    }

    pub fn insert(&mut self, seq: u64) {
        self.insert_range(seq, seq);
    }

    /// Add `start..=end` (ignored when `start > end`), merging neighbours.
    pub fn insert_range(&mut self, start: u64, end: u64) {
        if start > end {
            return;
        }
        let (mut start, mut end) = (start, end);
        // A range starting at or before `start` that reaches it (or touches it).
        if let Some((&s, &e)) = self.ranges.range(..=start).next_back() {
            if e.saturating_add(1) >= start {
                start = s;
                end = end.max(e);
            }
        }
        // Ranges starting inside `start..=end + 1` merge in.
        let reach = end.saturating_add(1);
        let swallowed: Vec<u64> = self.ranges.range(start..=reach).map(|(&s, _)| s).collect();
        for s in swallowed {
            if let Some(e) = self.ranges.remove(&s) {
                end = end.max(e);
            }
        }
        self.ranges.insert(start, end);
    }

    /// Remove every number in `other`.
    pub fn remove_all(&mut self, other: &SeqRanges) {
        for (&start, &end) in &other.ranges {
            self.remove_range(start, end);
        }
    }

    /// Remove `start..=end`.
    pub fn remove_range(&mut self, start: u64, end: u64) {
        if start > end {
            return;
        }
        let touched: Vec<(u64, u64)> = self
            .ranges
            .range(..=end)
            .filter(|(_, &e)| e >= start)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in touched {
            self.ranges.remove(&s);
            if s < start {
                self.ranges.insert(s, start - 1);
            }
            if e > end {
                self.ranges.insert(end + 1, e);
            }
        }
    }

    /// The highest `n` such that every number in `first..=n` is held.
    pub fn contiguous_from(&self, first: u64) -> Option<u64> {
        self.ranges
            .range(..=first)
            .next_back()
            .filter(|(_, &end)| end >= first)
            .map(|(_, &end)| end)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges.iter().map(|(&s, &e)| (s, e))
    }

    /// For an `Ack`: the newest [`MAX_WIRE_RANGES`] ranges.
    pub fn to_wire(&self) -> Vec<SeqRange> {
        let skip = self.ranges.len().saturating_sub(MAX_WIRE_RANGES);
        self.iter()
            .skip(skip)
            .map(|(s, e)| SeqRange(s, e))
            .collect()
    }

    /// From an `Ack` (or `TrackOpened`); inverted ranges are ignored.
    pub fn from_wire(ranges: &[SeqRange]) -> Self {
        let mut set = Self::new();
        for range in ranges {
            set.insert_range(range.0, range.1);
        }
        set
    }
}

/// One inclusive range on the wire: `[start, end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange(pub u64, pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(set: &SeqRanges) -> Vec<(u64, u64)> {
        set.iter().collect()
    }

    #[test]
    fn neighbours_merge_in_any_order() {
        let mut set = SeqRanges::new();
        for seq in [5, 3, 4, 10, 1, 2, 12, 11] {
            set.insert(seq);
        }
        assert_eq!(pairs(&set), [(1, 5), (10, 12)]);
        set.insert_range(6, 9);
        assert_eq!(pairs(&set), [(1, 12)]);
        set.insert_range(0, 20);
        assert_eq!(pairs(&set), [(0, 20)]);
        set.insert_range(3, 2);
        assert_eq!(pairs(&set), [(0, 20)], "an inverted range adds nothing");
        set.insert(u64::MAX);
        set.insert(u64::MAX - 1);
        assert_eq!(pairs(&set), [(0, 20), (u64::MAX - 1, u64::MAX)]);
    }

    #[test]
    fn membership_and_contiguity() {
        let set = SeqRanges::from_wire(&[SeqRange(1, 100), SeqRange(150, 200)]);
        assert!(set.contains(1) && set.contains(100) && set.contains(175));
        assert!(!set.contains(0) && !set.contains(101) && !set.contains(201));
        assert_eq!(set.contiguous_from(1), Some(100));
        assert_eq!(set.contiguous_from(160), Some(200));
        assert_eq!(set.contiguous_from(120), None);
    }

    #[test]
    fn removing_splits_ranges() {
        let mut set = SeqRanges::from_wire(&[SeqRange(1, 100), SeqRange(150, 200)]);
        set.remove_range(50, 160);
        assert_eq!(pairs(&set), [(1, 49), (161, 200)]);
        set.remove_all(&SeqRanges::from_wire(&[
            SeqRange(1, 49),
            SeqRange(200, 300),
        ]));
        assert_eq!(pairs(&set), [(161, 199)]);
    }

    #[test]
    fn the_wire_keeps_the_newest_ranges() {
        let mut set = SeqRanges::new();
        for i in 0..(MAX_WIRE_RANGES as u64 + 10) {
            set.insert(i * 10);
        }
        let wire = set.to_wire();
        assert_eq!(wire.len(), MAX_WIRE_RANGES);
        assert_eq!(wire.last(), Some(&SeqRange(730, 730)));
        assert_eq!(serde_json::to_string(&wire[..1]).unwrap(), "[[100,100]]");
    }
}
