//! Picture size from H.264 and H.265 sequence parameter sets, so a plan's
//! resolution is checked against the stream itself rather than what a
//! recorder claims.
use crate::messages::Codec;

/// The picture size the first SPS in an Annex-B access unit announces.
pub fn picture_size(codec: Codec, access_unit: &[u8]) -> Option<(u32, u32)> {
    nal_units(access_unit).find_map(|nal| match codec {
        Codec::H264 if nal.first()? & 0x1f == 7 => h264_size(nal),
        Codec::H265 if nal.len() > 2 && (nal[0] >> 1) & 0x3f == 33 => h265_size(nal),
        _ => None,
    })
}

/// The NAL units of an Annex-B byte stream (start codes removed).
pub fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = stream;
    std::iter::from_fn(move || loop {
        let start = find_start(rest)?;
        rest = &rest[start..];
        let end = find_start_code(rest).map_or(rest.len(), |(at, _)| at);
        let nal = &rest[..end];
        rest = &rest[end..];
        let nal = trim_trailing_zeros(nal);
        if !nal.is_empty() {
            return Some(nal);
        }
    })
}

/// Index just after the next start code.
fn find_start(stream: &[u8]) -> Option<usize> {
    find_start_code(stream).map(|(at, len)| at + len)
}

/// (index, length) of the next 3- or 4-byte start code.
fn find_start_code(stream: &[u8]) -> Option<(usize, usize)> {
    let at = stream.windows(3).position(|w| w == [0, 0, 1])?;
    Some(if at > 0 && stream[at - 1] == 0 {
        (at - 1, 4)
    } else {
        (at, 3)
    })
}

fn trim_trailing_zeros(nal: &[u8]) -> &[u8] {
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &nal[..end]
}

/// An SPS's RBSP: emulation prevention bytes (00 00 03) removed.
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        self.at += count;
        (self.at <= self.data.len() * 8).then_some(())
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        ((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?))
            .try_into()
            .ok()
    }

    /// Signed Exp-Golomb (only skipped here).
    fn se(&mut self) -> Option<()> {
        self.ue().map(|_| ())
    }
}

