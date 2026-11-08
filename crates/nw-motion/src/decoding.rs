//! Which decoder a camera's video goes through: a setting per camera, with the
//! same choices as NeuroWatch's own Settings → Video Wall.
//!
//! The decoder is named outright rather than left to `decodebin`, so the choice
//! is the one used and the status can say so. A choice that cannot be used never
//! stops motion detection: the camera falls back, ends on the CPU, and reports why.
use nw_analytics_protocol::messages::Codec;
use serde::{Deserialize, Serialize};

/// `decoder` in a binding's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecoderChoice {
    /// The first hardware decoder that works here, else the CPU.
    #[default]
    Auto,
    Vaapi,
    Nvdec,
    Qsv,
    V4l2,
    D3d11,
    D3d12,
    Software,
}

/// What the settings form offers on this system, with the names it shows.
pub fn choices() -> &'static [(&'static str, &'static str)] {
    if cfg!(windows) {
        &[
            ("auto", "Automatic"),
            ("nvdec", "NVIDIA NVDEC"),
            ("d3d11", "Direct3D 11"),
            ("d3d12", "Direct3D 12"),
            ("qsv", "Intel Quick Sync"),
            ("software", "CPU (software)"),
        ]
    } else {
        &[
            ("auto", "Automatic"),
            ("vaapi", "VA-API (Intel, AMD)"),
            ("nvdec", "NVIDIA NVDEC"),
            ("qsv", "Intel Quick Sync"),
            ("v4l2", "V4L2 (ARM boards)"),
            ("software", "CPU (software)"),
        ]
    }
}

/// What the status shows for the decoder in use: its name in the settings
/// form ("VA-API (Intel, AMD)", "CPU (software)"), not a GStreamer element,
/// and why it is not the one chosen, if it is not.
pub fn status_text(element: &str, note: Option<&str>) -> String {
    let shown: &str = DecoderChoice::of_element(element).map_or(element, |choice| choice.shown());
    match note {
        Some(note) => format!(
            "{shown}: {}",
            note.strip_prefix("on the CPU: ").unwrap_or(note)
        ),
        None => shown.to_owned(),
    }
}

/// `Automatic`'s order, as NeuroWatch's own.
const AUTO: [DecoderChoice; 4] = [
    DecoderChoice::Vaapi,
    DecoderChoice::Nvdec,
    DecoderChoice::D3d11,
    DecoderChoice::V4l2,
];

impl DecoderChoice {
    const ALL: [Self; 8] = [
        Self::Auto,
        Self::Vaapi,
        Self::Nvdec,
        Self::Qsv,
        Self::V4l2,
        Self::D3d11,
        Self::D3d12,
        Self::Software,
    ];

    /// The choice a decoder element belongs to.
    pub fn of_element(element: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| {
            [Codec::H264, Codec::H265]
                .into_iter()
                .any(|codec| choice.elements(codec).contains(&element))
        })
    }

    /// Its name in the settings form ([`choices`]).
    pub fn shown(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Vaapi => "VA-API (Intel, AMD)",
            Self::Nvdec => "NVIDIA NVDEC",
            Self::Qsv => "Intel Quick Sync",
            Self::V4l2 => "V4L2 (ARM boards)",
            Self::D3d11 => "Direct3D 11",
            Self::D3d12 => "Direct3D 12",
            Self::Software => "CPU (software)",
        }
    }

    /// The name people know it by.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Vaapi => "VA-API",
            Self::Nvdec => "NVDEC",
            Self::Qsv => "Quick Sync",
            Self::V4l2 => "V4L2",
            Self::D3d11 => "Direct3D 11",
            Self::D3d12 => "Direct3D 12",
            Self::Software => "CPU",
        }
    }

    /// Its elements for a codec, preferred first. Modern VA and QSV come before
    /// their legacy plugins, whose GPU memory the rest may not accept.
    fn elements(self, codec: Codec) -> &'static [&'static str] {
        let h264 = matches!(codec, Codec::H264);
        match (self, h264) {
            (Self::Auto, _) => &[],
            (Self::Vaapi, true) => &["vah264dec", "vaapih264dec"],
            (Self::Vaapi, false) => &["vah265dec", "vaapih265dec"],
            (Self::Nvdec, true) => &["nvh264dec"],
            (Self::Nvdec, false) => &["nvh265dec"],
            (Self::Qsv, true) => &["qsvh264dec", "msdkh264dec"],
            (Self::Qsv, false) => &["qsvh265dec", "msdkh265dec"],
            (Self::V4l2, true) => &["v4l2slh264dec", "v4l2h264dec"],
            (Self::V4l2, false) => &["v4l2slh265dec", "v4l2h265dec"],
            (Self::D3d11, true) => &["d3d11h264dec"],
            (Self::D3d11, false) => &["d3d11h265dec"],
            (Self::D3d12, true) => &["d3d12h264dec"],
            (Self::D3d12, false) => &["d3d12h265dec"],
            (Self::Software, true) => &["avdec_h264"],
            (Self::Software, false) => &["avdec_h265"],
        }
    }
}

