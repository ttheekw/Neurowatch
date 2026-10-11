//! Behavior tests for the stats module.
//!
//! Deterministic — no live camera, no GPU, no subprocess. Every side
//! effect is stubbed at the collector trait boundary.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use nw_core::stats::{
    BottleneckKind, GpuMetrics, GpuStatus, IpcMetrics, PerCameraHealth, ProcessMetrics, Severity,
};
use time::OffsetDateTime;

use super::bottleneck::classify;
use super::gpu::{parse_nvidia_smi, CachedGpuSource, GpuCollector, GpuProbe};
use super::health::StreamHealthRegistry;
use super::ipc_metrics::IpcMetricsRecorder;
use super::test_support::{StaticCameras, StaticGpu, StaticIpc, StaticSystem};
use super::{
    CameraHealthSource, GpuMetricsSource, IpcMetricsSource, PerformanceAggregator,
    SystemMetricsSource,
};

fn fake_process(cpu_percent: f32, mem_percent: f32) -> ProcessMetrics {
    ProcessMetrics {
        cpu_percent,
        rss_bytes: 128 * 1024 * 1024,
        memory_percent: mem_percent,
        system_cpu_percent: Some(cpu_percent),
        system_memory_percent: Some(mem_percent),
        network: None,
        siblings: vec![],
    }
}

fn fake_ipc(p95: f32, drops: u64, backpressure: u64) -> IpcMetrics {
    IpcMetrics {
        requests_total: 1,
        messages_total: 2,
        bytes_in_total: 128,
        bytes_out_total: 256,
        avg_latency_ms: p95 / 2.0,
        p95_latency_ms: p95,
        drops_total: drops,
        backpressure_events_total: backpressure,
        inflight: 0,
    }
}

fn cam(id: &str, decoded: f32, rendered: f32) -> PerCameraHealth {
    PerCameraHealth {
        camera_id: id.into(),
        ingest_fps: None,
        renderer_fps: Some(rendered),
        decoder_fps: decoded,
        renderer_drops: 0,
        decoder_drops: 0,
        end_to_end_latency_ms: None,
        state: "streaming".into(),
    }
}

#[test]
fn classifier_reports_ok_when_all_sources_are_healthy() {
    let process = fake_process(10.0, 20.0);
    let gpu = GpuStatus::unavailable("no source");
    let ipc = fake_ipc(5.0, 0, 0);
    let cameras = vec![cam("cam-1", 25.0, 25.0)];
    let b = classify(&process, &gpu, &ipc, &cameras);
    assert_eq!(b.kind, BottleneckKind::None);
    assert_eq!(b.severity, Severity::Ok);
}

#[test]
fn classifier_promotes_cpu_when_saturated() {
    let process = fake_process(95.0, 30.0);
    let b = classify(
        &process,
        &GpuStatus::unavailable("n/a"),
        &fake_ipc(1.0, 0, 0),
        &[],
    );
    assert_eq!(b.kind, BottleneckKind::Cpu);
    assert_eq!(b.severity, Severity::Critical);
    assert!(
        b.explanation.contains("CPU"),
        "explanation must identify CPU: {}",
        b.explanation
    );
}

#[test]
fn classifier_flags_gpu_only_when_available() {
    // GPU unavailable + high CPU → still CPU wins, no GPU fabrication.
    let b = classify(
        &fake_process(92.0, 20.0),
        &GpuStatus::unavailable("no counters"),
        &fake_ipc(5.0, 0, 0),
        &[],
    );
    assert_eq!(b.kind, BottleneckKind::Cpu);

    // GPU available and saturated → GPU wins over healthy CPU/RAM.
    let b = classify(
        &fake_process(20.0, 20.0),
        &GpuStatus::Available {
            metrics: GpuMetrics {
                name: "T".into(),
                source: "nvidia-smi".into(),
                utilization_percent: Some(97.0),
                memory_used_bytes: None,
                memory_total_bytes: None,
                decoder_utilization_percent: None,
            },
        },
        &fake_ipc(5.0, 0, 0),
        &[],
    );
    assert_eq!(b.kind, BottleneckKind::Gpu);
    assert_eq!(b.severity, Severity::Critical);
}

