//! IPC transport metrics recorder.
//!
//! Owned by the IPC server; a clone-cheap `IpcMetricsHandle` is passed to
//! every connection. All counters use `AtomicU64` so recording is
//! lock-free on the hot path; latencies are aggregated in a small ring
//! guarded by a `Mutex` so that reads (rare) and writes (per-request)
//! never contend for long.

use std::cell::Cell;
use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nw_core::stats::IpcMetrics;

use super::IpcMetricsSource;

const LATENCY_RING_CAP: usize = 256;
// A count-only ring can preserve startup/lifecycle delays indefinitely on
// lightly used control sockets. Diagnostics should describe recent activity.
const LATENCY_WINDOW: Duration = Duration::from_secs(30);

tokio::task_local! {
    static INTENTIONAL_IDLE: Cell<Duration>;
}

/// Measure a handler independently of other connections and nested requests.
/// Only explicitly observed event-wait time is excluded, never a method's
/// entire duration or its requested timeout. Lock/query/send delays remain.
pub(crate) async fn measure_handler<F: Future>(work: F) -> (F::Output, Duration) {
    INTENTIONAL_IDLE
        .scope(Cell::new(Duration::ZERO), async move {
            let result = work.await;
            (result, INTENTIONAL_IDLE.with(Cell::get))
        })
        .await
}

pub(crate) fn record_idle_wait(duration: Duration) {
    let _ = INTENTIONAL_IDLE.try_with(|idle| idle.set(idle.get().saturating_add(duration)));
}

#[derive(Debug, Default)]
struct Ring {
    samples: Vec<(Instant, f32)>,
    cursor: usize,
}

impl Ring {
    fn push(&mut self, sample: f32, completed_at: Instant) {
        if self.samples.len() < LATENCY_RING_CAP {
            self.samples.push((completed_at, sample));
        } else {
            self.samples[self.cursor] = (completed_at, sample);
            self.cursor = (self.cursor + 1) % LATENCY_RING_CAP;
        }
    }

    fn summary(&self, now: Instant) -> (f32, f32) {
        let mut sorted: Vec<f32> = self
            .samples
            .iter()
            .filter(|(completed_at, _)| {
                now.saturating_duration_since(*completed_at) < LATENCY_WINDOW
            })
            .map(|(_, latency)| *latency)
            .collect();
        if sorted.is_empty() {
            return (0.0, 0.0);
        }
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let sum: f32 = sorted.iter().sum();
        let avg = sum / (sorted.len() as f32);
        let idx = ((sorted.len() as f32) * 0.95).ceil() as usize;
        let idx = idx.saturating_sub(1).min(sorted.len() - 1);
        (avg, sorted[idx])
    }
}

/// Shared state — held by the recorder and read via `snapshot`.
#[derive(Debug, Default)]
struct Shared {
    requests: AtomicU64,
    messages: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    drops: AtomicU64,
    backpressure: AtomicU64,
    inflight: AtomicU32,
    latencies: Mutex<Ring>,
}

/// Recorder — allocation-free hot path via atomics. Cloneable via `handle()`.
#[derive(Debug, Default, Clone)]
pub struct IpcMetricsRecorder {
    shared: Arc<Shared>,
}

