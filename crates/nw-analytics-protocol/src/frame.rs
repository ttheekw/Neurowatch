//! Length-prefixed frames: what travels on a session's byte stream.
//!
//! ```text
//! u32 LE length (of what follows) | u8 kind | body
//! kind 1: a JSON-RPC message (UTF-8 JSON)
//! kind 2: a media unit: u8 version | u8 media | u16 flags | u32 binding |
//!         u64 seq | i64 time_us | i64 pts_ns | i64 dts_ns | payload
//! ```
//!
//! All integers little-endian. A missing timestamp is `i64::MIN`.
use std::io::{self, Read, Write};

/// Largest JSON message either side accepts.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Largest media unit (a 4K keyframe fits several times over).
pub const MAX_MEDIA_BYTES: usize = 16 * 1024 * 1024;

const KIND_MESSAGE: u8 = 1;
const KIND_MEDIA: u8 = 2;
const MEDIA_VERSION: u8 = 1;
const MEDIA_HEADER_BYTES: usize = 40;
const NO_TIME: i64 = i64::MIN;

/// What a unit carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// One video access unit, Annex-B, parameter sets before every keyframe.
    Video = 0,
}

/// Unit flags.
pub mod flags {
    /// A keyframe: decoding can start here.
    pub const KEYFRAME: u16 = 1;
    /// The first unit after lost units or a new camera connection: restart
    /// decoding (timestamps may start again).
    pub const DISCONTINUITY: u16 = 2;
}

/// One compressed unit of a bound camera's stream, unchanged from the camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaUnit {
    pub media: MediaKind,
    pub flags: u16,
    pub binding: u32,
    /// The frame's number on its stream: never reused while the node runs.
    pub seq: u64,
    /// When the node received it, µs since the Unix epoch (UTC).
    pub time_us: i64,
    /// Presentation and decoding times on the stream's own timeline, ns.
    pub pts_ns: Option<u64>,
    pub dts_ns: Option<u64>,
    pub data: Vec<u8>,
}

impl MediaUnit {
    pub fn keyframe(&self) -> bool {
        self.flags & flags::KEYFRAME != 0
    }
    pub fn discontinuity(&self) -> bool {
        self.flags & flags::DISCONTINUITY != 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Message(serde_json::Value),
    Media(MediaUnit),
}

/// Read one frame. `UnexpectedEof` at a frame boundary: the peer closed.
pub fn read_frame(input: &mut impl Read) -> io::Result<Frame> {
    let mut length = [0u8; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_MEDIA_BYTES + MEDIA_HEADER_BYTES + 1 {
        return Err(invalid("frame length out of range"));
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body)?;
    let kind = body[0];
    let body = &body[1..];
    match kind {
        KIND_MESSAGE => {
            if body.len() > MAX_MESSAGE_BYTES {
                return Err(invalid("message too large"));
            }
            serde_json::from_slice(body)
                .map(Frame::Message)
                .map_err(|_| invalid("message is not JSON"))
        }
        KIND_MEDIA => decode_media(body).map(Frame::Media),
        _ => Err(invalid("unknown frame kind")),
    }
}

/// Write one frame (the caller flushes when it wants).
pub fn write_frame(output: &mut impl Write, frame: &Frame) -> io::Result<()> {
    match frame {
        Frame::Message(message) => {
            let body = serde_json::to_vec(message).map_err(|_| invalid("unencodable message"))?;
            if body.len() > MAX_MESSAGE_BYTES {
                return Err(invalid("message too large"));
            }
            output.write_all(&((body.len() + 1) as u32).to_le_bytes())?;
            output.write_all(&[KIND_MESSAGE])?;
            output.write_all(&body)
        }
        Frame::Media(unit) => {
            if unit.data.len() > MAX_MEDIA_BYTES {
                return Err(invalid("media unit too large"));
            }
            let mut header = [0u8; MEDIA_HEADER_BYTES];
            header[0] = MEDIA_VERSION;
            header[1] = unit.media as u8;
            header[2..4].copy_from_slice(&unit.flags.to_le_bytes());
            header[4..8].copy_from_slice(&unit.binding.to_le_bytes());
            header[8..16].copy_from_slice(&unit.seq.to_le_bytes());
            header[16..24].copy_from_slice(&unit.time_us.to_le_bytes());
            header[24..32].copy_from_slice(&time_to_wire(unit.pts_ns).to_le_bytes());
            header[32..40].copy_from_slice(&time_to_wire(unit.dts_ns).to_le_bytes());
            let length = 1 + MEDIA_HEADER_BYTES + unit.data.len();
            output.write_all(&(length as u32).to_le_bytes())?;
            output.write_all(&[KIND_MEDIA])?;
            output.write_all(&header)?;
            output.write_all(&unit.data)
        }
    }
}

fn decode_media(body: &[u8]) -> io::Result<MediaUnit> {
    if body.len() < MEDIA_HEADER_BYTES {
        return Err(invalid("media header too short"));
    }
    if body[0] != MEDIA_VERSION {
        return Err(invalid("unknown media header version"));
    }
    let media = match body[1] {
        0 => MediaKind::Video,
        _ => return Err(invalid("unknown media kind")),
    };
    let u16_at = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
    let u32_at = |at: usize| {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(&body[at..at + 4]);
        u32::from_le_bytes(bytes)
    };
    let u64_at = |at: usize| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&body[at..at + 8]);
        u64::from_le_bytes(bytes)
    };
    Ok(MediaUnit {
        media,
        flags: u16_at(2),
        binding: u32_at(4),
        seq: u64_at(8),
        time_us: u64_at(16) as i64,
        pts_ns: time_from_wire(u64_at(24) as i64),
        dts_ns: time_from_wire(u64_at(32) as i64),
        data: body[MEDIA_HEADER_BYTES..].to_vec(),
    })
}