/// H.264 SPS (7.3.2.1.1) → cropped picture size.
fn h264_size(nal: &[u8]) -> Option<(u32, u32)> {
    let data = rbsp(nal);
    let mut bits = Bits::new(data.get(1..)?);
    let profile = bits.bits(8)?;
    bits.skip(16)?; // constraint flags, level
    bits.ue()?; // seq_parameter_set_id
    let mut chroma_format = 1;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format = bits.ue()?;
        if chroma_format == 3 {
            bits.bit()?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.bit()?; // qpprime_y_zero_transform_bypass_flag
        if bits.bit()? == 1 {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for i in 0..lists {
                if bits.bit()? == 1 {
                    skip_scaling_list(&mut bits, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.bit()?; // delta_pic_order_always_zero_flag
            bits.se()?; // offset_for_non_ref_pic
            bits.se()?; // offset_for_top_to_bottom_field
            for _ in 0..bits.ue()?.min(255) {
                bits.se()?;
            }
        }
        _ => {}
    }
    bits.ue()?; // max_num_ref_frames
    bits.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = bits.ue()?.checked_add(1)?;
    let height_units = bits.ue()?.checked_add(1)?;
    let frame_mbs_only = bits.bit()?;
    if frame_mbs_only == 0 {
        bits.bit()?; // mb_adaptive_frame_field_flag
    }
    bits.bit()?; // direct_8x8_inference_flag
    let mut width = width_mbs.checked_mul(16)?;
    let mut height = height_units
        .checked_mul(16)?
        .checked_mul(2 - frame_mbs_only)?;
    if bits.bit()? == 1 {
        let (left, right, top, bottom) = (bits.ue()?, bits.ue()?, bits.ue()?, bits.ue()?);
        let (sub_width, sub_height) = match chroma_format {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let crop_x = sub_width;
        let crop_y = sub_height * (2 - frame_mbs_only);
        width = width.checked_sub(crop_x * left.checked_add(right)?)?;
        height = height.checked_sub(crop_y * top.checked_add(bottom)?)?;
    }
    (width > 0 && height > 0).then_some((width, height))
}

fn skip_scaling_list(bits: &mut Bits, size: u32) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            // delta_scale: se(v), needed to know when the list ends.
            let code = i64::from(bits.ue()?);
            let delta = if code % 2 == 1 {
                (code + 1) / 2
            } else {
                -(code / 2)
            };
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// What an MP4 `hvcC` box needs from an H.265 SPS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcSps {
    pub width: u32,
    pub height: u32,
    pub chroma_format: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    /// sps_max_sub_layers_minus1.
    pub max_sub_layers_minus1: u32,
    pub temporal_id_nesting: bool,
    /// The general profile_tier_level bytes: profile space/tier/idc,
    /// compatibility flags, constraint flags, level (12 bytes).
    pub general_ptl: [u8; 12],
}

/// The size an SPS NAL unit (without its start code) announces.
pub fn sps_size(codec: Codec, nal: &[u8]) -> Option<(u32, u32)> {
    match codec {
        Codec::H264 => h264_size(nal),
        Codec::H265 => hevc_sps(nal).map(|sps| (sps.width, sps.height)),
        _ => None,
    }
}

fn h265_size(nal: &[u8]) -> Option<(u32, u32)> {
    hevc_sps(nal).map(|sps| (sps.width, sps.height))
}

/// H.265 SPS (7.3.2.2) → conformance-window picture size and `hvcC` fields.
pub fn hevc_sps(nal: &[u8]) -> Option<HevcSps> {
    let data = rbsp(nal);
    let body = data.get(2..)?;
    let mut bits = Bits::new(body);
    bits.skip(4)?; // sps_video_parameter_set_id
    let sub_layers = bits.bits(3)?; // sps_max_sub_layers_minus1
    let temporal_id_nesting = bits.bit()? == 1;
    // profile_tier_level(1, sub_layers): the general part is byte aligned.
    let general_ptl: [u8; 12] = body.get(1..13)?.try_into().ok()?;
    bits.skip(88)?; // general profile space … general_inbld/reserved
    bits.skip(8)?; // general_level_idc
    let mut present = Vec::with_capacity(sub_layers as usize);
    for _ in 0..sub_layers {
        present.push((bits.bit()?, bits.bit()?));
    }
    if sub_layers > 0 {
        bits.skip(2 * (8 - sub_layers as usize))?;
    }
    for (profile, level) in present {
        if profile == 1 {
            bits.skip(88)?;
        }
        if level == 1 {
            bits.skip(8)?;
        }
    }
    bits.ue()?; // sps_seq_parameter_set_id
    let chroma_format = bits.ue()?;
    if chroma_format == 3 {
        bits.bit()?; // separate_colour_plane_flag
    }
    let mut width = bits.ue()?;
    let mut height = bits.ue()?;
    if bits.bit()? == 1 {
        let (left, right, top, bottom) = (bits.ue()?, bits.ue()?, bits.ue()?, bits.ue()?);
        let (sub_width, sub_height) = match chroma_format {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        width = width.checked_sub(sub_width * left.checked_add(right)?)?;
        height = height.checked_sub(sub_height * top.checked_add(bottom)?)?;
    }
    let bit_depth_luma_minus8 = bits.ue()?;
    let bit_depth_chroma_minus8 = bits.ue()?;
    (width > 0 && height > 0).then_some(HevcSps {
        width,
        height,
        chroma_format,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        max_sub_layers_minus1: sub_layers,
        temporal_id_nesting,
        general_ptl,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// An Annex-B access unit: AUD, the SPS, then a fake slice.
    fn unit(sps: &str, h265: bool) -> Vec<u8> {
        let mut au = if h265 {
            vec![0, 0, 0, 1, 0x46, 0x01, 0x10]
        } else {
            vec![0, 0, 0, 1, 0x09, 0xf0]
        };
        au.extend_from_slice(&[0, 0, 1]);
        au.extend_from_slice(&unhex(sps));
        au.extend_from_slice(&[0, 0, 0, 1, if h265 { 0x26 } else { 0x65 }, 0x88, 0x84, 0x00]);
        au
    }

    // SPS from x264/x265 (ffmpeg testsrc), with the size ffprobe reports.
    const H264: &[(&str, (u32, u32))] = &[
        // Main, 1080 coded as 1088 and cropped.
        (
            "674d4028eca03c0113f2e022000003000200000300281e30632c",
            (1920, 1080),
        ),
        // High 4:4:4 (chroma_format_idc 3).
        (
            "67f4001f919b280a00b76022000003000200000300281e30632c",
            (1280, 720),
        ),
        // High.
        (
            "67640028acd940780227e5c044000003000400000300503c60c658",
            (1920, 1080),
        ),
        // Constrained baseline, no cropping.
        (
            "6742c00cd9016096c044000003000400000300503c50a920",
            (352, 288),
        ),
        // Interlaced (frame_mbs_only 0), cropped.
        (
            "67640016acd940a463cd36022000000300200000030283e28532c0",
            (646, 364),
        ),
    ];
    const H265: &[(&str, (u32, u32))] = &[
        ("420101016000000300900000030000030078a003c08010e596566924caf0168080000003008000000504", (1920, 1080)),
        // Conformance window on both axes.
        ("42010101600000030090000003000003003fa005020169d565959a4932bc05a02000000300200000030141", (638, 358)),
    ];

    #[test]
    fn h264_sizes_match_the_encoder() {
        for (sps, size) in H264 {
            assert_eq!(
                picture_size(Codec::H264, &unit(sps, false)),
                Some(*size),
                "{sps}"
            );
        }
    }

    #[test]
    fn h265_sizes_match_the_encoder() {
        for (sps, size) in H265 {
            assert_eq!(
                picture_size(Codec::H265, &unit(sps, true)),
                Some(*size),
                "{sps}"
            );
        }
    }

    #[test]
    fn hevc_details_for_mp4() {
        let nal = unhex(H265[0].0);
        let sps = hevc_sps(&nal).unwrap();
        assert_eq!((sps.width, sps.height, sps.chroma_format), (1920, 1080, 1));
        assert_eq!(
            (sps.bit_depth_luma_minus8, sps.bit_depth_chroma_minus8),
            (0, 0)
        );
        assert_eq!(sps.max_sub_layers_minus1, 0);
        // Main profile (idc 1), level 4 (120).
        assert_eq!(sps.general_ptl[0] & 0x1f, 1);
        assert_eq!(sps.general_ptl[11], 120);
        assert_eq!(sps_size(Codec::H265, &nal), Some((1920, 1080)));
        assert_eq!(sps_size(Codec::H264, &unhex(H264[0].0)), Some((1920, 1080)));
    }

    #[test]
    fn units_without_an_sps_or_with_a_broken_one_have_no_size() {
        assert_eq!(
            picture_size(Codec::H264, &[0, 0, 0, 1, 0x65, 0x88, 0x84]),
            None
        );
        assert_eq!(picture_size(Codec::H264, &[]), None);
        assert_eq!(
            picture_size(Codec::H264, &[0, 0, 1, 0x67, 0x4d]),
            None,
            "truncated"
        );
        assert_eq!(
            picture_size(Codec::H265, &unit(H264[0].0, false)),
            None,
            "wrong codec"
        );
        assert_eq!(picture_size(Codec::Aac, &unit(H264[0].0, false)), None);
        // Every prefix of a real SPS is refused or sized, never a panic.
        for (sps, _) in H264.iter().chain(H265) {
            let bytes = unhex(sps);
            for end in 0..bytes.len() {
                let mut au = vec![0, 0, 1];
                au.extend_from_slice(&bytes[..end]);
                let _ = picture_size(Codec::H264, &au);
                let _ = picture_size(Codec::H265, &au);
            }
        }
    }

    #[test]
    fn nal_units_split_on_both_start_codes() {
        let stream = [
            0, 0, 0, 1, 9, 0xf0, 0, 0, 1, 0x67, 1, 0, 0, 0, 0, 1, 0x65, 2,
        ];
        let nals: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(nals, [&[9, 0xf0][..], &[0x67, 1], &[0x65, 2]]);
    }

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(
            rbsp(&[0x67, 0, 0, 3, 1, 0, 0, 3, 0, 5]),
            [0x67, 0, 0, 1, 0, 0, 0, 5]
        );
    }
}
