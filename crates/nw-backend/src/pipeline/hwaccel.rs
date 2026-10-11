//! Hardware acceleration capability probe.
//!
//! Called once at startup, before pipelines are created. Excludes known-bad
//! plugins, then selects an available decoder family. Factory presence alone
//! does not prove a driver can decode a particular stream.

use std::sync::OnceLock;

const VA_UPDATE_REQUIRED: &str =
    "local live view requires a GStreamer VA plugin with the 1.24.6 DMA-BUF fix or later";
static LIVE_VIEW_BLOCK_REASON: OnceLock<&'static str> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HwAccelKind {
    VaApi,
    NvDec,
    V4l2M2m,
    Software,
}

impl std::fmt::Display for HwAccelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VaApi => write!(f, "VA-API"),
            Self::NvDec => write!(f, "NVDEC"),
            Self::V4l2M2m => write!(f, "V4L2 M2M"),
            Self::Software => write!(f, "Software (avdec)"),
        }
    }
}

impl HwAccelKind {
    /// Stable identifier exposed over IPC to desktop and external clients.
    pub const fn wire_kind(self) -> &'static str {
        match self {
            Self::VaApi => "vaapi",
            Self::NvDec => "nvdec",
            Self::V4l2M2m => "v4l2_m2m",
            Self::Software => "software",
        }
    }

    pub const fn is_hardware(self) -> bool {
        !matches!(self, Self::Software)
    }
}

/// Probe available hardware decoders. Returns the best available option.
pub fn probe() -> HwAccelKind {
    exclude_leaking_va_plugin();
    if factory_usable("vah264dec") {
        tracing::info!("Hardware acceleration: VA-API");
        return HwAccelKind::VaApi;
    }
    if factory_usable("nvh264dec") {
        tracing::info!("Hardware acceleration: NVDEC");
        return HwAccelKind::NvDec;
    }
    if factory_usable("v4l2h264dec") {
        tracing::info!("Hardware acceleration: V4L2 M2M");
        return HwAccelKind::V4l2M2m;
    }
    tracing::info!("Hardware acceleration: Software (avdec_h264)");
    HwAccelKind::Software
}

/// Apply to the process registry, not just our explicit H.264 preference:
/// live RTSP, native viewing, playback and thumbnails all use decodebin.
/// The `va` plugin (gst-plugins-bad) is distinct from legacy `vaapi`.
fn exclude_leaking_va_plugin() {
    let registry = gstreamer::Registry::get();
    let Some(plugin) = registry.find_plugin("va") else {
        return;
    };
    // Load before changing the registry: loading cached features later would
    // register their factories again. Read the loaded plugin's own version,
    // which can differ from the GStreamer core version.
    let plugin = match plugin.load() {
        Ok(plugin) => plugin,
        Err(error) => {
            let removed = exclude_plugin_features(&registry, "va");
            block_local_live_view();
            tracing::error!(
                %error,
                removed,
                "Local live view disabled because the VA plugin version could not be verified; \
                 recording and compressed browser live view remain available"
            );
            return;
        }
    };
    let version = plugin.version();
    if va_version_needs_exclusion(&version) {
        let removed = exclude_plugin_features(&registry, "va");
        block_local_live_view();
        tracing::error!(
            plugin_version = %version,
            removed,
            "Local live view disabled: GStreamer 1.24.0-1.24.5 leaks DMA-BUF descriptors; \
             an unknown version is also blocked. Install a supported GStreamer runtime \
             containing the 1.24.6 VA fix. Recording and compressed browser live view \
             remain available"
        );
    }
}

fn block_local_live_view() {
    let _ = LIVE_VIEW_BLOCK_REASON.set(VA_UPDATE_REQUIRED);
}

/// Why decoder-backed local live view is unavailable for this process.
/// Browser live view stays compressed and does not use this path.
pub fn live_view_block_reason() -> Option<&'static str> {
    LIVE_VIEW_BLOCK_REASON.get().copied()
}

fn va_version_needs_exclusion(version: &str) -> bool {
    let mut parts = version.split('.');
    let mut next = || parts.next().and_then(|part| part.parse::<u32>().ok());
    match (next(), next(), next()) {
        (Some(1), Some(24), Some(micro)) => micro < 6,
        (Some(_), Some(_), Some(_)) => false,
        _ => true,
    }
}