fn time_to_wire(time: Option<u64>) -> i64 {
    time.and_then(|t| i64::try_from(t).ok()).unwrap_or(NO_TIME)
}

fn time_from_wire(time: i64) -> Option<u64> {
    u64::try_from(time).ok()
}

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit() -> MediaUnit {
        MediaUnit {
            media: MediaKind::Video,
            flags: flags::KEYFRAME | flags::DISCONTINUITY,
            binding: 7,
            seq: 1234,
            time_us: 1_761_000_000_000_000,
            pts_ns: Some(40_000_000),
            dts_ns: None,
            data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
        }
    }

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::Message(serde_json::json!({"jsonrpc": "2.0", "method": "status", "params": {}})),
            Frame::Media(unit()),
        ];
        let mut wire = Vec::new();
        for frame in &frames {
            write_frame(&mut wire, frame).unwrap();
        }
        let mut input = wire.as_slice();
        for frame in &frames {
            assert_eq!(&read_frame(&mut input).unwrap(), frame);
        }
        let end = read_frame(&mut input).unwrap_err();
        assert_eq!(end.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn the_media_header_layout_is_fixed() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &Frame::Media(unit())).unwrap();
        assert_eq!(&wire[0..4], &(1u32 + 40 + 8).to_le_bytes());
        assert_eq!(wire[4], KIND_MEDIA);
        assert_eq!(&wire[5..7], &[1, 0]);
        assert_eq!(&wire[7..9], &3u16.to_le_bytes());
        assert_eq!(&wire[37..45], &i64::MIN.to_le_bytes());
    }

    #[test]
    fn malformed_frames_are_refused() {
        for wire in [
            vec![0, 0, 0, 0],
            vec![2, 0, 0, 0, 9, 0],
            vec![3, 0, 0, 0, KIND_MESSAGE, b'{', b'x'],
            vec![5, 0, 0, 0, KIND_MEDIA, 1, 0, 0, 0],
            (u32::MAX).to_le_bytes().to_vec(),
        ] {
            let error = read_frame(&mut wire.as_slice()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{wire:?}");
        }
    }
}
