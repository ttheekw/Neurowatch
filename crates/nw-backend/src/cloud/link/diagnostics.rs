//! Live diagnostics sent to NeuroWatch Cloud while a portal page watches.
//!
//! Only what the desktop Performance page shows leaves the node: resource
//! use, the video decoder, per-camera stream health and disk space. Cameras
//! appear under the id the inventory reports them by — never a stream
//! address, credential or name. Text is clipped to the cloud's limits and the
//! lists are bounded, so one odd value cannot get a whole snapshot refused.

use std::collections::HashMap;

use nw_core::stats::{GpuStatus, PerformanceSnapshot};
use serde::Serialize;
use serde_json::{json, Value};

use crate::storage_drives::Drive;

/// Matches the cloud's `MAX_SNAPSHOT_BYTES`.
pub const MAX_DIAGNOSTICS_BYTES: usize = 65_536;
const MAX_CAMERAS: usize = 256;
const MAX_DRIVES: usize = 32;
const MAX_PROCESSES: usize = 16;
const MAX_MOUNT_POINT_CHARS: usize = 256;

/// Host facts the performance snapshot does not carry.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SystemInfo {
    pub cpu_cores: Option<u32>,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
    pub uptime_seconds: Option<u64>,
    pub load_average: Option<Vec<f32>>,
}

impl SystemInfo {
    /// Read from `/proc`; anything unreadable stays `None`.
    pub fn collect() -> Self {
        let read = |path: &str| std::fs::read_to_string(path).ok();
        let memory = read("/proc/meminfo").as_deref().and_then(parse_meminfo);
        Self {
            cpu_cores: std::thread::available_parallelism()
                .ok()
                .and_then(|n| u32::try_from(n.get()).ok()),
            memory_total_bytes: memory.map(|(total, _)| total),
            memory_available_bytes: memory.map(|(_, available)| available),
            uptime_seconds: read("/proc/uptime").as_deref().and_then(parse_uptime),
            load_average: read("/proc/loadavg").as_deref().and_then(parse_loadavg),
        }
    }
}

/// `(MemTotal, MemAvailable)` in bytes.
fn parse_meminfo(data: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        data.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .map(|kib| kib.saturating_mul(1024))
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

fn parse_uptime(data: &str) -> Option<u64> {
    let seconds: f64 = data.split_whitespace().next()?.parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds as u64)
}

fn parse_loadavg(data: &str) -> Option<Vec<f32>> {
    let loads: Vec<f32> = data
        .split_whitespace()
        .take(3)
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    (loads.len() == 3 && loads.iter().all(|l| l.is_finite() && *l >= 0.0)).then_some(loads)
}

fn clip(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// A measured rate or percentage; a value that is not a real number is sent
/// as zero rather than getting the whole snapshot refused.
fn measured(value: f32) -> f32 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    }
}

fn measured_opt(value: Option<f32>) -> Option<f32> {
    value.filter(|v| v.is_finite() && *v >= 0.0)
}