fn exclude_plugin_features(registry: &gstreamer::Registry, name: &str) -> usize {
    let features = registry.features_by_plugin(name);
    let count = features.len();
    for feature in features {
        // Removing factories also prevents explicit ElementFactory::find/make
        // from selecting them. A rank-only change would only cover autoplugging.
        registry.remove_feature(&feature);
    }
    count
}

fn factory_usable(element_name: &str) -> bool {
    gstreamer::ElementFactory::find(element_name).is_some()
}

/// Returns the GStreamer element name for H.264 decode given a `HwAccelKind`.
pub fn h264_decoder(kind: HwAccelKind) -> &'static str {
    match kind {
        HwAccelKind::VaApi => "vah264dec",
        HwAccelKind::NvDec => "nvh264dec",
        HwAccelKind::V4l2M2m => "v4l2h264dec",
        HwAccelKind::Software => "avdec_h264",
    }
}

/// Returns the GStreamer element for YUV→RGB colour conversion.
///
/// On NVIDIA, `nvvidconv` performs the conversion on the GPU (zero CPU cost).
/// All other backends fall back to `videoconvert` (software, runs in the
/// Rust process rather than the Python UI process — still faster than the
/// old Python numpy path).
///
/// Note: `nvvidconv` is checked at call-time because it ships in a separate
/// package from `nvh264dec` — the decoder factory can be registered while
/// the converter is absent (different NVIDIA GStreamer plugin packages).
pub fn color_converter(kind: HwAccelKind) -> &'static str {
    match kind {
        HwAccelKind::NvDec if factory_usable("nvvidconv") => "nvvidconv",
        _ => "videoconvert",
    }
}

#[cfg(test)]
mod tests {
    use super::{h264_decoder, HwAccelKind};

    #[test]
    fn va_leak_version_boundaries() {
        for version in [
            "1.24.0", "1.24.2", "1.24.5", "1.24.5.1", "", "unknown", "1.24",
        ] {
            assert!(super::va_version_needs_exclusion(version), "{version}");
        }
        for version in ["1.22.12", "1.24.6", "1.24.13", "1.26.0", "1.28.0"] {
            assert!(!super::va_version_needs_exclusion(version), "{version}");
        }
    }