#[test]
fn classifier_flags_ipc_drops_as_critical() {
    let b = classify(
        &fake_process(10.0, 20.0),
        &GpuStatus::unavailable("n/a"),
        &fake_ipc(20.0, 1, 0),
        &[],
    );
    assert_eq!(b.kind, BottleneckKind::Ipc);
    assert_eq!(b.severity, Severity::Critical);
}

#[test]
fn classifier_flags_recent_slow_handlers_without_claiming_a_stall() {
    let b = classify(
        &fake_process(10.0, 20.0),
        &GpuStatus::unavailable("n/a"),
        &fake_ipc(3415.0, 0, 0),
        &[],
    );
    assert_eq!(b.kind, BottleneckKind::Ipc);
    assert_eq!(b.severity, Severity::Critical);
    assert!(b.explanation.contains("3415"));
    assert!(!b.explanation.contains("stalled"));
}

#[test]
fn classifier_does_not_call_normal_pipeline_rpc_an_ipc_failure() {
    let b = classify(
        &fake_process(10.0, 20.0),
        &GpuStatus::unavailable("n/a"),
        &fake_ipc(350.0, 0, 0),
        &[],
    );
    assert_eq!(b.kind, BottleneckKind::None);
    assert_eq!(b.severity, Severity::Ok);
}

#[test]
fn classifier_flags_render_side_drop() {
    let cameras = vec![cam("cam-1", 30.0, 20.0)];
    let b = classify(
        &fake_process(10.0, 20.0),
        &GpuStatus::unavailable("n/a"),
        &fake_ipc(5.0, 0, 0),
        &cameras,
    );
    assert_eq!(b.kind, BottleneckKind::DecodeRender);
    assert_ne!(b.severity, Severity::Ok);
    assert!(b.explanation.contains("cam-1"));
}

#[test]
fn aggregator_composes_all_sources_and_stamps_time() {
    let process = fake_process(30.0, 25.0);
    let cams = vec![cam("cam-a", 25.0, 25.0)];
    let ipc = fake_ipc(10.0, 0, 0);
    let system: Arc<dyn SystemMetricsSource> = Arc::new(StaticSystem(process.clone()));
    let gpu_status = GpuStatus::unavailable("test");
    let gpu: Arc<dyn GpuMetricsSource> = Arc::new(StaticGpu(gpu_status.clone()));
    let ipc_src: Arc<dyn IpcMetricsSource> = Arc::new(StaticIpc(ipc.clone()));
    let cams_src: Arc<dyn CameraHealthSource> = Arc::new(StaticCameras(Mutex::new(cams.clone())));

    let fixed = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = Arc::new(move || fixed);
    let agg = PerformanceAggregator::new(system, gpu, ipc_src, cams_src).with_clock(clock);
    let snap = agg.snapshot();

    assert_eq!(snap.process, process);
    assert_eq!(snap.gpu, gpu_status);
    assert_eq!(snap.ipc, ipc);
    assert_eq!(snap.cameras, cams);
    assert!(
        snap.captured_at_utc.starts_with("2023-"),
        "captured_at_utc should use the injected clock: {}",
        snap.captured_at_utc
    );
    assert_eq!(snap.bottleneck.kind, BottleneckKind::None);
}

#[test]
fn ipc_recorder_messages_total_counts_each_message_once_not_twice_per_request() {
    // Regression: completion previously double-counted messages_total by
    // bumping once at begin_request and again at complete(). messages_total
    // must reflect the number of distinct JSON-RPC messages processed —
    // exactly one per received request line.
    let rec = IpcMetricsRecorder::new();
    let handle = rec.handle();
    for _ in 0..3 {
        let span = handle.begin_request(64);
        span.complete(128);
    }
    let snap = rec.snapshot();
    assert_eq!(snap.requests_total, 3);
    assert_eq!(
        snap.messages_total, 3,
        "messages_total must not double-count on completion (was {} expected 3)",
        snap.messages_total
    );
}

