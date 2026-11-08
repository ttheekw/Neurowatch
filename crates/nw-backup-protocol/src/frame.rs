//! Length-prefixed frames: what travels on a session's streams.
//!
//! ```text
//! u32 LE length (of what follows) | u8 kind | body
//! kind 1: a control message (UTF-8 JSON, see crate::messages)
//! kind 2: a media unit: u8 version | u8 media | u16 flags | u32 track |
//!         u64 seq | i64 time_us | i64 pts_ns | i64 dts_ns | payload
//! ```
//!
//! All integers little-endian; a missing timestamp is `i64::MIN`. On QUIC the
//! control stream carries messages and each unidirectional media stream starts
//! with a [`Chunk`](crate::messages::Chunk) message, then units of that one
//! track. On TLS over TCP everything shares the one stream: a chunk message
//! precedes each run of one track's units, and the recorder pings at least
//! every 5 s (TCP has no idle timeout of its own; the ingest gives up after
//! 20 s of silence).
use std::io::{self, Read};

/// Largest control message either side accepts.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// Largest media unit (a 4K keyframe fits several times over).
pub const MAX_MEDIA_BYTES: usize = 16 * 1024 * 1024;
/// Size of the length prefix.
pub const PREFIX_BYTES: usize = 4;

const KIND_MESSAGE: u8 = 1;
const KIND_MEDIA: u8 = 2;
const MEDIA_VERSION: u8 = 1;
const MEDIA_HEADER_BYTES: usize = 40;
const MAX_BODY_BYTES: usize = 1 + MEDIA_HEADER_BYTES + MAX_MEDIA_BYTES;
const NO_TIME: i64 = i64::MIN;

/// What a unit carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// One video access unit, Annex-B, parameter sets before every keyframe.
    Video = 1,
    /// One sound frame as the camera sent it (AAC raw, G.711, Opus, PCM).
    Audio = 2,
}

/// Unit flags.
pub mod flags {
    /// A keyframe: decoding can start here (every sound unit sets it).
    pub const KEYFRAME: u16 = 1;
    /// The first unit after lost units or a new camera connection: timestamps
    /// may start again, and a file should not join it to what came before.
    pub const DISCONTINUITY: u16 = 2;
}

/// One compressed unit of a backed-up track, unchanged from the camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaUnit {
    pub media: MediaKind,
    pub flags: u16,
    /// The track, as numbered by the recorder's `OpenTrack` in this session.
    pub track: u32,
    /// The unit's number in its track's epoch: 1, 2, 3… with no gaps, so the
    /// ingest can confirm ranges and a resent unit is recognised.
    pub seq: u64,
    /// When the recorder received it, µs since the Unix epoch (UTC, the
    /// recorder's clock; `Hello.clock_offset_ms` says how far that is off).
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
    /// A control message's JSON, decoded by [`crate::messages`].
    Message(Vec<u8>),
    Media(MediaUnit),
}

/// The body length a 4-byte prefix announces, checked against the limits.
pub fn body_length(prefix: [u8; PREFIX_BYTES]) -> io::Result<usize> {
    let length = u32::from_le_bytes(prefix) as usize;
    if length == 0 || length > MAX_BODY_BYTES {
        return Err(invalid("frame length out of range"));
    }
    Ok(length)
}

/// Decode a body of the length [`body_length`] gave.
pub fn decode_body(body: &[u8]) -> io::Result<Frame> {
    let (&kind, rest) = body.split_first().ok_or_else(|| invalid("empty frame"))?;
    match kind {
        KIND_MESSAGE => {
            if rest.len() > MAX_MESSAGE_BYTES {
                return Err(invalid("message too large"));
            }
            Ok(Frame::Message(rest.to_vec()))
        }
        KIND_MEDIA => decode_media(rest).map(Frame::Media),
        _ => Err(invalid("unknown frame kind")),
    }
}

/// Read one frame. `UnexpectedEof` at a frame boundary: the peer closed.
pub fn read_frame(input: &mut impl Read) -> io::Result<Frame> {
    let mut prefix = [0u8; PREFIX_BYTES];
    input.read_exact(&mut prefix)?;
    let mut body = vec![0u8; body_length(prefix)?];
    input.read_exact(&mut body)?;
    decode_body(&body)
}

/// Append one encoded frame to `output`.
pub fn encode_frame(output: &mut Vec<u8>, frame: &Frame) -> io::Result<()> {
    match frame {
        Frame::Message(message) => encode_message(output, message),
        Frame::Media(unit) => encode_media(output, unit),
    }
}