/// Build the cloud snapshot. `reported_ids` maps each local camera id to the
/// id the inventory reports it under; cameras without one are left out.
pub fn diagnostics_snapshot(
    mut perf: PerformanceSnapshot,
    video_acceleration: Value,
    reported_ids: &HashMap<String, String>,
    system: SystemInfo,
    drives: &[Drive],
) -> Value {
    let process = &mut perf.process;
    process.cpu_percent = measured(process.cpu_percent);
    process.memory_percent = measured(process.memory_percent);
    process.system_cpu_percent = measured_opt(process.system_cpu_percent);
    process.system_memory_percent = measured_opt(process.system_memory_percent);
    process.siblings.truncate(MAX_PROCESSES);
    for sibling in &mut process.siblings {
        sibling.label = clip(&sibling.label, 64);
        sibling.cpu_percent = measured(sibling.cpu_percent);
    }
    match &mut perf.gpu {
        GpuStatus::Available { metrics } => {
            metrics.name = clip(&metrics.name, 128);
            metrics.source = clip(&metrics.source, 32);
            metrics.utilization_percent = measured_opt(metrics.utilization_percent);
            metrics.decoder_utilization_percent = measured_opt(metrics.decoder_utilization_percent);
        }
        GpuStatus::Unavailable { reason } => *reason = clip(reason, 300),
    }
    perf.ipc.avg_latency_ms = measured(perf.ipc.avg_latency_ms);
    perf.ipc.p95_latency_ms = measured(perf.ipc.p95_latency_ms);
    perf.bottleneck.explanation = clip(&perf.bottleneck.explanation, 300);
    let cameras: Vec<Value> = perf
        .cameras
        .iter()
        .filter_map(|camera| {
            let id = reported_ids.get(&camera.camera_id)?;
            Some(json!({
                "camera_id": id,
                "renderer_fps": measured_opt(camera.renderer_fps),
                "decoder_fps": measured(camera.decoder_fps),
                "renderer_drops": camera.renderer_drops,
                "decoder_drops": camera.decoder_drops,
                "end_to_end_latency_ms": measured_opt(camera.end_to_end_latency_ms),
                "state": clip(&camera.state, 32),
            }))
        })
        .take(MAX_CAMERAS)
        .collect();
    let drives: Vec<Value> = drives
        .iter()
        .filter(|drive| drive.mount_point.chars().count() <= MAX_MOUNT_POINT_CHARS)
        .take(MAX_DRIVES)
        .map(|drive| {
            json!({
                "mount_point": drive.mount_point,
                "filesystem": clip(&drive.filesystem, 32),
                "total_bytes": drive.total_bytes,
                "available_bytes": drive.available_bytes,
                "recording": drive.selected,
            })
        })
        .collect();
    json!({
        "captured_at_utc": perf.captured_at_utc,
        "app_version": env!("CARGO_PKG_VERSION"),
        "process": perf.process,
        "system": system,
        "gpu": perf.gpu,
        "video_acceleration": video_acceleration,
        "ipc": perf.ipc,
        "cameras": cameras,
        "bottleneck": perf.bottleneck,
        "drives": drives,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::stats::{
        Bottleneck, GpuMetrics, IpcMetrics, PerCameraHealth, ProcessBreakdown, ProcessMetrics,
    };

    fn perf() -> PerformanceSnapshot {
        PerformanceSnapshot {
            captured_at_utc: "2026-10-02T10:00:00Z".into(),
            process: ProcessMetrics {
                cpu_percent: f32::NAN,
                rss_bytes: 1_000,
                memory_percent: 1.5,
                system_cpu_percent: Some(20.0),
                system_memory_percent: None,
                network: None,
                siblings: vec![
                    ProcessBreakdown {
                        label: "nw-backend".into(),
                        pid: 7,
                        cpu_percent: 3.0,
                        rss_bytes: 1_000,
                    };
                    20
                ],
            },
            gpu: GpuStatus::Available {
                metrics: GpuMetrics {
                    name: "G".repeat(200),
                    source: "vaapi".into(),
                    utilization_percent: None,
                    memory_used_bytes: None,
                    memory_total_bytes: None,
                    decoder_utilization_percent: Some(f32::INFINITY),
                },
            },
            ipc: IpcMetrics::default(),
            cameras: vec![
                PerCameraHealth {
                    camera_id: "front door".into(),
                    ingest_fps: None,
                    renderer_fps: None,
                    decoder_fps: 15.0,
                    renderer_drops: 0,
                    decoder_drops: 2,
                    end_to_end_latency_ms: None,
                    state: "streaming".into(),
                },
                PerCameraHealth {
                    camera_id: "removed".into(),
                    ingest_fps: None,
                    renderer_fps: None,
                    decoder_fps: 0.0,
                    renderer_drops: 0,
                    decoder_drops: 0,
                    end_to_end_latency_ms: None,
                    state: "stopped".into(),
                },
            ],
            bottleneck: Bottleneck::ok("No resource is under pressure."),
        }
    }

    fn drive(mount_point: &str, selected: bool) -> Drive {
        Drive {
            mount_point: mount_point.into(),
            device: "/dev/sda1".into(),
            filesystem: "ext4".into(),
            total_bytes: 500,
            available_bytes: 200,
            recommended: true,
            selected,
        }
    }

    #[test]
    fn cameras_use_their_reported_ids_and_unknown_ones_are_left_out() {
        let ids = HashMap::from([("front door".to_owned(), "camera-1".to_owned())]);
        let snapshot = diagnostics_snapshot(
            perf(),
            json!({"kind": "vaapi"}),
            &ids,
            SystemInfo::default(),
            &[],
        );
        let cameras = snapshot["cameras"].as_array().expect("cameras");
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0]["camera_id"], "camera-1");
        assert_eq!(cameras[0]["decoder_drops"], 2);
    }

    #[test]
    fn network_throughput_reaches_the_portal_as_totals() {
        let mut measured = perf();
        measured.process.network = Some(nw_core::stats::NetworkThroughput {
            rx_bits_per_sec: 12_340_000,
            tx_bits_per_sec: 1_024_000_000,
        });
        let snapshot = diagnostics_snapshot(
            measured,
            json!({"kind": "vaapi"}),
            &HashMap::new(),
            SystemInfo::default(),
            &[],
        );
        assert_eq!(
            snapshot["process"]["network"],
            json!({"rx_bits_per_sec": 12_340_000, "tx_bits_per_sec": 1_024_000_000})
        );
        let unmeasured = diagnostics_snapshot(
            perf(),
            json!({}),
            &HashMap::new(),
            SystemInfo::default(),
            &[],
        );
        assert!(unmeasured["process"]["network"].is_null());
    }

    #[test]
    fn values_are_bounded_and_never_not_a_number() {
        let snapshot = diagnostics_snapshot(
            perf(),
            json!({"kind": "vaapi"}),
            &HashMap::new(),
            SystemInfo::default(),
            &[],
        );
        assert_eq!(snapshot["process"]["cpu_percent"], 0.0);
        assert_eq!(
            snapshot["process"]["siblings"].as_array().map(Vec::len),
            Some(MAX_PROCESSES)
        );
        assert_eq!(
            snapshot["gpu"]["metrics"]["name"].as_str().map(str::len),
            Some(128)
        );
        assert_eq!(
            snapshot["gpu"]["metrics"]["decoder_utilization_percent"],
            Value::Null
        );
    }

    #[test]
    fn drives_say_which_one_records_and_carry_no_device_path() {
        let long = format!("/{}", "x".repeat(300));
        let drives = [
            drive("/", false),
            drive("/srv/nvr", true),
            drive(&long, false),
        ];
        let snapshot = diagnostics_snapshot(
            perf(),
            Value::Null,
            &HashMap::new(),
            SystemInfo::default(),
            &drives,
        );
        assert_eq!(
            snapshot["drives"],
            json!([
                {"mount_point": "/", "filesystem": "ext4", "total_bytes": 500, "available_bytes": 200, "recording": false},
                {"mount_point": "/srv/nvr", "filesystem": "ext4", "total_bytes": 500, "available_bytes": 200, "recording": true},
            ])
        );
        assert!(!snapshot.to_string().contains("/dev/sda1"));
    }

    #[test]
    fn snapshot_keys_pass_the_clouds_forbidden_key_list() {
        // Exact keys the cloud refuses anywhere in a node payload.
        const FORBIDDEN: &[&str] = &[
            "username",
            "password",
            "credential",
            "token",
            "port",
            "ip",
            "host",
            "hostname",
            "address",
            "endpoint",
            "media",
            "video",
            "camera",
            "storage",
        ];
        fn keys(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => map.iter().for_each(|(k, v)| {
                    out.push(k.clone());
                    keys(v, out);
                }),
                Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
                _ => {}
            }
        }
        let ids = HashMap::from([("front door".to_owned(), "camera-1".to_owned())]);
        let snapshot = diagnostics_snapshot(
            perf(),
            json!({"kind": "vaapi", "label": "VA-API", "hardware": true}),
            &ids,
            SystemInfo::collect(),
            &[drive("/", true)],
        );
        let mut found = Vec::new();
        keys(&snapshot, &mut found);
        for key in found {
            assert!(!FORBIDDEN.contains(&key.as_str()), "forbidden key {key}");
        }
    }

    #[test]
    fn proc_files_parse_into_bytes_seconds_and_loads() {
        assert_eq!(
            parse_meminfo("MemTotal: 16 kB\nMemFree: 1 kB\nMemAvailable: 8 kB\n"),
            Some((16 * 1024, 8 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 16 kB\n"), None);
        assert_eq!(parse_uptime("86400.52 1000.00\n"), Some(86_400));
        assert_eq!(
            parse_loadavg("1.20 0.90 0.70 2/300 1234\n"),
            Some(vec![1.2, 0.9, 0.7])
        );
        assert_eq!(parse_loadavg("garbage"), None);
    }
}
