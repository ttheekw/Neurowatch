//! Performance-observability data types.
//!
//! Wire contract for the `stats.get_performance` JSON-RPC method. These types
//! are shared between the Rust backend (`nw-backend`) that produces snapshots
//! and any consumer (the desktop UI, external scrapers) that reads them.
//!
//! All fields are credential-free and bounded: no raw frame data, no URLs,
//! no usernames/passwords, no unbounded strings.

use serde::{Deserialize, Serialize};

/// Root snapshot returned by `stats.get_performance`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerformanceSnapshot {
    /// RFC3339 UTC timestamp of when the snapshot was captured.
    pub captured_at_utc: String,
    pub process: ProcessMetrics,
    pub gpu: GpuStatus,
    pub ipc: IpcMetrics,
    pub cameras: Vec<PerCameraHealth>,
    pub bottleneck: Bottleneck,
}

/// Backend process CPU/RAM metrics plus optional system-wide context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProcessMetrics {
    /// Backend process CPU% (0-100 * N-cores worst case; caller may clamp).
    pub cpu_percent: f32,
    /// Backend process resident-set size in bytes.
    pub rss_bytes: u64,
    /// Backend process memory as % of total system memory.
    pub memory_percent: f32,
    /// System-wide CPU% (0-100), when available.
    pub system_cpu_percent: Option<f32>,
    /// System-wide memory used %, when available.
    pub system_memory_percent: Option<f32>,
    /// Per-process breakdown (backend + known children/siblings we can observe).
    pub siblings: Vec<ProcessBreakdown>,
    /// System-wide network throughput, when it can be measured (two readings
    /// at least a second apart are needed).
    #[serde(default)]
    pub network: Option<NetworkThroughput>,
}

/// Bits per second through the host's network interfaces (all of its real
/// interfaces together; loopback and virtual ones are left out).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct NetworkThroughput {
    /// Received (download).
    pub rx_bits_per_sec: u64,
    /// Sent (upload).
    pub tx_bits_per_sec: u64,
}

/// A single sibling/child process CPU/RSS breakdown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessBreakdown {
    pub label: String,
    pub pid: u32,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
}

/// GPU status is either available (with metrics) or unavailable (with reason).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GpuStatus {
    /// A real GPU accelerator was detected. Individual counters remain `None`
    /// when the source (for example VA-API) cannot expose them reliably.
    Available { metrics: GpuMetrics },
    /// GPU counters could not be collected. The reason string explains why so
    /// the UI can render a clear "unavailable / permission denied / error"
    /// state instead of fabricating zeros.
    Unavailable { reason: String },
}

impl GpuStatus {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

/// GPU metrics collected from a real hardware source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GpuMetrics {
    pub name: String,
    /// Source of the metrics — e.g. `"nvidia-smi"`, `"vaapi"`.
    pub source: String,
    pub utilization_percent: Option<f32>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    /// Video-decode engine utilization %, when the source exposes it.
    pub decoder_utilization_percent: Option<f32>,
}

/// IPC transport metrics (server + connection layer).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct IpcMetrics {
    pub requests_total: u64,
    pub messages_total: u64,
    pub bytes_in_total: u64,
    pub bytes_out_total: u64,
    pub avg_latency_ms: f32,
    pub p95_latency_ms: f32,
    pub drops_total: u64,
    pub backpressure_events_total: u64,
    pub inflight: u32,
}

/// Per-camera renderer/decoder health.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerCameraHealth {
    pub camera_id: String,
    /// Frames painted by the UI per second, else None (never fabricated
    /// from decode-side SHM writes).
    pub renderer_fps: Option<f32>,
    /// Compressed access units received per second by the camera's
    /// recording ingest, else None. Desktop decoding runs only while a
    /// desktop view shows the camera, so `decoder_fps` is 0 otherwise.
    #[serde(default)]
    pub ingest_fps: Option<f32>,
    pub decoder_fps: f32,
    pub renderer_drops: u64,
    pub decoder_drops: u64,
    /// End-to-end latency ms if measurable, else None (never fabricated).
    pub end_to_end_latency_ms: Option<f32>,
    /// Free-form supervisor state string (`streaming`, `error`, …).
    pub state: String,
}

/// The current dominant bottleneck (or `None` when nothing is stressed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bottleneck {
    pub kind: BottleneckKind,
    pub severity: Severity,
    /// Short human-readable explanation. Bounded, no dynamic user input.
    pub explanation: String,
}

impl Bottleneck {
    pub fn ok(msg: impl Into<String>) -> Self {
        Self {
            kind: BottleneckKind::None,
            severity: Severity::Ok,
            explanation: msg.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BottleneckKind {
    None,
    Cpu,
    Gpu,
    Ram,
    Ipc,
    DecodeRender,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Ok,
    Warning,
    Critical,
}
