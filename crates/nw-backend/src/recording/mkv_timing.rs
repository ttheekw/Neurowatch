//! Where a recording segment sits in its run, read from the file itself.
//!
//! `splitmuxsink` keeps one timeline for a whole recording run: the second
//! segment's timestamps continue where the first stopped. A segment's place
//! is therefore its first cluster's timestamp, and its length the Matroska
//! `Duration` the muxer writes when the segment is finished. Assuming every
//! segment is exactly the configured length drifted (the muxer splits at a
//! keyframe: 59.111 s files on a camera with a 2 s keyframe interval), and
//! the file's modification time is not a reliable end (seconds early on
//! Windows).
//!
//! Only the start of the file is read (`HEAD_BYTES`): the EBML header, the
//! segment's `Info` and its first `Cluster` come first in what the muxer
//! writes. A segment still being written has no `Duration` yet.

use std::io::Read;
use std::path::Path;

/// How much of a segment is read: Info and the first cluster's timestamp
/// are within the first few kilobytes; tracks with large codec headers and a
/// first keyframe that fills a cluster still fit.
const HEAD_BYTES: u64 = 1 << 20;

const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
/// Matroska's default: timestamps in milliseconds.
const DEFAULT_SCALE_NS: u64 = 1_000_000;

/// A segment's timing on its run's timeline, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentTiming {
    /// The first cluster's timestamp: where the segment starts in its run.
    pub start_ms: i64,
    /// The finished segment's length; None while it is still being written.
    pub duration_ms: Option<i64>,
}

/// The timing of the Matroska file at `path`, or None if it cannot be read.
pub fn read(path: &Path) -> Option<SegmentTiming> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(HEAD_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    parse(&head)
}

/// The timing in the first bytes of a Matroska file.
pub fn parse(data: &[u8]) -> Option<SegmentTiming> {
    let mut at = 0;
    let (id, size, body) = element(data, at)?;
    if id != EBML_HEADER {
        return None;
    }
    at = body.checked_add(size?)?;
    let (id, _segment_size, mut at_child) = element(data, at)?;
    if id != SEGMENT {
        return None;
    }
    let mut scale_ns = DEFAULT_SCALE_NS;
    let mut duration_units: Option<f64> = None;
    // The segment's children until the first cluster (a segment still being
    // written has an unknown size: read on until the data runs out).
    while let Some((id, size, body)) = element(data, at_child) {
        match id {
            INFO => {
                let end = body.checked_add(size?)?.min(data.len());
                let mut at_info = body;
                while let Some((id, size, value)) = element(data, at_info) {
                    let size = size?;
                    let bytes = data.get(value..value.checked_add(size)?)?;
                    match id {
                        TIMESTAMP_SCALE => scale_ns = unsigned(bytes)?.max(1),
                        DURATION => duration_units = float(bytes),
                        _ => {}
                    }
                    at_info = value.checked_add(size)?;
                    if at_info >= end {
                        break;
                    }
                }
            }
            CLUSTER => {
                // The cluster's own timestamp is its first child.
                let (id, size, value) = element(data, body)?;
                if id != CLUSTER_TIMESTAMP {
                    return None;
                }
                let bytes = data.get(value..value.checked_add(size?)?)?;
                let start_ns = unsigned(bytes)?.checked_mul(scale_ns)?;
                let duration_ms = duration_units
                    .filter(|units| units.is_finite() && *units > 0.0)
                    .map(|units| (units * scale_ns as f64 / 1_000_000.0).round() as i64);
                return Some(SegmentTiming {
                    start_ms: i64::try_from(start_ns / 1_000_000).ok()?,
                    duration_ms,
                });
            }
            _ => {}
        }
        at_child = body.checked_add(size?)?;
    }
    None
}

