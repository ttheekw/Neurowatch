//! Hub units (H.264 `avc`, H.265 `hvc1`/`hev1`: length-prefixed NAL units,
//! parameter sets in the caps' `codec_data`) as Annex-B access units with the
//! parameter sets repeated before every keyframe: what WebRTC sends and what a
//! viewer can start from at any keyframe. Byte rewriting only; nothing is
//! parsed beyond NAL headers and nothing is decoded.

use nw_core::encoded::VideoCodec;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Converts one stream's units (one set of caps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnexB {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Bytes of each NAL length prefix; 0 when the units are Annex-B already.
    length_size: usize,
    /// Parameter sets (VPS/SPS/PPS) from `codec_data`, without prefixes.
    parameter_sets: Vec<Vec<u8>>,
}

impl AnnexB {
    /// The converter for units with `caps`; `Err` holds the caps name of a
    /// stream that is not H.264/H.265 (it is never transcoded).
    pub fn from_caps(caps: &gstreamer::CapsRef) -> Result<Self, String> {
        let structure = caps.structure(0).ok_or_else(|| "no caps".to_owned())?;
        let name = structure.name().to_string();
        let codec = VideoCodec::from_caps_name(&name).ok_or_else(|| name.clone())?;
        let dimension = |field| {
            structure
                .get::<i32>(field)
                .ok()
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0)
        };
        let format = structure.get::<String>("stream-format").unwrap_or_default();
        let codec_data = structure
            .get::<gstreamer::Buffer>("codec_data")
            .ok()
            .and_then(|buffer| buffer.map_readable().ok().map(|map| map.to_vec()));
        let (length_size, parameter_sets) = if format == "byte-stream" {
            (0, Vec::new())
        } else {
            let data = codec_data.ok_or_else(|| format!("{name} without codec_data"))?;
            match codec {
                VideoCodec::H264 => parse_avcc(&data),
                VideoCodec::H265 => parse_hvcc(&data),
            }
            .ok_or_else(|| format!("{name} with unreadable codec_data"))?
        };
        Ok(Self {
            codec,
            width: dimension("width"),
            height: dimension("height"),
            length_size,
            parameter_sets,
        })
    }

    /// One unit as an Annex-B access unit; `None` when it is malformed.
    pub fn convert(&self, data: &[u8], keyframe: bool) -> Option<Vec<u8>> {
        if self.length_size == 0 {
            return Some(data.to_vec());
        }
        let nals = split(data, self.length_size)?;
        let has_parameters = nals.iter().any(|nal| self.is_parameter_set(nal));
        let mut inserted = !keyframe || has_parameters || self.parameter_sets.is_empty();
        let extra: usize = self.parameter_sets.iter().map(|p| p.len() + 4).sum();
        let mut out = Vec::with_capacity(data.len() + extra + 4);
        for nal in nals {
            // Parameter sets go after an access unit delimiter, before the rest.
            if !inserted && !self.is_delimiter(nal) {
                self.write_parameter_sets(&mut out);
                inserted = true;
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        if !inserted {
            self.write_parameter_sets(&mut out);
        }
        Some(out)
    }

    fn write_parameter_sets(&self, out: &mut Vec<u8>) {
        for set in &self.parameter_sets {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(set);
        }
    }

    fn nal_type(&self, nal: &[u8]) -> u8 {
        let first = nal.first().copied().unwrap_or(0);
        match self.codec {
            VideoCodec::H264 => first & 0x1f,
            VideoCodec::H265 => (first >> 1) & 0x3f,
        }
    }

    fn is_parameter_set(&self, nal: &[u8]) -> bool {
        match self.codec {
            VideoCodec::H264 => self.nal_type(nal) == 7,
            VideoCodec::H265 => matches!(self.nal_type(nal), 32 | 33),
        }
    }

    fn is_delimiter(&self, nal: &[u8]) -> bool {
        match self.codec {
            VideoCodec::H264 => self.nal_type(nal) == 9,
            VideoCodec::H265 => self.nal_type(nal) == 35,
        }
    }
}

/// The NAL units of a length-prefixed unit.
fn split(data: &[u8], length_size: usize) -> Option<Vec<&[u8]>> {
    let mut nals = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        if rest.len() < length_size {
            return None;
        }
        let (prefix, tail) = rest.split_at(length_size);
        let length = prefix
            .iter()
            .fold(0usize, |acc, byte| (acc << 8) | usize::from(*byte));
        if length > tail.len() {
            return None;
        }
        let (nal, after) = tail.split_at(length);
        if !nal.is_empty() {
            nals.push(nal);
        }
        rest = after;
    }
    Some(nals)
}

/// A big-endian u16 at `at`, and the slice after it.
fn u16_at(data: &[u8], at: usize) -> Option<usize> {
    Some(usize::from(*data.get(at)?) << 8 | usize::from(*data.get(at + 1)?))
}

/// `(length size, parameter sets)` of an `AVCDecoderConfigurationRecord`.
fn parse_avcc(data: &[u8]) -> Option<(usize, Vec<Vec<u8>>)> {
    let length_size = usize::from(*data.get(4)? & 0x03) + 1;
    let mut sets = Vec::new();
    let mut at = 5;
    // The SPS count has 5 bits (3 reserved bits set); the PPS count 8.
    for mask in [0x1f, 0xff] {
        let count = usize::from(*data.get(at)? & mask);
        at += 1;
        for _ in 0..count {
            let length = u16_at(data, at)?;
            sets.push(data.get(at + 2..at + 2 + length)?.to_vec());
            at += 2 + length;
        }
    }
    Some((length_size, sets))
}

