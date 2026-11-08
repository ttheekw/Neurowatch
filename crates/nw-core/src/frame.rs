//! Video frame types and shared memory layout constants.

use serde::{Deserialize, Serialize};

/// Identity assigned once by a shared camera stream, before fan-out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameIdentity {
    pub seq: u64,
    pub time_us: i64,
    /// Node receipt clock for live alignment across independently clocked streams.
    /// Zero for playback/legacy frames, whose presentation timeline remains authoritative.
    #[serde(default)]
    pub observed_us: i64,
}

/// Pixel format of a VideoFrame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum PixelFormat {
    I420 = 0,   // YUV420 planar (most common decode output)
    Nv12 = 1,   // YUV420 semi-planar (NVIDIA NVDEC output)
    Rgb24 = 2,  // RGB packed (for annotated V3 frames)
    Rgba32 = 3, // RGBA packed (for overlay-composited frames)
    BGRx32 = 4, // BGRx packed — GStreamer BGRx; maps to Qt Format_RGB32 (little-endian)
}

/// A single decoded video frame.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub timestamp_us: i64,
    pub data: Vec<u8>,
}

/// Shared memory ring buffer layout constants.
pub mod shm {
    /// Number of frame slots per camera ring buffer.
    pub const SLOT_COUNT: usize = 8;

    /// Maximum raw frame payload size: 4K RGB24 (3840×2160×3 bytes).
    pub const MAX_FRAME_BYTES: usize = 3840 * 2160 * 3;

    /// Per-slot header: width(4) + height(4) + data_len(4) = 12 bytes.
    pub const SLOT_HEADER_BYTES: usize = 12;

    /// Total bytes per ring buffer slot (header + max payload).
    pub const SLOT_SIZE: usize = SLOT_HEADER_BYTES + MAX_FRAME_BYTES;

    /// Optional UI preview limit in milli-FPS (u32 LE); zero means unlimited.
    /// Lives in formerly reserved header space, preserving older readers/writers.
    pub const PREVIEW_RATE_OFFSET: usize = 4;
    /// Optional reader capability in reserved space; zero retains RGB.
    pub const PREVIEW_FORMAT_OFFSET: usize = 8;
    pub const NV12_CAPABILITY: u32 = 0x3156_5559; // YUV1
    /// NV12 payload has a four-byte colour descriptor. Legacy readers skip it.
    pub const NV12_SLOT_FLAG: u32 = 1 << 31;

    /// Global ring buffer header size (aligned to 64 bytes).
    pub const HEADER_BYTES: usize = 64;

    /// Optional picture identity trailer. Slot/payload offsets stay compatible
    /// with older desktop readers; each slot adds seq(u64) + UTC time(i64).
    pub const FRAME_IDENTITIES_OFFSET: usize = HEADER_BYTES + SLOT_COUNT * SLOT_SIZE;
    pub const FRAME_IDENTITY_BYTES: usize = 16;

    /// Separate optional observation trailer preserves existing identity offsets.
    pub const FRAME_OBSERVED_OFFSET: usize =
        FRAME_IDENTITIES_OFFSET + SLOT_COUNT * FRAME_IDENTITY_BYTES;

    /// Total shared memory size per camera, including both trailers.
    pub const TOTAL_BYTES: usize = FRAME_OBSERVED_OFFSET + SLOT_COUNT * 8;

    /// Shared memory name prefix.
    pub const SHM_PREFIX: &str = "/neurowatch_";
}