/// Append one control message's frame.
pub fn encode_message(output: &mut Vec<u8>, message: &[u8]) -> io::Result<()> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(invalid("message too large"));
    }
    output.extend_from_slice(&((message.len() + 1) as u32).to_le_bytes());
    output.push(KIND_MESSAGE);
    output.extend_from_slice(message);
    Ok(())
}

/// Append one media unit's frame.
pub fn encode_media(output: &mut Vec<u8>, unit: &MediaUnit) -> io::Result<()> {
    if unit.data.len() > MAX_MEDIA_BYTES {
        return Err(invalid("media unit too large"));
    }
    let length = 1 + MEDIA_HEADER_BYTES + unit.data.len();
    output.reserve(PREFIX_BYTES + length);
    output.extend_from_slice(&(length as u32).to_le_bytes());
    output.push(KIND_MEDIA);
    output.push(MEDIA_VERSION);
    output.push(unit.media as u8);
    output.extend_from_slice(&unit.flags.to_le_bytes());
    output.extend_from_slice(&unit.track.to_le_bytes());
    output.extend_from_slice(&unit.seq.to_le_bytes());
    output.extend_from_slice(&unit.time_us.to_le_bytes());
    output.extend_from_slice(&time_to_wire(unit.pts_ns).to_le_bytes());
    output.extend_from_slice(&time_to_wire(unit.dts_ns).to_le_bytes());
    output.extend_from_slice(&unit.data);
    Ok(())
}

fn decode_media(body: &[u8]) -> io::Result<MediaUnit> {
    if body.len() < MEDIA_HEADER_BYTES {
        return Err(invalid("media header too short"));
    }
    if body[0] != MEDIA_VERSION {
        return Err(invalid("unknown media header version"));
    }
    let media = match body[1] {
        1 => MediaKind::Video,
        2 => MediaKind::Audio,
        _ => return Err(invalid("unknown media kind")),
    };
    let u64_at = |at: usize| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&body[at..at + 8]);
        u64::from_le_bytes(bytes)
    };
    let mut track = [0u8; 4];
    track.copy_from_slice(&body[4..8]);
    Ok(MediaUnit {
        media,
        flags: u16::from_le_bytes([body[2], body[3]]),
        track: u32::from_le_bytes(track),
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

    fn unit(media: MediaKind) -> MediaUnit {
        MediaUnit {
            media,
            flags: flags::KEYFRAME | flags::DISCONTINUITY,
            track: 7,
            seq: 1234,
            time_us: 1_762_500_000_000_000,
            pts_ns: Some(40_000_000),
            dts_ns: None,
            data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
        }
    }

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::Message(br#"{"type":"ping","at_ms":1}"#.to_vec()),
            Frame::Media(unit(MediaKind::Video)),
            Frame::Media(unit(MediaKind::Audio)),
        ];
        let mut wire = Vec::new();
        for frame in &frames {
            encode_frame(&mut wire, frame).unwrap();
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
        encode_frame(&mut wire, &Frame::Media(unit(MediaKind::Audio))).unwrap();
        assert_eq!(&wire[0..4], &(1u32 + 40 + 8).to_le_bytes());
        assert_eq!(wire[4], KIND_MEDIA);
        assert_eq!(&wire[5..7], &[1, 2]);
        assert_eq!(&wire[7..9], &3u16.to_le_bytes());
        assert_eq!(&wire[9..13], &7u32.to_le_bytes());
        assert_eq!(&wire[13..21], &1234u64.to_le_bytes());
        assert_eq!(&wire[37..45], &i64::MIN.to_le_bytes());
    }

    #[test]
    fn malformed_frames_are_refused() {
        for wire in [
            vec![0, 0, 0, 0],
            vec![2, 0, 0, 0, 9, 0],
            vec![5, 0, 0, 0, KIND_MEDIA, 1, 1, 0, 0],
            (u32::MAX).to_le_bytes().to_vec(),
        ] {
            let error = read_frame(&mut wire.as_slice()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{wire:?}");
        }
        let mut wire = Vec::new();
        encode_frame(&mut wire, &Frame::Media(unit(MediaKind::Video))).unwrap();
        wire[5] = 2;
        assert!(
            read_frame(&mut wire.as_slice()).is_err(),
            "unknown header version"
        );
        let too_big = vec![b' '; MAX_MESSAGE_BYTES + 1];
        assert!(encode_message(&mut Vec::new(), &too_big).is_err());
    }
}