#[test]
fn ipc_recorder_tracks_counts_bytes_and_latency() {
    let rec = IpcMetricsRecorder::new();
    let handle = rec.handle();
    {
        let span = handle.begin_request(64);
        std::thread::sleep(std::time::Duration::from_millis(5));
        span.complete(128);
    }
    let snap = rec.snapshot();
    assert_eq!(snap.requests_total, 1);
    assert_eq!(snap.messages_total, 1);
    assert_eq!(snap.bytes_in_total, 64);
    assert_eq!(snap.bytes_out_total, 128);
    assert!(
        snap.avg_latency_ms >= 4.5,
        "avg_latency should reflect the 5ms sleep: {}",
        snap.avg_latency_ms
    );
    assert!(snap.p95_latency_ms >= snap.avg_latency_ms);
    assert_eq!(snap.drops_total, 0);
    assert_eq!(snap.inflight, 0);
}

#[test]
fn ipc_recorder_counts_drop_on_span_drop_without_completion() {
    let rec = IpcMetricsRecorder::new();
    let handle = rec.handle();
    {
        let _span = handle.begin_request(32);
        // dropped without complete() — connection aborted mid-request.
    }
    let snap = rec.snapshot();
    assert_eq!(
        snap.drops_total, 1,
        "un-completed span must count as a drop"
    );
    assert_eq!(snap.inflight, 0, "inflight must decrement even on drop");
}

#[test]
fn ipc_recorder_backpressure_counter_is_recorded() {
    let rec = IpcMetricsRecorder::new();
    rec.record_backpressure();
    rec.record_backpressure();
    let snap = rec.snapshot();
    assert_eq!(snap.backpressure_events_total, 2);
}

#[test]
fn health_registry_reports_rates_from_atomic_counters() {
    let reg = StreamHealthRegistry::new();
    let counters = reg.counters_for(&"cam-1".to_owned());
    counters.set_state("streaming");
    // Prime — the first snapshot resets the baseline; interval assumed
    // ≈ 0 so we discard its values.
    let _ = reg.snapshot();
    for _ in 0..25 {
        counters.record_decoded();
    }
    for _ in 0..20 {
        counters.record_rendered();
    }
    // Wait a moment so the rate calculation has non-zero elapsed time.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let snap = reg.snapshot();
    assert_eq!(snap.len(), 1);
    let cam = &snap[0];
    assert_eq!(cam.camera_id, "cam-1");
    assert!(
        cam.decoder_fps > 0.0,
        "decoder_fps should be non-zero after decoded frames: {}",
        cam.decoder_fps
    );
    let ren = cam
        .renderer_fps
        .expect("renderer_fps should be Some when the UI has reported rendered frames");
    assert!(ren > 0.0);
    assert!(cam.decoder_fps >= ren);
    assert_eq!(cam.state, "streaming");
}

#[test]
fn health_registry_reports_renderer_fps_none_when_never_recorded() {
    let reg = StreamHealthRegistry::new();
    let counters = reg.counters_for(&"cam-1".to_owned());
    counters.set_state("streaming");
    // Prime baseline.
    let _ = reg.snapshot();
    for _ in 0..10 {
        counters.record_decoded();
    }
    // No record_rendered() calls — the UI has not (yet) reported a frame.
    std::thread::sleep(std::time::Duration::from_millis(30));
    let snap = reg.snapshot();
    assert_eq!(snap.len(), 1);
    assert!(
        snap[0].renderer_fps.is_none(),
        "renderer_fps must be None (not fabricated from decode-side counts): {:?}",
        snap[0].renderer_fps
    );
    assert!(
        snap[0].decoder_fps > 0.0,
        "decoder_fps should still be measured from SHM writes"
    );
}

#[test]
fn health_counters_are_shared_across_registry_lookups() {
    let reg = StreamHealthRegistry::new();
    let c1 = reg.counters_for(&"cam-1".to_owned());
    let c2 = reg.counters_for(&"cam-1".to_owned());
    c1.record_decoded();
    assert_eq!(c2.decoded_frames.load(Ordering::Relaxed), 1);
}

#[test]
fn health_registry_forget_removes_camera() {
    let reg = StreamHealthRegistry::new();
    let _ = reg.counters_for(&"cam-a".to_owned());
    let _ = reg.counters_for(&"cam-b".to_owned());
    reg.forget(&"cam-a".to_owned());
    let snap = reg.snapshot();
    assert!(snap.iter().all(|c| c.camera_id != "cam-a"));
}

