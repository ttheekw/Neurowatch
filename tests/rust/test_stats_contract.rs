//! Wire-contract tests for the performance-observability data types.
//!
//! These types are the stable JSON-RPC surface for `stats.get_performance`.
//! Downstream consumers (the desktop UI, external scrapers) depend on the
//! exact field names and enum-tag layout — if this test drifts, the wire
//! format has silently broken.

use nw_core::stats::{
    Bottleneck, BottleneckKind, GpuMetrics, GpuStatus, IpcMetrics, PerCameraHealth,
    PerformanceSnapshot, ProcessBreakdown, ProcessMetrics, Severity,
};
use serde_json::json;

#[test]
fn snapshot_round_trips_through_json() {
    let snap = PerformanceSnapshot {
        captured_at_utc: "2026-09-16T12:00:00Z".into(),
        process: ProcessMetrics {
            cpu_percent: 12.5,
            rss_bytes: 1_048_576,
            memory_percent: 0.5,
            system_cpu_percent: Some(31.0),
            system_memory_percent: Some(52.0),
            network: None,
            siblings: vec![ProcessBreakdown {
                label: "gst-worker".into(),
                pid: 42,
                cpu_percent: 3.2,
                rss_bytes: 65_536,
            }],
        },
        gpu: GpuStatus::Available {
            metrics: GpuMetrics {
                name: "Test GPU".into(),
                source: "nvidia-smi".into(),
                utilization_percent: Some(40.0),
                memory_used_bytes: Some(512),
                memory_total_bytes: Some(1024),
                decoder_utilization_percent: Some(25.0),
            },
        },
        ipc: IpcMetrics {
            requests_total: 4,
            messages_total: 4,
            bytes_in_total: 128,
            bytes_out_total: 512,
            avg_latency_ms: 2.5,
            p95_latency_ms: 5.0,
            drops_total: 0,
            backpressure_events_total: 0,
            inflight: 0,
        },
        cameras: vec![PerCameraHealth {
            camera_id: "cam-1".into(),
            renderer_fps: Some(25.0),
            ingest_fps: None,
            decoder_fps: 25.0,
            renderer_drops: 0,
            decoder_drops: 0,
            end_to_end_latency_ms: Some(40.0),
            state: "streaming".into(),
        }],
        bottleneck: Bottleneck {
            kind: BottleneckKind::None,
            severity: Severity::Ok,
            explanation: "healthy".into(),
        },
    };

    let value = serde_json::to_value(&snap).expect("snapshot serializes");
    let reparsed: PerformanceSnapshot =
        serde_json::from_value(value.clone()).expect("snapshot deserializes");
    assert_eq!(reparsed, snap);
}

#[test]
fn gpu_status_uses_tagged_enum_layout() {
    let available = serde_json::to_value(GpuStatus::Available {
        metrics: GpuMetrics {
            name: "T".into(),
            source: "vaapi".into(),
            utilization_percent: None,
            memory_used_bytes: None,
            memory_total_bytes: None,
            decoder_utilization_percent: None,
        },
    })
    .unwrap();
    assert_eq!(available["status"], json!("available"));
    assert_eq!(available["metrics"]["source"], json!("vaapi"));

    let unavailable = serde_json::to_value(GpuStatus::unavailable("permission_denied")).unwrap();
    assert_eq!(unavailable["status"], json!("unavailable"));
    assert_eq!(unavailable["reason"], json!("permission_denied"));
}

#[test]
fn bottleneck_kind_serializes_snake_case() {
    let value = serde_json::to_value(BottleneckKind::DecodeRender).unwrap();
    assert_eq!(value, json!("decode_render"));
    let value = serde_json::to_value(Severity::Critical).unwrap();
    assert_eq!(value, json!("critical"));
}
