//! Per-camera renderer/decoder health registry.
//!
//! Each camera owns an `Arc<HealthCounters>`. The pipeline supervisor and the
//! shared-memory frame writer bump atomic counters on the hot path; the
//! stats aggregator turns raw counts into per-second rates by remembering
//! the last read.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nw_core::camera::CameraId;
use nw_core::stats::PerCameraHealth;

use super::CameraHealthSource;

/// Lock-free per-camera counters. Written from the GStreamer callback and
/// from the SHM writer, read from the aggregator.
#[derive(Debug, Default)]
pub struct HealthCounters {
    /// Compressed access units received by the camera's ingest (recording),
    /// whether or not anything decodes them.
    pub ingested_units: AtomicU64,
    pub decoded_frames: AtomicU64,
    pub rendered_frames: AtomicU64,
    pub decoder_drops: AtomicU64,
    pub renderer_drops: AtomicU64,
    /// Optional end-to-end latency in microseconds. `-1` means unknown.
    pub last_end_to_end_us: AtomicI64,
    /// State string is small and rarely written; separate mutex avoids
    /// dragging an atomic pointer dance into the hot path.
    state: Mutex<String>,
}

impl HealthCounters {
    pub fn record_ingested(&self) {
        self.ingested_units.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_decoded(&self) {
        self.decoded_frames.fetch_add(1, Ordering::Relaxed);
    }

    // Reported by the UI-side reader once it paints a frame; the SHM
    // writer must not call this (decode-side writes are not renders).
    #[allow(dead_code)]
    pub fn record_rendered(&self) {
        self.rendered_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_decoder_drop(&self) {
        self.decoder_drops.fetch_add(1, Ordering::Relaxed);
    }

    // Public counters intended for the UI-side SHM reader and end-to-end
    // latency probes. Kept as `pub` on the counters type so those callers
    // do not need to touch this module later.
    #[allow(dead_code)]
    pub fn record_renderer_drop(&self) {
        self.renderer_drops.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn set_end_to_end_us(&self, us: i64) {
        self.last_end_to_end_us.store(us, Ordering::Relaxed);
    }

    pub fn set_state(&self, s: impl Into<String>) {
        if let Ok(mut lock) = self.state.lock() {
            *lock = s.into();
        }
    }

    fn state_snapshot(&self) -> String {
        self.state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| String::new())
    }
}

/// Registry keyed by camera id. Concurrent-safe (multiple supervisors
/// write; a single aggregator reads).
#[derive(Debug, Default)]
pub struct StreamHealthRegistry {
    entries: Mutex<HashMap<CameraId, Entry>>,
}

#[derive(Debug)]
struct Entry {
    counters: Arc<HealthCounters>,
    last_read: Instant,
    last_ingested: u64,
    last_decoded: u64,
    last_rendered: u64,
}

impl StreamHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or create counters for a camera. Multiple callers see the same
    /// counters, so a supervisor restart preserves history rather than
    /// resetting rates to zero.
    pub fn counters_for(&self, camera_id: &CameraId) -> Arc<HealthCounters> {
        let mut entries = self.entries.lock().expect("registry mutex not poisoned");
        entries
            .entry(camera_id.clone())
            .or_insert_with(|| Entry {
                counters: Arc::new(HealthCounters::default()),
                last_read: Instant::now(),
                last_ingested: 0,
                last_decoded: 0,
                last_rendered: 0,
            })
            .counters
            .clone()
    }

    // Called when a camera is removed from the config; keeps the registry
    // bounded to only currently-configured cameras.
    pub fn forget(&self, camera_id: &CameraId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(camera_id);
        }
    }
}

impl CameraHealthSource for StreamHealthRegistry {
    fn snapshot(&self) -> Vec<PerCameraHealth> {
        let mut entries = self.entries.lock().expect("registry mutex not poisoned");
        let now = Instant::now();
        let mut out = Vec::with_capacity(entries.len());
        for (id, entry) in entries.iter_mut() {
            let ingested = entry.counters.ingested_units.load(Ordering::Relaxed);
            let decoded = entry.counters.decoded_frames.load(Ordering::Relaxed);
            let rendered = entry.counters.rendered_frames.load(Ordering::Relaxed);
            let elapsed = now.saturating_duration_since(entry.last_read).as_secs_f32();
            let elapsed = if elapsed <= 0.0 { 1.0 } else { elapsed };
            let decoder_fps = ((decoded - entry.last_decoded) as f32) / elapsed;
            // Only the UI reader reports rendered frames; until it does the
            // renderer rate is unknown rather than zero.
            let renderer_fps =
                (rendered > 0).then(|| ((rendered - entry.last_rendered) as f32) / elapsed);
            // Only a recording ingests; otherwise the ingest rate is unknown.
            let ingest_fps =
                (ingested > 0).then(|| ((ingested - entry.last_ingested) as f32) / elapsed);
            entry.last_ingested = ingested;
            entry.last_decoded = decoded;
            entry.last_rendered = rendered;
            entry.last_read = now;
            let end_to_end = entry.counters.last_end_to_end_us.load(Ordering::Relaxed);
            let end_to_end_latency_ms = if end_to_end < 0 {
                None
            } else {
                Some((end_to_end as f32) / 1000.0)
            };
            out.push(PerCameraHealth {
                camera_id: id.clone(),
                renderer_fps,
                ingest_fps,
                decoder_fps,
                renderer_drops: entry.counters.renderer_drops.load(Ordering::Relaxed),
                decoder_drops: entry.counters.decoder_drops.load(Ordering::Relaxed),
                end_to_end_latency_ms,
                state: entry.counters.state_snapshot(),
            });
        }
        out.sort_by(|a, b| a.camera_id.cmp(&b.camera_id));
        out
    }
}
