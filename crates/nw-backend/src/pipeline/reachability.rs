//! "The camera answered", shared by a camera's ingest and its desktop view.
//!
//! Each retries a lost camera on its own backoff (up to 30 s). Without this,
//! after an outage one of them can get data again up to a full backoff before
//! the other: the view shows pictures while the camera's status (the
//! ingest's) still says offline, and the UI flips between the two. When one
//! side gets data, the other, if it is waiting out a backoff, retries at once.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Reachability {
    answers: AtomicU64,
}

impl Reachability {
    /// The camera delivered data (an ingest or a view started streaming).
    pub fn answered(&self) {
        self.answers.fetch_add(1, Ordering::Relaxed);
    }

    /// A mark to compare with later, taken when a backoff starts.
    pub fn mark(&self) -> u64 {
        self.answers.load(Ordering::Relaxed)
    }

    /// Whether the camera answered since `mark` was taken.
    pub fn answered_since(&self, mark: u64) -> bool {
        self.mark() != mark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_after_the_mark_is_seen_once_per_mark() {
        let reach = Reachability::default();
        let mark = reach.mark();
        assert!(!reach.answered_since(mark));
        reach.answered();
        assert!(reach.answered_since(mark));
        assert!(!reach.answered_since(reach.mark()));
    }
}