impl IpcMetricsRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cheap clone-able handle suitable for per-connection state.
    pub fn handle(&self) -> IpcMetricsHandle {
        IpcMetricsHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    // Public counters intended for direct call from server/backpressure
    // paths that don't currently exist (e.g. explicit send-side stall
    // detection). The transport currently signals drops via the
    // `IpcRequestSpan` Drop path, which already increments `drops`.
    #[allow(dead_code)]
    pub fn record_drop(&self) {
        self.shared.drops.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn record_backpressure(&self) {
        self.shared.backpressure.fetch_add(1, Ordering::Relaxed);
    }
}

impl IpcMetricsSource for IpcMetricsRecorder {
    fn snapshot(&self) -> IpcMetrics {
        let (avg, p95) = self
            .shared
            .latencies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .summary(Instant::now());
        IpcMetrics {
            requests_total: self.shared.requests.load(Ordering::Relaxed),
            messages_total: self.shared.messages.load(Ordering::Relaxed),
            bytes_in_total: self.shared.bytes_in.load(Ordering::Relaxed),
            bytes_out_total: self.shared.bytes_out.load(Ordering::Relaxed),
            avg_latency_ms: avg,
            p95_latency_ms: p95,
            drops_total: self.shared.drops.load(Ordering::Relaxed),
            backpressure_events_total: self.shared.backpressure.load(Ordering::Relaxed),
            inflight: self.shared.inflight.load(Ordering::Relaxed),
        }
    }
}

/// Per-connection handle. Cheap to clone; state is shared.
#[derive(Debug, Clone)]
pub struct IpcMetricsHandle {
    shared: Arc<Shared>,
}

impl IpcMetricsHandle {
    pub fn begin_request(&self, bytes_in: usize) -> IpcRequestSpan {
        self.shared.requests.fetch_add(1, Ordering::Relaxed);
        self.shared.messages.fetch_add(1, Ordering::Relaxed);
        self.shared
            .bytes_in
            .fetch_add(bytes_in as u64, Ordering::Relaxed);
        self.shared.inflight.fetch_add(1, Ordering::Relaxed);
        IpcRequestSpan {
            shared: Arc::clone(&self.shared),
            start: Instant::now(),
            completed: false,
        }
    }
}

/// RAII-style span — records latency + response size on `complete()` and
/// cleans up `inflight` on drop even when a connection aborts.
pub struct IpcRequestSpan {
    shared: Arc<Shared>,
    start: Instant,
    completed: bool,
}

impl IpcRequestSpan {
    pub fn complete(self, bytes_out: usize) {
        self.complete_with_idle(bytes_out, Duration::ZERO);
    }

    pub(crate) fn complete_with_idle(mut self, bytes_out: usize, idle: Duration) {
        let completed_at = Instant::now();
        let elapsed_ms = completed_at
            .duration_since(self.start)
            .saturating_sub(idle)
            .as_secs_f32()
            * 1000.0;
        self.shared
            .bytes_out
            .fetch_add(bytes_out as u64, Ordering::Relaxed);
        self.shared
            .latencies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(elapsed_ms, completed_at);
        self.shared.inflight.fetch_sub(1, Ordering::Relaxed);
        self.completed = true;
    }
}

impl Drop for IpcRequestSpan {
    fn drop(&mut self) {
        if !self.completed {
            self.shared.inflight.fetch_sub(1, Ordering::Relaxed);
            self.shared.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_event_wait_is_not_backpressure_but_other_work_is_counted() {
        let recorder = IpcMetricsRecorder::new();
        let mut span = recorder.handle().begin_request(64);
        span.start = Instant::now() - Duration::from_millis(1500);
        span.complete_with_idle(128, Duration::from_millis(1450));
        let snapshot = recorder.snapshot();
        assert!(snapshot.p95_latency_ms < 100.0);
        assert_eq!(snapshot.requests_total, 1);
        assert_eq!(snapshot.bytes_out_total, 128);
        assert_eq!(snapshot.inflight, 0);
        assert_eq!(snapshot.drops_total, 0);
        assert_eq!(snapshot.backpressure_events_total, 0);

        let mut slow_control = recorder.handle().begin_request(32);
        slow_control.start = Instant::now() - Duration::from_millis(750);
        slow_control.complete(64);
        assert!(recorder.snapshot().p95_latency_ms >= 750.0);
    }

    #[tokio::test]
    async fn intentional_wait_accounting_is_scoped_to_each_request() {
        let ((_, _), idle) = measure_handler(async {
            record_idle_wait(Duration::from_millis(20));
            let nested = measure_handler(async {
                record_idle_wait(Duration::from_millis(30));
            })
            .await;
            assert_eq!(nested.1, Duration::from_millis(30));
            ((), ())
        })
        .await;
        assert_eq!(idle, Duration::from_millis(20));
        let (_, next_idle) = measure_handler(async { tokio::task::yield_now().await }).await;
        assert_eq!(next_idle, Duration::ZERO);
    }

    #[tokio::test]
    async fn actual_event_long_poll_records_only_its_idle_wait() {
        let db = Arc::new(Mutex::new(crate::db::connection::open_in_memory().unwrap()));
        let hub = crate::events::EventHub::new(db);
        let (_, idle) = measure_handler(hub.wait(None, Some(0), Duration::from_millis(30))).await;
        assert!(idle >= Duration::from_millis(25));
        let (_, ready_idle) = measure_handler(hub.wait(None, None, Duration::ZERO)).await;
        assert_eq!(ready_idle, Duration::ZERO);
    }

    #[test]
    fn latency_window_keeps_recent_samples_and_expires_old_spikes() {
        let now = Instant::now();
        let mut ring = Ring::default();
        ring.push(3415.0, now);
        ring.push(5.0, now + LATENCY_WINDOW - Duration::from_secs(1));

        assert_eq!(
            ring.summary(now + LATENCY_WINDOW - Duration::from_secs(1)),
            (1710.0, 3415.0)
        );
        assert_eq!(ring.summary(now + LATENCY_WINDOW), (5.0, 5.0));
        assert_eq!(ring.summary(now + LATENCY_WINDOW * 2), (0.0, 0.0));
    }

    #[test]
    fn latency_ring_stays_bounded_and_uses_latest_requests() {
        let now = Instant::now();
        let mut ring = Ring::default();
        for _ in 0..LATENCY_RING_CAP {
            ring.push(3415.0, now);
        }
        for _ in 0..LATENCY_RING_CAP {
            ring.push(5.0, now);
        }

        assert_eq!(ring.samples.len(), LATENCY_RING_CAP);
        assert_eq!(ring.summary(now), (5.0, 5.0));
    }

    #[test]
    fn expired_latency_does_not_reset_lifetime_counters() {
        let recorder = IpcMetricsRecorder::new();
        recorder.handle().begin_request(64).complete(128);
        {
            let mut ring = recorder.shared.latencies.lock().unwrap();
            ring.samples[0] = (Instant::now() - LATENCY_WINDOW, 3415.0);
        }

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.avg_latency_ms, 0.0);
        assert_eq!(snapshot.p95_latency_ms, 0.0);
        assert_eq!(snapshot.requests_total, 1);
        assert_eq!(snapshot.messages_total, 1);
        assert_eq!(snapshot.bytes_in_total, 64);
        assert_eq!(snapshot.bytes_out_total, 128);
        assert_eq!(snapshot.drops_total, 0);
        assert_eq!(snapshot.inflight, 0);

        let bottleneck = super::super::classify(
            &nw_core::stats::ProcessMetrics::default(),
            &nw_core::stats::GpuStatus::unavailable("test"),
            &snapshot,
            &[],
        );
        assert_eq!(bottleneck.severity, nw_core::stats::Severity::Ok);
    }
}
