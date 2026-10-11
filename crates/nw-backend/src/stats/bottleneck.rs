//! Actionable bottleneck classifier.
//!
//! Never fabricates values — only reports what the collectors actually
//! measured. When every source is unavailable / idle the classifier
//! reports `Ok`/`None`.

use nw_core::stats::{
    Bottleneck, BottleneckKind, GpuStatus, IpcMetrics, PerCameraHealth, ProcessMetrics, Severity,
};

const CPU_WARN: f32 = 75.0;
const CPU_CRIT: f32 = 90.0;
const MEM_WARN: f32 = 80.0;
const MEM_CRIT: f32 = 93.0;
const GPU_WARN: f32 = 80.0;
const GPU_CRIT: f32 = 95.0;
// This measures complete JSON-RPC handler time, including intentional pipeline
// start/stop work; it is not socket round-trip latency. A 100 ms threshold
// falsely marked normal camera lifecycle calls as transport failures.
const IPC_LATENCY_WARN_MS: f32 = 500.0;
const IPC_LATENCY_CRIT_MS: f32 = 2_000.0;
const RENDER_DROP_RATIO_WARN: f32 = 0.05; // 5% dropped frames
const RENDER_DROP_RATIO_CRIT: f32 = 0.15; // 15% dropped frames

pub fn classify(
    process: &ProcessMetrics,
    gpu: &GpuStatus,
    ipc: &IpcMetrics,
    cameras: &[PerCameraHealth],
) -> Bottleneck {
    // Score each candidate; the worst wins. `Critical` beats `Warning` beats
    // `Ok`. If a source is unavailable it is silently skipped rather than
    // treated as healthy or fabricated.
    let mut worst: Bottleneck = Bottleneck::ok("all monitored subsystems within normal range");

    let system_cpu = process
        .system_cpu_percent
        .unwrap_or(process.cpu_percent.min(100.0));
    if system_cpu >= CPU_CRIT {
        worst = maybe_promote(
            worst,
            Bottleneck {
                kind: BottleneckKind::Cpu,
                severity: Severity::Critical,
                explanation: format!(
                    "CPU saturated at {:.0}% — pipelines will drop frames",
                    system_cpu
                ),
            },
        );
    } else if system_cpu >= CPU_WARN {
        worst = maybe_promote(
            worst,
            Bottleneck {
                kind: BottleneckKind::Cpu,
                severity: Severity::Warning,
                explanation: format!(
                    "CPU high at {:.0}% — approaching drop threshold",
                    system_cpu
                ),
            },
        );
    }

    let mem = process
        .system_memory_percent
        .unwrap_or(process.memory_percent);
    if mem >= MEM_CRIT {
        worst = maybe_promote(
            worst,
            Bottleneck {
                kind: BottleneckKind::Ram,
                severity: Severity::Critical,
                explanation: format!("memory critical at {:.0}%", mem),
            },
        );
    } else if mem >= MEM_WARN {
        worst = maybe_promote(
            worst,
            Bottleneck {
                kind: BottleneckKind::Ram,
                severity: Severity::Warning,
                explanation: format!("memory high at {:.0}%", mem),
            },
        );
    }

    if let GpuStatus::Available { metrics } = gpu {
        if let Some(u) = metrics.utilization_percent {
            if u >= GPU_CRIT {
                worst = maybe_promote(
                    worst,
                    Bottleneck {
                        kind: BottleneckKind::Gpu,
                        severity: Severity::Critical,
                        explanation: format!("GPU saturated at {:.0}%", u),
                    },
                );
            } else if u >= GPU_WARN {
                worst = maybe_promote(
                    worst,
                    Bottleneck {
                        kind: BottleneckKind::Gpu,
                        severity: Severity::Warning,
                        explanation: format!("GPU high at {:.0}%", u),
                    },
                );
            }
        }
    }

    if ipc.p95_latency_ms >= IPC_LATENCY_CRIT_MS || ipc.drops_total > 0 {
        worst = maybe_promote(
            worst,
            Bottleneck {
                kind: BottleneckKind::Ipc,
                severity: Severity::Critical,
                explanation: if ipc.drops_total > 0 {
                    format!(
                        "RPC requests dropped: drops {}, p95 handler {:.0} ms",
                        ipc.drops_total, ipc.p95_latency_ms
                    )
                } else {
                    format!(
                        "RPC handlers very slow: p95 handler {:.0} ms",
                        ipc.p95_latency_ms
                    )
                },
            },
        );
    } else if ipc.p95_latency_ms >= IPC_LATENCY_WARN_MS || ipc.backpressure_events_total > 0 {
        worst = maybe_promote(
            worst,
            Bottleneck {
                kind: BottleneckKind::Ipc,
                severity: Severity::Warning,
                explanation: format!(
                    "RPC handlers slow: p95 {:.0} ms, backpressure {}",
                    ipc.p95_latency_ms, ipc.backpressure_events_total
                ),
            },
        );
    }

    for cam in cameras {
        // Ratio uses decoder as the denominator — the pipeline can only
        // render what the decoder produced. A rendered ratio > 1 (renderer
        // ahead of decoder) is impossible in practice; guard anyway.
        // An unmeasured renderer rate is not a render-side gap.
        let ratio = match cam.renderer_fps {
            Some(renderer_fps) => {
                let produced = cam.decoder_fps.max(renderer_fps).max(1.0);
                (cam.decoder_fps - renderer_fps).max(0.0) / produced
            }
            None => 0.0,
        };
        if ratio >= RENDER_DROP_RATIO_CRIT || cam.renderer_drops > 10 * cam.decoder_drops.max(1) {
            worst = maybe_promote(
                worst,
                Bottleneck {
                    kind: BottleneckKind::DecodeRender,
                    severity: Severity::Critical,
                    explanation: format!(
                        "camera {} render-side drops ({:.0}%)",
                        cam.camera_id,
                        ratio * 100.0
                    ),
                },
            );
        } else if ratio >= RENDER_DROP_RATIO_WARN {
            worst = maybe_promote(
                worst,
                Bottleneck {
                    kind: BottleneckKind::DecodeRender,
                    severity: Severity::Warning,
                    explanation: format!(
                        "camera {} render-side drops ({:.0}%)",
                        cam.camera_id,
                        ratio * 100.0
                    ),
                },
            );
        }
    }

    worst
}

fn maybe_promote(current: Bottleneck, candidate: Bottleneck) -> Bottleneck {
    if severity_rank(candidate.severity) > severity_rank(current.severity) {
        candidate
    } else {
        current
    }
}

fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Ok => 0,
        Severity::Warning => 1,
        Severity::Critical => 2,
    }
}