#[test]
fn nvidia_smi_parser_extracts_expected_fields() {
    let csv = "NVIDIA GeForce RTX 4080, 42, 3072, 16384, 30";
    let m = parse_nvidia_smi(csv).expect("csv should parse");
    assert_eq!(m.name, "NVIDIA GeForce RTX 4080");
    assert_eq!(m.source, "nvidia-smi");
    assert_eq!(m.utilization_percent, Some(42.0));
    assert_eq!(m.memory_used_bytes, Some(3072 * 1024 * 1024));
    assert_eq!(m.memory_total_bytes, Some(16384 * 1024 * 1024));
    assert_eq!(m.decoder_utilization_percent, Some(30.0));
}

#[test]
fn gpu_collector_reports_available_when_probe_returns_csv() {
    struct Stub;
    impl GpuProbe for Stub {
        fn nvidia_smi_csv(&self) -> Option<String> {
            Some("Test GPU, 10, 100, 1000, 5".into())
        }
        fn vaapi_present(&self) -> bool {
            true
        }
    }
    let collector = GpuCollector::new(Stub);
    match collector.snapshot() {
        GpuStatus::Available { metrics } => {
            assert_eq!(metrics.name, "Test GPU");
            assert_eq!(metrics.source, "nvidia-smi");
            assert_eq!(metrics.utilization_percent, Some(10.0));
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[test]
fn gpu_collector_reports_unavailable_with_reason_when_nothing_detected() {
    struct Stub;
    impl GpuProbe for Stub {
        fn nvidia_smi_csv(&self) -> Option<String> {
            None
        }
        fn vaapi_present(&self) -> bool {
            false
        }
    }
    let collector = GpuCollector::new(Stub);
    match collector.snapshot() {
        GpuStatus::Unavailable { reason } => {
            assert!(!reason.is_empty(), "reason must be non-empty");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn cached_gpu_source_does_not_block_snapshot_on_slow_probe() {
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};

    struct SlowProbe {
        calls: Arc<std::sync::atomic::AtomicU32>,
    }
    impl GpuProbe for SlowProbe {
        fn nvidia_smi_csv(&self) -> Option<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(500));
            Some("Test GPU, 10, 100, 1000, 5".into())
        }
        fn vaapi_present(&self) -> bool {
            false
        }
    }

    let calls = Arc::new(AtomicU32::new(0));
    let source = CachedGpuSource::spawn(
        SlowProbe {
            calls: Arc::clone(&calls),
        },
        Duration::from_millis(50),
    );

    // Snapshot must return immediately even though the probe would take 500ms.
    let start = Instant::now();
    let status = source.snapshot();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "snapshot must not block on the probe: took {:?}",
        elapsed
    );
    match status {
        GpuStatus::Unavailable { reason } => {
            assert!(!reason.is_empty(), "pending reason must be non-empty");
        }
        other => panic!(
            "expected Unavailable while first probe is in flight, got {:?}",
            other
        ),
    }

    // After the probe completes, the cached value should surface Available.
    std::thread::sleep(Duration::from_millis(900));
    match source.snapshot() {
        GpuStatus::Available { metrics } => {
            assert_eq!(metrics.name, "Test GPU");
        }
        other => panic!("expected Available after probe finishes, got {:?}", other),
    }
}

#[test]
fn gpu_collector_reports_available_without_fabricated_counters_for_vaapi() {
    struct Stub;
    impl GpuProbe for Stub {
        fn nvidia_smi_csv(&self) -> Option<String> {
            None
        }
        fn vaapi_present(&self) -> bool {
            true
        }
    }
    let collector = GpuCollector::new(Stub);
    match collector.snapshot() {
        GpuStatus::Available { metrics } => {
            assert_eq!(metrics.name, "VA-API device");
            assert_eq!(metrics.source, "vaapi");
            assert_eq!(metrics.utilization_percent, None);
            assert_eq!(metrics.memory_used_bytes, None);
            assert_eq!(metrics.memory_total_bytes, None);
            assert_eq!(metrics.decoder_utilization_percent, None);
        }
        other => panic!("expected Available, got {other:?}"),
    }
}