/// The decoder to use, and what to tell the user if it is not the one chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub element: &'static str,
    /// Hardware that may turn out not to work; the CPU is the last resort.
    pub hardware: bool,
    pub note: Option<String>,
}

/// Choose from `choice`, skipping `failed` elements (they have let this camera
/// down) and any family this GStreamer must not use. `installed`: can GStreamer
/// create this element here. None: not even the CPU's is here.
pub fn pick(
    choice: DecoderChoice,
    codec: Codec,
    failed: &[&'static str],
    va_blocked: Option<&str>,
    installed: &dyn Fn(&str) -> bool,
) -> Option<Pick> {
    let families: &[DecoderChoice] = match choice {
        DecoderChoice::Auto => &AUTO,
        DecoderChoice::Software => &[],
        DecoderChoice::Vaapi => &[DecoderChoice::Vaapi],
        DecoderChoice::Nvdec => &[DecoderChoice::Nvdec],
        DecoderChoice::Qsv => &[DecoderChoice::Qsv],
        DecoderChoice::V4l2 => &[DecoderChoice::V4l2],
        DecoderChoice::D3d11 => &[DecoderChoice::D3d11],
        DecoderChoice::D3d12 => &[DecoderChoice::D3d12],
    };
    for family in families {
        if *family == DecoderChoice::Vaapi && va_blocked.is_some() {
            continue;
        }
        let usable = family
            .elements(codec)
            .iter()
            .find(|element| !failed.contains(element) && installed(element));
        if let Some(element) = usable {
            return Some(Pick {
                element,
                hardware: true,
                note: None,
            });
        }
    }
    let element = DecoderChoice::Software
        .elements(codec)
        .iter()
        .find(|element| installed(element))?;
    // Automatic and CPU got what they asked for: nothing was refused.
    let note = match choice {
        DecoderChoice::Auto | DecoderChoice::Software => None,
        chosen => Some(if chosen == DecoderChoice::Vaapi && va_blocked.is_some() {
            format!(
                "on the CPU: {}",
                va_blocked.unwrap_or("VA-API cannot be used")
            )
        } else if chosen.elements(codec).iter().any(|e| failed.contains(e)) {
            format!("on the CPU: {} could not decode this video", chosen.label())
        } else {
            format!("on the CPU: {} is not available here", chosen.label())
        }),
    };
    Some(Pick {
        element,
        hardware: false,
        note,
    })
}

/// GStreamer's `va` plugin leaks DMA-BUF descriptors before 1.24.6, as NeuroWatch
/// itself refuses it (`pipeline/hwaccel.rs`); a leak here ends in a dead process
/// hours later. An unknown version counts as unsafe, as it does there.
pub fn va_leaks(version: &str) -> bool {
    let mut parts = version.split('.');
    let mut next = || parts.next().and_then(|part| part.parse::<u32>().ok());
    match (next(), next(), next()) {
        (Some(1), Some(24), Some(micro)) => micro < 6,
        (Some(_), Some(_), Some(_)) => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn automatic_takes_the_first_hardware_there_is_and_ends_on_the_cpu() {
        let auto = |installed: &dyn Fn(&str) -> bool| {
            pick(DecoderChoice::Auto, Codec::H264, &[], None, installed).unwrap()
        };
        assert_eq!(auto(&all).element, "vah264dec");
        assert!(auto(&all).hardware);
        assert_eq!(
            auto(&|e| e != "vah264dec" && e != "vaapih264dec").element,
            "nvh264dec"
        );
        let cpu = auto(&|e| e == "avdec_h264");
        assert_eq!(cpu.element, "avdec_h264");
        assert!(!cpu.hardware);
        assert_eq!(cpu.note, None, "Automatic asked for nothing in particular");
    }

    #[test]
    fn each_choice_names_its_own_decoder_for_the_codec() {
        let one = |choice, codec| pick(choice, codec, &[], None, &all).unwrap().element;
        assert_eq!(one(DecoderChoice::Vaapi, Codec::H265), "vah265dec");
        assert_eq!(one(DecoderChoice::Nvdec, Codec::H264), "nvh264dec");
        assert_eq!(one(DecoderChoice::Nvdec, Codec::H265), "nvh265dec");
        assert_eq!(one(DecoderChoice::Qsv, Codec::H264), "qsvh264dec");
        assert_eq!(one(DecoderChoice::Software, Codec::H264), "avdec_h264");
        assert_eq!(one(DecoderChoice::Software, Codec::H265), "avdec_h265");
    }

    #[test]
    fn a_choice_that_cannot_be_used_falls_back_to_the_cpu_and_says_why() {
        let missing = pick(DecoderChoice::Nvdec, Codec::H264, &[], None, &|e| {
            !e.starts_with("nv")
        })
        .unwrap();
        assert_eq!(missing.element, "avdec_h264");
        assert_eq!(
            missing.note.as_deref(),
            Some("on the CPU: NVDEC is not available here")
        );
        // Never another GPU's decoder: it was not what was asked for.
        assert!(!missing.hardware);

        let failed = pick(
            DecoderChoice::Vaapi,
            Codec::H264,
            &["vah264dec", "vaapih264dec"],
            None,
            &all,
        )
        .unwrap();
        assert_eq!(failed.element, "avdec_h264");
        assert_eq!(
            failed.note.as_deref(),
            Some("on the CPU: VA-API could not decode this video")
        );
    }

    #[test]
    fn a_failed_decoder_gives_way_to_the_next_one() {
        let next = pick(DecoderChoice::Auto, Codec::H264, &["vah264dec"], None, &all).unwrap();
        assert_eq!(next.element, "vaapih264dec");
        let qsv = pick(DecoderChoice::Qsv, Codec::H264, &["qsvh264dec"], None, &all).unwrap();
        assert_eq!(qsv.element, "msdkh264dec");
    }

    #[test]
    fn a_leaking_va_plugin_is_never_used() {
        let reason = "GStreamer 1.24.2's VA-API leaks memory (fixed in 1.24.6)";
        let chosen = pick(DecoderChoice::Vaapi, Codec::H264, &[], Some(reason), &all).unwrap();
        assert_eq!(chosen.element, "avdec_h264");
        assert_eq!(
            chosen.note.as_deref(),
            Some(format!("on the CPU: {reason}").as_str())
        );
        let auto = pick(DecoderChoice::Auto, Codec::H264, &[], Some(reason), &all).unwrap();
        assert_eq!(
            auto.element, "nvh264dec",
            "Automatic moves on to the next family"
        );
        // The same versions NeuroWatch refuses.
        for version in [
            "1.24.0", "1.24.2", "1.24.5", "1.24.5.1", "", "unknown", "1.24",
        ] {
            assert!(va_leaks(version), "{version}");
        }
        for version in ["1.22.12", "1.24.6", "1.24.13", "1.26.0", "1.28.8"] {
            assert!(!va_leaks(version), "{version}");
        }
    }

    #[test]
    fn the_status_names_decoders_as_the_form_does() {
        assert_eq!(status_text("vah264dec", None), "VA-API (Intel, AMD)");
        assert_eq!(status_text("vaapih265dec", None), "VA-API (Intel, AMD)");
        assert_eq!(status_text("nvh265dec", None), "NVIDIA NVDEC");
        assert_eq!(status_text("msdkh264dec", None), "Intel Quick Sync");
        assert_eq!(status_text("v4l2slh264dec", None), "V4L2 (ARM boards)");
        assert_eq!(status_text("d3d12h265dec", None), "Direct3D 12");
        assert_eq!(status_text("avdec_h264", None), "CPU (software)");
        assert_eq!(
            status_text(
                "avdec_h264",
                Some("on the CPU: NVDEC is not available here")
            ),
            "CPU (software): NVDEC is not available here"
        );
        // Not one of ours: as it is.
        assert_eq!(status_text("somedec", None), "somedec");
    }

    #[test]
    fn nothing_at_all_is_an_answer_not_a_panic() {
        assert_eq!(
            pick(DecoderChoice::Auto, Codec::H264, &[], None, &|_| false),
            None
        );
    }

    #[test]
    fn the_form_offers_what_settings_accept() {
        for (value, label) in choices() {
            let parsed: DecoderChoice = serde_json::from_value(serde_json::json!(value)).unwrap();
            assert_eq!(
                serde_json::to_value(parsed).unwrap(),
                serde_json::json!(value)
            );
            assert!(!label.is_empty());
        }
        assert_eq!(choices()[0].0, "auto");
        for (value, label) in choices() {
            let parsed: DecoderChoice = serde_json::from_value(serde_json::json!(value)).unwrap();
            assert_eq!(
                parsed.shown(),
                *label,
                "the status names it as the form does"
            );
        }
        assert_eq!(DecoderChoice::default(), DecoderChoice::Auto);
    }
}
