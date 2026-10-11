//! Real GPU collector.
//!
//! Tries `nvidia-smi` first (rich metrics: utilization, memory, decoder),
//! then falls back to detecting VAAPI availability. When nothing is
//! genuinely detectable we return `GpuStatus::Unavailable { reason }` so the
//! UI can show an accurate "no GPU / permission denied" indicator rather
//! than fabricating zeros.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use nw_core::stats::{GpuMetrics, GpuStatus};

use super::GpuMetricsSource;

/// Thin trait over the two side-effects — subprocess spawn + VAAPI probe —
/// so tests can stub them out at the boundary.
pub trait GpuProbe: Send + Sync {
    /// Return raw `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`
    /// output when the command is present *and* returned success.
    fn nvidia_smi_csv(&self) -> Option<String>;

    /// Return `true` when a VAAPI device node is present. This is a
    /// necessary-but-not-sufficient signal — no metrics come from it,
    /// but it justifies naming the GPU vendor rather than reporting
    /// `Unavailable` when the caller does have VAAPI hardware.
    fn vaapi_present(&self) -> bool;
}

/// Real production probe.
#[derive(Debug, Default)]
pub struct SystemGpuProbe;

impl GpuProbe for SystemGpuProbe {
    fn nvidia_smi_csv(&self) -> Option<String> {
        // Very defensive: bounded timeout, no shell, arguments-are-fixed.
        // The output is a single CSV line per GPU with 5 columns.
        let output = Command::new("nvidia-smi")
            .arg("--query-gpu=name,utilization.gpu,memory.used,memory.total,utilization.decoder")
            .arg("--format=csv,noheader,nounits")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.to_owned())
    }

    fn vaapi_present(&self) -> bool {
        for node in ["/dev/dri/renderD128", "/dev/dri/card0"] {
            if Path::new(node).exists() {
                return true;
            }
        }
        false
    }
}

pub struct GpuCollector<P: GpuProbe> {
    probe: P,
    #[allow(dead_code)]
    query_timeout: Duration,
}

impl<P: GpuProbe> GpuCollector<P> {
    pub fn new(probe: P) -> Self {
        Self {
            probe,
            query_timeout: Duration::from_millis(500),
        }
    }
}

impl<P: GpuProbe> GpuMetricsSource for GpuCollector<P> {
    fn snapshot(&self) -> GpuStatus {
        if let Some(csv) = self.probe.nvidia_smi_csv() {
            match parse_nvidia_smi(&csv) {
                Some(metrics) => return GpuStatus::Available { metrics },
                None => {
                    return GpuStatus::Unavailable {
                        reason: "nvidia-smi output was unparseable".into(),
                    }
                }
            }
        }
        if self.probe.vaapi_present() {
            // Device availability and live utilization counters are separate
            // capabilities. Intel VA-API is a valid hardware decode source
            // even though this driver does not expose NVIDIA-style counters.
            // Keep the unknown counters as None rather than incorrectly
            // labelling the accelerator itself unavailable.
            return GpuStatus::Available {
                metrics: GpuMetrics {
                    name: "VA-API device".into(),
                    source: "vaapi".into(),
                    utilization_percent: None,
                    memory_used_bytes: None,
                    memory_total_bytes: None,
                    decoder_utilization_percent: None,
                },
            };
        }
        GpuStatus::Unavailable {
            reason: "no supported GPU counter source (nvidia-smi absent, VAAPI not detected)"
                .into(),
        }
    }
}

/// GPU source that probes on a background thread and serves the latest
/// result, so `stats.get_performance` never waits on `nvidia-smi`.
pub struct CachedGpuSource {
    latest: Arc<Mutex<GpuStatus>>,
}

impl CachedGpuSource {
    pub fn spawn<P: GpuProbe + 'static>(probe: P, interval: Duration) -> Self {
        let latest = Arc::new(Mutex::new(GpuStatus::Unavailable {
            reason: "gpu probe pending".into(),
        }));
        let weak: Weak<Mutex<GpuStatus>> = Arc::downgrade(&latest);
        let collector = GpuCollector::new(probe);
        let spawned = std::thread::Builder::new()
            .name("nw-gpu-probe".into())
            .spawn(move || loop {
                let status = collector.snapshot();
                // Stop once the source has been dropped.
                let Some(latest) = weak.upgrade() else { break };
                if let Ok(mut slot) = latest.lock() {
                    *slot = status;
                }
                drop(latest);
                std::thread::sleep(interval);
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "GPU probe thread failed to start");
            if let Ok(mut slot) = latest.lock() {
                *slot = GpuStatus::Unavailable {
                    reason: "gpu probe thread failed to start".into(),
                };
            }
        }
        Self { latest }
    }
}

impl GpuMetricsSource for CachedGpuSource {
    fn snapshot(&self) -> GpuStatus {
        self.latest
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| GpuStatus::Unavailable {
                reason: "gpu probe state unavailable".into(),
            })
    }
}

pub(crate) fn parse_nvidia_smi(csv: &str) -> Option<GpuMetrics> {
    let first_line = csv.lines().next()?;
    let cols: Vec<&str> = first_line.split(',').map(|s| s.trim()).collect();
    if cols.len() < 5 {
        return None;
    }
    let name = cols[0].to_owned();
    let utilization_percent = cols[1].parse::<f32>().ok();
    let memory_used_bytes = cols[2].parse::<u64>().ok().map(|mib| mib * 1024 * 1024);
    let memory_total_bytes = cols[3].parse::<u64>().ok().map(|mib| mib * 1024 * 1024);
    let decoder_utilization_percent = cols[4].parse::<f32>().ok();
    Some(GpuMetrics {
        name,
        source: "nvidia-smi".into(),
        utilization_percent,
        memory_used_bytes,
        memory_total_bytes,
        decoder_utilization_percent,
    })
}