/// The element at `at`: its id, size (None when unknown) and where its data starts.
fn element(data: &[u8], at: usize) -> Option<(u32, Option<usize>, usize)> {
    let (id, id_len) = vint(data, at, true)?;
    let (size, size_len) = vint(data, at.checked_add(id_len)?, false)?;
    let id = u32::try_from(id).ok()?;
    let unknown = size_len > 0 && size == (1u64 << (7 * size_len)) - 1;
    let size = if unknown {
        None
    } else {
        Some(usize::try_from(size).ok()?)
    };
    Some((id, size, at + id_len + size_len))
}

/// An EBML variable-length integer at `at`: its value (with the length
/// marker kept for ids, removed for sizes) and its length in bytes.
fn vint(data: &[u8], at: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *data.get(at)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let bytes = data.get(at..at.checked_add(len)?)?;
    let mut value = if keep_marker {
        u64::from(first)
    } else {
        u64::from(first) & (0xFF >> len)
    };
    for byte in &bytes[1..] {
        value = (value << 8) | u64::from(*byte);
    }
    Some((value, len))
}

fn unsigned(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    Some(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

fn float(bytes: &[u8]) -> Option<f64> {
    match bytes.len() {
        4 => Some(f64::from(f32::from_be_bytes(bytes.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An element with a one-byte size.
    fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.push(0x80 | u8::try_from(body.len()).unwrap());
        out.extend_from_slice(body);
        out
    }

    fn file(info: &[u8], cluster_ts: u64, segment_size_known: bool) -> Vec<u8> {
        let mut segment_body = el(&[0x15, 0x49, 0xA9, 0x66], info);
        let ts = cluster_ts.to_be_bytes();
        let first = ts.iter().position(|b| *b != 0).unwrap_or(7);
        let cluster_body = el(&[0xE7], &ts[first..]);
        segment_body.extend(el(&[0x1F, 0x43, 0xB6, 0x75], &cluster_body));
        let mut out = el(&[0x1A, 0x45, 0xDF, 0xA3], &el(&[0x42, 0x86], &[1]));
        out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        if segment_size_known {
            out.extend_from_slice(&[0x40, u8::try_from(segment_body.len()).unwrap()]);
        } else {
            out.push(0xFF); // unknown size: still being written
        }
        out.extend(segment_body);
        out
    }

    #[test]
    fn a_finished_segment_has_its_place_and_length() {
        let mut info = el(&[0x2A, 0xD7, 0xB1], &[0x0F, 0x42, 0x40]); // 1 ms scale
        info.extend(el(&[0x44, 0x89], &59_110.8_f64.to_be_bytes()));
        let timing = parse(&file(&info, 59_111, true)).unwrap();
        assert_eq!(
            timing,
            SegmentTiming {
                start_ms: 59_111,
                duration_ms: Some(59_111)
            }
        );
    }

    #[test]
    fn a_segment_being_written_has_no_length_yet() {
        let info = el(&[0x2A, 0xD7, 0xB1], &[0x0F, 0x42, 0x40]);
        let timing = parse(&file(&info, 118_221, false)).unwrap();
        assert_eq!(timing.start_ms, 118_221);
        assert_eq!(timing.duration_ms, None);
    }

    #[test]
    fn other_timestamp_scales_are_honoured() {
        // 100 µs units: 2 s start, 1.5 s long.
        let mut info = el(&[0x2A, 0xD7, 0xB1], &[0x01, 0x86, 0xA0]);
        info.extend(el(&[0x44, 0x89], &15_000.0_f32.to_be_bytes()));
        let timing = parse(&file(&info, 20_000, true)).unwrap();
        assert_eq!(timing.start_ms, 2_000);
        assert_eq!(timing.duration_ms, Some(1_500));
    }

    #[test]
    fn anything_else_is_not_read() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"\0\0\0\x18ftypmp42"), None);
        let info = el(&[0x2A, 0xD7, 0xB1], &[0x0F, 0x42, 0x40]);
        let whole = file(&info, 5, true);
        assert_eq!(
            parse(&whole[..whole.len() - 3]),
            None,
            "cut before the cluster time"
        );
    }
}