    #[test]
    fn excluded_plugin_is_unavailable_to_explicit_and_automatic_selection() {
        use gstreamer::prelude::*;

        // Registry changes must not affect other concurrently running tests.
        const CHILD: &str = "NW_TEST_PLUGIN_EXCLUSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pipeline::hwaccel::tests::excluded_plugin_is_unavailable_to_explicit_and_automatic_selection",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            return;
        }
        gstreamer::init().unwrap();
        // Use libav to exercise registry exclusion without requiring a GPU.
        gstreamer::Plugin::load_by_name("libav").expect("libav is a runtime dependency");
        let registry = gstreamer::Registry::get();
        assert!(gstreamer::ElementFactory::find("avdec_h264").is_some());
        let candidates = || {
            gstreamer::ElementFactory::factories_with_type(
                gstreamer::ElementFactoryType::DECODER,
                gstreamer::Rank::MARGINAL,
            )
            .iter()
            .any(|factory| factory.plugin_name().as_deref() == Some("libav"))
        };
        assert!(candidates());
        assert!(super::exclude_plugin_features(&registry, "libav") > 0);
        assert!(!candidates());
        assert!(gstreamer::ElementFactory::find("avdec_h264").is_none());
        // Loading an already-loaded plugin must not restore excluded features.
        gstreamer::Plugin::load_by_name("libav").unwrap();
        assert!(gstreamer::ElementFactory::find("avdec_h264").is_none());
        assert!(gstreamer::ElementFactory::find("decodebin").is_some());
        assert!(gstreamer::ElementFactory::find("matroskamux").is_some());
        assert_eq!(super::exclude_plugin_features(&registry, "libav"), 0);
    }

    #[test]
    fn strict_policy_blocks_local_live_view_in_an_isolated_process() {
        const CHILD: &str = "NW_TEST_STRICT_VA_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pipeline::hwaccel::tests::strict_policy_blocks_local_live_view_in_an_isolated_process",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            return;
        }
        assert!(super::live_view_block_reason().is_none());
        super::block_local_live_view();
        assert_eq!(
            super::live_view_block_reason(),
            Some(super::VA_UPDATE_REQUIRED)
        );

        // Tests use an in-memory recording index: no data directory is touched.
        let test_root = std::env::temp_dir().join(format!("nw-strict-va-{}", uuid::Uuid::new_v4()));
        let mut config = crate::config::AppConfig::default();
        let camera = nw_core::camera::CameraConfig::new("Blocked", "rtsp://example.invalid/live");
        let camera_id = camera.id.clone();
        config.cameras.push(camera);
        let mut state = crate::app_state::AppState::new(
            config,
            test_root.join("config.json"),
            super::HwAccelKind::Software,
        );
        state.set_view_demand(
            "test-ui",
            std::iter::once(camera_id).collect(),
            std::time::Duration::from_secs(10),
        );
        assert_eq!(
            state.active_views(),
            0,
            "strict policy must create no decoder"
        );

        let status = crate::ipc::handler::acceleration_json(super::HwAccelKind::Software);
        assert_eq!(status["kind"], "blocked");
        assert_eq!(status["available"], false);
        drop(state);
        assert!(!test_root.exists(), "nothing is written for a blocked view");
    }

    /// Run alone on each release GPU/driver/runtime combination. Uses generated
    /// video and no backend, sockets, camera credentials, service or recordings.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "decoder lifecycle qualification; run alone with --ignored --nocapture"]
    fn decoder_recreation_keeps_descriptors_bounded() {
        use gstreamer::prelude::*;
        use std::collections::BTreeSet;

        gstreamer::init().unwrap();
        let selected = super::probe();
        let cycles: usize = std::env::var("NW_DECODER_SOAK_CYCLES")
            .map(|value| value.parse().expect("positive cycle count"))
            .unwrap_or(1000);
        assert!(cycles > 0);
        let count_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let mut decoders = BTreeSet::new();
        let mut cycle = || {
            let pipeline = gstreamer::parse::launch(
                "videotestsrc num-buffers=4 ! video/x-raw,format=I420,width=64,height=64 \
                 ! x264enc tune=zerolatency ! h264parse ! decodebin \
                 ! videoconvert ! video/x-raw,format=RGB ! fakesink sync=false",
            )
            .unwrap()
            .downcast::<gstreamer::Pipeline>()
            .unwrap();
            pipeline.set_state(gstreamer::State::Playing).unwrap();
            let message = pipeline.bus().unwrap().timed_pop_filtered(
                gstreamer::ClockTime::from_seconds(10),
                &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
            );
            for element in pipeline.iterate_recurse().into_iter().flatten() {
                if let Some(factory) = element.factory() {
                    if factory.has_type(gstreamer::ElementFactoryType::DECODER) {
                        decoders.insert(factory.name().to_string());
                    }
                }
            }
            pipeline.set_state(gstreamer::State::Null).unwrap();
            assert!(
                matches!(
                    message.as_ref().map(|m| m.view()),
                    Some(gstreamer::MessageView::Eos(_))
                ),
                "decoder did not reach EOS: {message:?}"
            );
        };
        // Prime lazy plugin initialization, global thread pools and caches.
        for _ in 0..5 {
            cycle();
        }
        let baseline = count_fds();
        for index in 1..=cycles {
            cycle();
            let current = count_fds();
            assert!(
                current <= baseline + 8,
                "descriptor growth after {index} cycles: {baseline} -> {current}"
            );
        }
        assert!(!decoders.is_empty());
        eprintln!(
            "selected={selected}; actual_decoders={decoders:?}; cycles={cycles}; fds={baseline}->{}",
            count_fds()
        );
    }

    #[test]
    fn decoder_mapping_includes_v4l2() {
        assert_eq!(h264_decoder(HwAccelKind::VaApi), "vah264dec");
        assert_eq!(h264_decoder(HwAccelKind::NvDec), "nvh264dec");
        assert_eq!(h264_decoder(HwAccelKind::V4l2M2m), "v4l2h264dec");
        assert_eq!(h264_decoder(HwAccelKind::Software), "avdec_h264");
    }

    #[test]
    fn acceleration_kinds_have_stable_wire_names() {
        assert_eq!(HwAccelKind::VaApi.wire_kind(), "vaapi");
        assert_eq!(HwAccelKind::NvDec.wire_kind(), "nvdec");
        assert_eq!(HwAccelKind::V4l2M2m.wire_kind(), "v4l2_m2m");
        assert_eq!(HwAccelKind::Software.wire_kind(), "software");
        assert!(HwAccelKind::VaApi.is_hardware());
        assert!(!HwAccelKind::Software.is_hardware());
    }
}
