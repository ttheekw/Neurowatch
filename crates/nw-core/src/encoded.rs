//! Compressed camera video, kept compressed.
//!
//! An [`EncodedAccessUnit`] is one picture of a camera's own H.264 or H.265
//! stream, after RTP depayloading and parsing but never decoded. Browser live
//! view forwards these unchanged; the browser decodes them. Separate from
//! [`crate::frame::VideoFrame`], which holds decoded pixels for desktop views.

/// Codecs a camera stream can be forwarded to a browser in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    /// The name in the cloud inventory contract.
    pub fn code(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
        }
    }

    /// From a GStreamer caps name (`video/x-h264`, `video/x-h265`).
    pub fn from_caps_name(name: &str) -> Option<Self> {
        match name {
            "video/x-h264" => Some(Self::H264),
            "video/x-h265" => Some(Self::H265),
            _ => None,
        }
    }
}

/// One compressed picture in Annex-B byte-stream form, with its parameter
/// sets repeated before every keyframe so a viewer can start at any one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    pub codec: VideoCodec,
    pub payload: Vec<u8>,
    /// A random-access point: decoding can start here.
    pub keyframe: bool,
    /// Coded size from the stream's parameter sets, never from decoding.
    pub width: u32,
    pub height: u32,
    /// Wall-clock arrival time in microseconds, which keeps the camera's cadence.
    pub timestamp_us: i64,
    /// Presentation time as stream time (for a recording file, the offset
    /// into that file), if known.
    pub pts_us: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_map_from_parser_caps_and_to_the_inventory_contract() {
        assert_eq!(
            VideoCodec::from_caps_name("video/x-h264"),
            Some(VideoCodec::H264)
        );
        assert_eq!(
            VideoCodec::from_caps_name("video/x-h265"),
            Some(VideoCodec::H265)
        );
        assert_eq!(VideoCodec::from_caps_name("image/jpeg"), None);
        assert_eq!(VideoCodec::H265.code(), "h265");
    }
}
