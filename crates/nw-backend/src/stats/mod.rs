//! Performance observability — collectors + aggregator behind
//! `stats.get_performance`.
//!
//! Design constraints:
//! - Backend-owned, bounded, credential-free telemetry.
//! - Collectors are injectable at the system boundary so tests never depend
//!   on a live camera, GPU driver, or subprocess.
//! - Fabrication is not allowed: unavailable sources surface a clear
//!   `Unavailable { reason }` variant rather than fake zeros.

pub mod bottleneck;
pub mod gpu;
pub mod health;
pub mod ipc_metrics;
pub mod system;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use nw_core::stats::{
    Bottleneck, GpuStatus, IpcMetrics, PerCameraHealth, PerformanceSnapshot, ProcessMetrics,
};
use time::OffsetDateTime;

pub use bottleneck::classify;
pub use health::{HealthCounters, StreamHealthRegistry};
pub use ipc_metrics::{IpcMetricsHandle, IpcMetricsRecorder};

/// Injectable collector for backend process + system CPU/RAM metrics.
pub trait SystemMetricsSource: Send + Sync {
    fn snapshot(&self) -> ProcessMetrics;
}

/// Injectable collector for GPU utilization / memory / decode metrics.
pub trait GpuMetricsSource: Send + Sync {
    fn snapshot(&self) -> GpuStatus;
}

/// Injectable snapshot of per-camera renderer/decoder health.
pub trait CameraHealthSource: Send + Sync {
    fn snapshot(&self) -> Vec<PerCameraHealth>;
}

/// Injectable IPC-metrics readout.
pub trait IpcMetricsSource: Send + Sync {
    fn snapshot(&self) -> IpcMetrics;
}

/// Aggregates all sub-collectors into a single `PerformanceSnapshot`.
///
/// The aggregator owns nothing about the sources it composes — every
/// dependency is an `Arc<dyn Trait>` so tests can inject deterministic
/// fakes at the system boundary without touching production paths.
pub struct PerformanceAggregator {
    system: Arc<dyn SystemMetricsSource>,
    gpu: Arc<dyn GpuMetricsSource>,
    ipc: Arc<dyn IpcMetricsSource>,
    cameras: Arc<dyn CameraHealthSource>,
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

impl PerformanceAggregator {
    pub fn new(
        system: Arc<dyn SystemMetricsSource>,
        gpu: Arc<dyn GpuMetricsSource>,
        ipc: Arc<dyn IpcMetricsSource>,
        cameras: Arc<dyn CameraHealthSource>,
    ) -> Self {
        Self {
            system,
            gpu,
            ipc,
            cameras,
            clock: Arc::new(OffsetDateTime::now_utc),
        }
    }

    /// Override the clock used for `captured_at_utc`. Test-only escape hatch.
    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub fn snapshot(&self) -> PerformanceSnapshot {
        let process = self.system.snapshot();
        let gpu = self.gpu.snapshot();
        let ipc = self.ipc.snapshot();
        let cameras = self.cameras.snapshot();
        let bottleneck: Bottleneck = classify(&process, &gpu, &ipc, &cameras);
        let now = (self.clock)();
        let captured_at_utc = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into());
        PerformanceSnapshot {
            captured_at_utc,
            process,
            gpu,
            ipc,
            cameras,
            bottleneck,
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only in-memory sources.
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use std::sync::Mutex;

    pub struct StaticSystem(pub ProcessMetrics);
    impl SystemMetricsSource for StaticSystem {
        fn snapshot(&self) -> ProcessMetrics {
            self.0.clone()
        }
    }

    pub struct StaticGpu(pub GpuStatus);
    impl GpuMetricsSource for StaticGpu {
        fn snapshot(&self) -> GpuStatus {
            self.0.clone()
        }
    }

    pub struct StaticIpc(pub IpcMetrics);
    impl IpcMetricsSource for StaticIpc {
        fn snapshot(&self) -> IpcMetrics {
            self.0.clone()
        }
    }

    pub struct StaticCameras(pub Mutex<Vec<PerCameraHealth>>);
    impl CameraHealthSource for StaticCameras {
        fn snapshot(&self) -> Vec<PerCameraHealth> {
            self.0.lock().expect("mutex not poisoned").clone()
        }
    }
}