/// `(length size, parameter sets)` of an `HEVCDecoderConfigurationRecord`.
fn parse_hvcc(data: &[u8]) -> Option<(usize, Vec<Vec<u8>>)> {
    let length_size = usize::from(*data.get(21)? & 0x03) + 1;
    let arrays = usize::from(*data.get(22)?);
    let mut sets = Vec::new();
    let mut at = 23;
    for _ in 0..arrays {
        let kind = *data.get(at)? & 0x3f;
        let count = u16_at(data, at + 1)?;
        at += 3;
        for _ in 0..count {
            let length = u16_at(data, at)?;
            let nal = data.get(at + 2..at + 2 + length)?;
            // VPS, SPS and PPS; SEI arrays are left out.
            if matches!(kind, 32..=34) {
                sets.push(nal.to_vec());
            }
            at += 2 + length;
        }
    }
    Some((length_size, sets))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 4] = [0x67, 0x42, 0x00, 0x1e];
    const PPS: [u8; 3] = [0x68, 0xce, 0x3c];
    const AUD: [u8; 2] = [0x09, 0xf0];
    const IDR: [u8; 3] = [0x65, 0x88, 0x84];
    const SLICE: [u8; 3] = [0x41, 0x9a, 0x02];

    fn avcc() -> Vec<u8> {
        let mut data = vec![1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0, SPS.len() as u8];
        data.extend_from_slice(&SPS);
        data.extend_from_slice(&[1, 0, PPS.len() as u8]);
        data.extend_from_slice(&PPS);
        data
    }

    fn caps(name: &str, format: &str, codec_data: &[u8]) -> gstreamer::Caps {
        gstreamer::init().unwrap();
        gstreamer::Caps::builder(name)
            .field("stream-format", format)
            .field("alignment", "au")
            .field("width", 640i32)
            .field("height", 360i32)
            .field(
                "codec_data",
                gstreamer::Buffer::from_slice(codec_data.to_vec()),
            )
            .build()
    }

    fn prefixed(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| {
                let mut unit = (nal.len() as u32).to_be_bytes().to_vec();
                unit.extend_from_slice(nal);
                unit
            })
            .collect()
    }

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| {
                let mut unit = START_CODE.to_vec();
                unit.extend_from_slice(nal);
                unit
            })
            .collect()
    }

    #[test]
    fn h264_keyframes_carry_their_parameter_sets_after_the_delimiter() {
        let converter = AnnexB::from_caps(&caps("video/x-h264", "avc", &avcc())).unwrap();
        assert_eq!(
            (converter.codec, converter.width, converter.height),
            (VideoCodec::H264, 640, 360)
        );
        let keyframe = converter.convert(&prefixed(&[&AUD, &IDR]), true).unwrap();
        assert_eq!(keyframe, annex_b(&[&AUD, &SPS, &PPS, &IDR]));
        let delta = converter
            .convert(&prefixed(&[&AUD, &SLICE]), false)
            .unwrap();
        assert_eq!(delta, annex_b(&[&AUD, &SLICE]), "only keyframes get them");
        // A keyframe that has its own is left as it is.
        let own = converter
            .convert(&prefixed(&[&SPS, &PPS, &IDR]), true)
            .unwrap();
        assert_eq!(own, annex_b(&[&SPS, &PPS, &IDR]));
    }

    #[test]
    fn h265_parameter_sets_come_from_hvcc_arrays() {
        let (vps, sps, pps, sei) = (
            [0x40, 0x01, 0x0c],
            [0x42, 0x01, 0x01],
            [0x44, 0x01, 0xc0],
            [0x4e, 0x01, 0x05],
        );
        let mut hvcc = vec![1; 21];
        hvcc.push(0x0f); // length size 4
        hvcc.push(4); // arrays
        for (kind, nal) in [(32u8, &vps), (33, &sps), (34, &pps), (39, &sei)] {
            hvcc.extend_from_slice(&[0x80 | kind, 0, 1, 0, nal.len() as u8]);
            hvcc.extend_from_slice(nal);
        }
        let converter = AnnexB::from_caps(&caps("video/x-h265", "hvc1", &hvcc)).unwrap();
        let idr = [0x26, 0x01, 0xaf];
        let keyframe = converter.convert(&prefixed(&[&idr]), true).unwrap();
        assert_eq!(
            keyframe,
            annex_b(&[&vps, &sps, &pps, &idr]),
            "SEI arrays are not repeated"
        );
    }

    #[test]
    fn other_codecs_and_broken_units_are_refused() {
        gstreamer::init().unwrap();
        let jpeg = gstreamer::Caps::builder("image/jpeg").build();
        assert_eq!(AnnexB::from_caps(&jpeg), Err("image/jpeg".to_owned()));
        let converter = AnnexB::from_caps(&caps("video/x-h264", "avc", &avcc())).unwrap();
        let mut truncated = prefixed(&[&IDR]);
        truncated.pop();
        assert_eq!(converter.convert(&truncated, true), None);
        let stream = AnnexB::from_caps(&caps("video/x-h264", "byte-stream", &[])).unwrap();
        assert_eq!(
            stream.convert(&annex_b(&[&IDR]), true).unwrap(),
            annex_b(&[&IDR])
        );
    }
}
