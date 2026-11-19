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

#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::stats::GpuMetrics;

    fn process(cpu: Option<f32>, memory: Option<f32>) -> ProcessMetrics {
        ProcessMetrics {
            system_cpu_percent: cpu,
            system_memory_percent: memory,
            ..Default::default()
        }
    }

    fn gpu(utilization: Option<f32>) -> GpuStatus {
        GpuStatus::Available {
            metrics: GpuMetrics {
                name: "test".into(),
                source: "test".into(),
                utilization_percent: utilization,
                memory_used_bytes: None,
                memory_total_bytes: None,
                decoder_utilization_percent: None,
            },
        }
    }

    fn camera(decoder_fps: f32, renderer_fps: Option<f32>) -> PerCameraHealth {
        PerCameraHealth {
            camera_id: "cam".into(),
            renderer_fps,
            ingest_fps: None,
            decoder_fps,
            renderer_drops: 0,
            decoder_drops: 0,
            end_to_end_latency_ms: None,
            state: "streaming".into(),
        }
    }

    fn run(
        process: &ProcessMetrics,
        gpu: &GpuStatus,
        ipc: &IpcMetrics,
        cameras: &[PerCameraHealth],
    ) -> Bottleneck {
        classify(process, gpu, ipc, cameras)
    }

    const NO_GPU: GpuStatus = GpuStatus::Unavailable {
        reason: String::new(),
    };

    #[test]
    fn idle_or_unmeasured_systems_report_nothing() {
        let result = run(
            &process(None, None),
            &GpuStatus::unavailable("no gpu"),
            &IpcMetrics::default(),
            &[],
        );
        assert_eq!(result.kind, BottleneckKind::None);
        assert_eq!(result.severity, Severity::Ok);
        // A GPU without a utilization counter is not a bottleneck.
        let result = run(
            &process(Some(10.0), Some(10.0)),
            &gpu(None),
            &IpcMetrics::default(),
            &[camera(25.0, None)],
        );
        assert_eq!(result.severity, Severity::Ok);
    }

    #[test]
    fn cpu_and_memory_thresholds_are_graded() {
        let ipc = IpcMetrics::default();
        let cpu = |value| run(&process(Some(value), None), &NO_GPU, &ipc, &[]);
        assert_eq!(cpu(74.0).severity, Severity::Ok);
        let warn = cpu(80.0);
        assert_eq!(
            (warn.kind, warn.severity),
            (BottleneckKind::Cpu, Severity::Warning)
        );
        assert!(warn.explanation.contains("CPU high at 80%"));
        let crit = cpu(95.0);
        assert_eq!(
            (crit.kind, crit.severity),
            (BottleneckKind::Cpu, Severity::Critical)
        );
        assert!(crit.explanation.contains("saturated"));

        // Without a system reading, the process's own (clamped) CPU is used.
        let own = ProcessMetrics {
            cpu_percent: 350.0,
            ..Default::default()
        };
        assert_eq!(run(&own, &NO_GPU, &ipc, &[]).severity, Severity::Critical);

        let memory = |value| run(&process(None, Some(value)), &NO_GPU, &ipc, &[]);
        assert_eq!(memory(50.0).severity, Severity::Ok);
        let warn = memory(85.0);
        assert_eq!(
            (warn.kind, warn.severity),
            (BottleneckKind::Ram, Severity::Warning)
        );
        assert!(warn.explanation.contains("memory high at 85%"));
        let crit = memory(97.0);
        assert_eq!(
            (crit.kind, crit.severity),
            (BottleneckKind::Ram, Severity::Critical)
        );
        let fallback = ProcessMetrics {
            memory_percent: 99.0,
            ..Default::default()
        };
        assert_eq!(run(&fallback, &NO_GPU, &ipc, &[]).kind, BottleneckKind::Ram);
    }

    #[test]
    fn the_worst_subsystem_wins_and_the_first_of_equals_is_kept() {
        let ipc = IpcMetrics::default();
        // Warning CPU, critical memory: memory wins.
        let result = run(&process(Some(80.0), Some(99.0)), &NO_GPU, &ipc, &[]);
        assert_eq!(result.kind, BottleneckKind::Ram);
        // Both critical: the one checked first stays.
        let result = run(&process(Some(99.0), Some(99.0)), &NO_GPU, &ipc, &[]);
        assert_eq!(result.kind, BottleneckKind::Cpu);
    }

    #[test]
    fn gpu_utilization_is_graded() {
        let ipc = IpcMetrics::default();
        let result = |value| run(&process(None, None), &gpu(Some(value)), &ipc, &[]);
        assert_eq!(result(50.0).severity, Severity::Ok);
        let warn = result(85.0);
        assert_eq!(
            (warn.kind, warn.severity),
            (BottleneckKind::Gpu, Severity::Warning)
        );
        assert!(warn.explanation.contains("GPU high at 85%"));
        let crit = result(99.0);
        assert_eq!(
            (crit.kind, crit.severity),
            (BottleneckKind::Gpu, Severity::Critical)
        );
        assert!(crit.explanation.contains("GPU saturated"));
    }

    #[test]
    fn rpc_slowness_and_drops_are_reported() {
        let idle = process(None, None);
        let ipc = |p95: f32, drops: u64, backpressure: u64| IpcMetrics {
            p95_latency_ms: p95,
            drops_total: drops,
            backpressure_events_total: backpressure,
            ..Default::default()
        };
        assert_eq!(
            run(&idle, &NO_GPU, &ipc(100.0, 0, 0), &[]).severity,
            Severity::Ok
        );
        let slow = run(&idle, &NO_GPU, &ipc(600.0, 0, 0), &[]);
        assert_eq!(
            (slow.kind, slow.severity),
            (BottleneckKind::Ipc, Severity::Warning)
        );
        assert!(slow.explanation.contains("RPC handlers slow: p95 600 ms"));
        let pressured = run(&idle, &NO_GPU, &ipc(10.0, 0, 3), &[]);
        assert_eq!(pressured.severity, Severity::Warning);
        assert!(pressured.explanation.contains("backpressure 3"));
        let very_slow = run(&idle, &NO_GPU, &ipc(2500.0, 0, 0), &[]);
        assert_eq!(very_slow.severity, Severity::Critical);
        assert!(very_slow.explanation.contains("very slow"));
        let dropped = run(&idle, &NO_GPU, &ipc(10.0, 4, 0), &[]);
        assert_eq!(dropped.severity, Severity::Critical);
        assert!(dropped.explanation.contains("dropped: drops 4"));
    }

    #[test]
    fn render_side_drops_are_judged_per_camera_against_the_decoder() {
        let idle = process(None, None);
        let ipc = IpcMetrics::default();
        let cameras = |list: &[PerCameraHealth]| run(&idle, &NO_GPU, &ipc, list);
        // Painting every decoded picture, or more, is healthy.
        assert_eq!(cameras(&[camera(25.0, Some(25.0))]).severity, Severity::Ok);
        assert_eq!(cameras(&[camera(20.0, Some(30.0))]).severity, Severity::Ok);
        // No decoder at all: nothing to compare.
        assert_eq!(cameras(&[camera(0.0, Some(0.0))]).severity, Severity::Ok);
        let warn = cameras(&[camera(25.0, Some(23.0))]);
        assert_eq!(
            (warn.kind, warn.severity),
            (BottleneckKind::DecodeRender, Severity::Warning)
        );
        assert!(warn
            .explanation
            .contains("camera cam render-side drops (8%)"));
        let crit = cameras(&[camera(25.0, Some(15.0))]);
        assert_eq!(crit.severity, Severity::Critical);
        assert!(crit.explanation.contains("(40%)"));
        // Many more drops in the renderer than in the decoder.
        let mut dropping = camera(25.0, Some(25.0));
        dropping.renderer_drops = 50;
        dropping.decoder_drops = 2;
        assert_eq!(cameras(&[dropping]).severity, Severity::Critical);
        let mut few = camera(25.0, Some(25.0));
        few.renderer_drops = 10;
        assert_eq!(cameras(&[few]).severity, Severity::Ok);
        // The worst camera decides.
        let worst = cameras(&[camera(25.0, Some(24.0)), camera(25.0, Some(10.0))]);
        assert_eq!(worst.severity, Severity::Critical);
    }
}
