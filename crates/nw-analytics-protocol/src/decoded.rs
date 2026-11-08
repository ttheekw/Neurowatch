//! Optional local decoded-picture offers. Remote transports never carry these.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawFormat {
    Rgb24,
    Nv12,
    Gray8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawLayout {
    pub width: u32,
    pub height: u32,
    pub format: RawFormat,
    /// The desktop's NV12 colour descriptor, when present (never converted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedRequest {
    pub formats: Vec<RawFormat>,
    pub max_fps: u32,
    /// Nonzero bounds also allow a larger desktop stream of this camera.
    #[serde(default)]
    pub min_width: u32,
    #[serde(default)]
    pub min_height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalityChallenge {
    pub ring_id: String,
    pub bytes: u64,
    /// Windows: a read-only handle duplicated into the verified peer process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedAvailable {
    pub binding: u32,
    pub ring: LocalityChallenge,
    pub layout: RawLayout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedWithdrawn {
    pub binding: u32,
    pub reason: String,
}
