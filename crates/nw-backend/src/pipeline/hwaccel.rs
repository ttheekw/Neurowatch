//! Hardware acceleration capability probe.
//!
//! Called once at startup, before pipelines are created. Excludes known-bad
//! plugins, applies the user's decoder choice (`video.decoder`), then selects
//! an available decoder family. Factory presence alone does not prove a
//! driver can decode a particular stream.
//!
//! The choice is enforced in GStreamer's process registry, not only in the
//! explicit [`h264_decoder`]: live views, motion, playback and snapshots use
//! `decodebin`, which picks decoders by rank. A chosen hardware family is
//! ranked above every other decoder and the other hardware families' decoders
//! are removed; "software" removes them all. Registry changes are process-wide
//! and cannot be undone, so a changed choice restarts the backend.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::VideoDecoder;

const VA_UPDATE_REQUIRED: &str =
    "local live view requires a GStreamer VA plugin with the 1.24.6 DMA-BUF fix or later";
static LIVE_VIEW_BLOCK_REASON: OnceLock<&'static str> = OnceLock::new();

/// Why hardware decoding is not in use although a graphics card is there.
pub const GPU_NOT_ACCESSIBLE: &str = "The graphics card is not available to NeuroWatch yet: it \
     started before you signed in. It switches to hardware decoding once you sign in. Installing \
     the .deb, .rpm or Arch package gives your account the graphics card from boot (the render \
     and video groups, after a restart); with the portable download: sudo usermod -aG \
     render,video $USER.";
/// The decoder devices that could not be opened when the backend started.
static GPU_PENDING: OnceLock<Vec<PathBuf>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HwAccelKind {
    VaApi,
    NvDec,
    /// Direct3D 11 (Windows): Intel, AMD and NVIDIA GPUs. VA-API is Linux only.
    D3d11,
    /// Direct3D 12 (Windows, GStreamer 1.24's `d3d12` plugin).
    D3d12,
    /// Intel Quick Sync Video (`qsv`, or the older `msdk` plugin).
    Qsv,
    V4l2M2m,
    Software,
}

impl std::fmt::Display for HwAccelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VaApi => write!(f, "VA-API"),
            Self::NvDec => write!(f, "NVDEC"),
            Self::D3d11 => write!(f, "Direct3D 11"),
            Self::D3d12 => write!(f, "Direct3D 12"),
            Self::Qsv => write!(f, "Intel Quick Sync"),
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
            Self::D3d11 => "d3d11",
            Self::D3d12 => "d3d12",
            Self::Qsv => "qsv",
            Self::V4l2M2m => "v4l2_m2m",
            Self::Software => "software",
        }
    }

    pub const fn is_hardware(self) -> bool {
        !matches!(self, Self::Software)
    }
}

/// A hardware decoder family: the GStreamer plugins whose decoders use one
/// driver interface. Each is one choice of `video.decoder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderFamily {
    NvDec,
    D3d11,
    D3d12,
    Qsv,
    VaApi,
    V4l2,
}

impl DecoderFamily {
    /// In the order Settings lists them.
    pub const ALL: [Self; 6] = [
        Self::NvDec,
        Self::D3d11,
        Self::D3d12,
        Self::Qsv,
        Self::VaApi,
        Self::V4l2,
    ];

    /// The `video.decoder` value choosing this family.
    pub const fn preference(self) -> VideoDecoder {
        match self {
            Self::NvDec => VideoDecoder::Nvdec,
            Self::D3d11 => VideoDecoder::D3d11,
            Self::D3d12 => VideoDecoder::D3d12,
            Self::Qsv => VideoDecoder::Qsv,
            Self::VaApi => VideoDecoder::Vaapi,
            Self::V4l2 => VideoDecoder::V4l2,
        }
    }

    pub fn from_preference(preference: VideoDecoder) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|family| family.preference() == preference)
    }

    /// The name Settings shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::NvDec => "NVIDIA NVDEC",
            Self::D3d11 => "Direct3D 11",
            Self::D3d12 => "Direct3D 12",
            Self::Qsv => "Intel Quick Sync",
            Self::VaApi => "VA-API",
            Self::V4l2 => "V4L2",
        }
    }

    /// The plugins whose decoders belong to the family, preferred first.
    /// Modern VA/QSV implementations must outrank their legacy alternatives:
    /// equal ranks can select a legacy decoder with incompatible GPU memory.
    /// Only their decoders
    /// are ever ranked or removed: `video4linux2` also holds `v4l2src`
    /// (USB cameras), and several hold encoders.
    pub const fn plugins(self) -> &'static [&'static str] {
        match self {
            Self::NvDec => &["nvcodec"],
            Self::D3d11 => &["d3d11"],
            Self::D3d12 => &["d3d12"],
            Self::Qsv => &["qsv", "msdk"],
            // `va` (gst-plugins-bad) and the legacy `vaapi` plugin.
            Self::VaApi => &["va", "vaapi"],
            // Stateful (`v4l2h264dec`) and stateless (`v4l2slh264dec`).
            Self::V4l2 => &["video4linux2", "v4l2codecs"],
        }
    }

    pub const fn kind(self) -> HwAccelKind {
        match self {
            Self::NvDec => HwAccelKind::NvDec,
            Self::D3d11 => HwAccelKind::D3d11,
            Self::D3d12 => HwAccelKind::D3d12,
            Self::Qsv => HwAccelKind::Qsv,
            Self::VaApi => HwAccelKind::VaApi,
            Self::V4l2 => HwAccelKind::V4l2M2m,
        }
    }

    fn of_plugin(plugin: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|family| family.plugins().contains(&plugin))
    }
}

/// Rank given to the chosen family's decoders: above the software decoders
/// (`avdec_*`, primary) and above every other hardware decoder.
const CHOSEN_RANK_BOOST: i32 = 2;

/// What `probe` found and applied, for `system.status` and the settings.
#[derive(Debug)]
struct DecoderChoice {
    preference: VideoDecoder,
    available: Vec<DecoderFamily>,
    /// The chosen hardware family is not on this computer: decoding fell
    /// back to the automatic choice.
    unavailable: bool,
}

static DECODER_CHOICE: OnceLock<DecoderChoice> = OnceLock::new();

/// The `video.decoder` this process applied at start; None before `probe`
/// (and in unit tests, which never probe).
pub fn applied_preference() -> Option<VideoDecoder> {
    DECODER_CHOICE.get().map(|choice| choice.preference)
}

/// The hardware decoder families this computer offered when the backend
/// started (before the choice removed any), for Settings to list.
pub fn available_decoders() -> &'static [DecoderFamily] {
    DECODER_CHOICE
        .get()
        .map_or(&[], |choice| choice.available.as_slice())
}

/// Why the chosen decoder is not in use: it is not on this computer.
pub fn preference_warning() -> Option<String> {
    let choice = DECODER_CHOICE.get().filter(|choice| choice.unavailable)?;
    let name = DecoderFamily::from_preference(choice.preference)
        .map_or(choice.preference.id(), DecoderFamily::label);
    Some(format!(
        "{name} is not available on this computer, so NeuroWatch chose a decoder automatically."
    ))
}

/// Whether a `video.decoder` change from `before` to `after` needs a backend
/// restart: the value changed, and differs from what this process applied.
/// Never before a probe (unit tests).
pub fn needs_restart(before: VideoDecoder, after: VideoDecoder) -> bool {
    restart_needed(before, after, applied_preference())
}

fn restart_needed(
    before: VideoDecoder,
    after: VideoDecoder,
    applied: Option<VideoDecoder>,
) -> bool {
    before != after && applied.is_some_and(|applied| applied != after)
}

/// What a decoder choice does to the registry.
#[derive(Debug, PartialEq)]
struct Plan {
    /// Ranked above every other decoder.
    chosen: Option<DecoderFamily>,
    /// Their decoders are removed.
    remove: Vec<DecoderFamily>,
    /// The chosen family is not available: the automatic choice applies.
    fallback: bool,
}

fn plan(preference: VideoDecoder, available: &[DecoderFamily]) -> Plan {
    let automatic = |fallback| Plan {
        chosen: None,
        remove: Vec::new(),
        fallback,
    };
    match preference {
        VideoDecoder::Auto => automatic(false),
        VideoDecoder::Software => Plan {
            chosen: None,
            remove: DecoderFamily::ALL.to_vec(),
            fallback: false,
        },
        other => match DecoderFamily::from_preference(other) {
            Some(family) if available.contains(&family) => Plan {
                chosen: Some(family),
                remove: DecoderFamily::ALL
                    .into_iter()
                    .filter(|other| *other != family)
                    .collect(),
                fallback: false,
            },
            _ => automatic(true),
        },
    }
}

/// Probe available hardware decoders and apply `preference`. Returns the
/// decoder family in use.
pub fn probe(preference: VideoDecoder) -> HwAccelKind {
    exclude_leaking_va_plugin();
    let available = available_families();
    let plan = plan(preference, &available);
    tracing::info!(
        preference = preference.id(),
        available = ?available,
        "Video decoder choice"
    );
    if plan.fallback {
        tracing::warn!(
            preference = preference.id(),
            "The chosen video decoder is not available on this computer; choosing one automatically"
        );
    }
    let _ = DECODER_CHOICE.set(DecoderChoice {
        preference,
        available,
        unavailable: plan.fallback,
    });
    let removed: usize = plan
        .remove
        .iter()
        .flat_map(|family| family.plugins())
        .map(|plugin| remove_decoders(plugin))
        .sum();
    if let Some(family) = plan.chosen {
        let ranked = rank_decoders(family.plugins(), CHOSEN_RANK_BOOST);
        tracing::info!(
            removed,
            ranked,
            "Hardware acceleration: {} (chosen)",
            family.kind()
        );
        return family.kind();
    }
    if preference == VideoDecoder::Software {
        tracing::info!(
            removed,
            "Hardware acceleration: off, software decoding (chosen)"
        );
        return HwAccelKind::Software;
    }
    probe_automatic()
}

/// The decoder families with a decoder for H.264 or H.265 in the registry.
pub fn available_families() -> Vec<DecoderFamily> {
    use gstreamer::prelude::*;
    let found: Vec<DecoderFamily> = gstreamer::ElementFactory::factories_with_type(
        gstreamer::ElementFactoryType::DECODER,
        gstreamer::Rank::NONE,
    )
    .into_iter()
    .filter(decodes_h264_or_h265)
    .filter_map(|factory| DecoderFamily::of_plugin(factory.plugin_name()?.as_str()))
    .collect();
    DecoderFamily::ALL
        .into_iter()
        .filter(|family| found.contains(family))
        .collect()
}

fn decodes_h264_or_h265(factory: &gstreamer::ElementFactory) -> bool {
    ["video/x-h264", "video/x-h265"]
        .into_iter()
        .any(|media| factory.can_sink_any_caps(&gstreamer::Caps::new_empty_simple(media)))
}

/// The decoders of `plugin` in the registry, loading the plugin first:
/// loading it later would register its factories again (and with their own
/// ranks), undoing a removal or rank change.
fn plugin_decoders(
    registry: &gstreamer::Registry,
    plugin: &str,
) -> Vec<(gstreamer::PluginFeature, gstreamer::ElementFactory)> {
    use gstreamer::prelude::*;
    let Some(found) = registry.find_plugin(plugin) else {
        return Vec::new();
    };
    if let Err(error) = found.load() {
        tracing::warn!(plugin, %error, "Decoder plugin could not be loaded");
    }
    registry
        .features_by_plugin(plugin)
        .into_iter()
        .filter_map(|feature| {
            let factory = feature
                .clone()
                .downcast::<gstreamer::ElementFactory>()
                .ok()?;
            factory
                .has_type(gstreamer::ElementFactoryType::DECODER)
                .then_some((feature, factory))
        })
        .collect()
}

/// Remove `plugin`'s decoders from this process's registry, so neither
/// `decodebin` nor an explicit `ElementFactory::make` can use them. Its
/// other elements (sources, encoders) stay. Irreversible for the process.
fn remove_decoders(plugin: &str) -> usize {
    let registry = gstreamer::Registry::get();
    let decoders = plugin_decoders(&registry, plugin);
    for (feature, _) in &decoders {
        registry.remove_feature(feature);
    }
    decoders.len()
}

/// Rank a chosen family's plugins in preference order, never lowering ranks.
/// Load them all before computing priorities: legacy plugins can initially
/// outrank modern ones, and a tie can make decodebin choose the legacy path.
fn rank_decoders(plugins: &[&str], boost: i32) -> usize {
    rank_decoders_where(plugins, boost, |_| true)
}

/// `rank_decoders` for the decoders `keep` accepts; the others keep their rank.
fn rank_decoders_where(
    plugins: &[&str],
    boost: i32,
    keep: impl Fn(&gstreamer::ElementFactory) -> bool,
) -> usize {
    use gstreamer::prelude::PluginFeatureExtManual;
    let registry = gstreamer::Registry::get();
    let groups: Vec<_> = plugins
        .iter()
        .map(|plugin| {
            let mut decoders = plugin_decoders(&registry, plugin);
            decoders.retain(|(_, factory)| keep(factory));
            (*plugin, decoders)
        })
        .filter(|(_, decoders)| !decoders.is_empty())
        .collect();
    let mut rank = groups
        .iter()
        .flat_map(|(_, decoders)| decoders.iter().map(|(_, factory)| factory.rank()))
        .max()
        .unwrap_or(gstreamer::Rank::NONE)
        .max(gstreamer::Rank::PRIMARY + boost);
    let mut ranked = 0;
    for (plugin, decoders) in groups.into_iter().rev() {
        tracing::debug!(
            plugin,
            rank = i32::from(rank),
            "Video decoder plugin priority"
        );
        for (_, factory) in decoders {
            factory.set_rank(rank);
            ranked += 1;
        }
        rank = gstreamer::Rank::from(i32::from(rank).saturating_add(1));
    }
    ranked
}

/// The automatic choice: the first family found, VA-API first (with Intel or
/// AMD and NVIDIA graphics it decodes on the former; docs/docker.md).
fn probe_automatic() -> HwAccelKind {
    // The family found decodes: VA-API and NVDEC H.264/H.265 decoders both
    // register at primary + 1, and decodebin breaks a tie by name, so with
    // both it decoded on NVIDIA while reporting (and explicitly building)
    // VA-API. Only those codecs are ranked: MJPEG keeps its usual decoder.
    for (family, h264) in [
        (DecoderFamily::VaApi, "vah264dec"),
        (DecoderFamily::NvDec, "nvh264dec"),
    ] {
        if factory_usable(h264) {
            let ranked =
                rank_decoders_where(family.plugins(), CHOSEN_RANK_BOOST, decodes_h264_or_h265);
            tracing::info!(ranked, "Hardware acceleration: {}", family.kind());
            return family.kind();
        }
    }
    // Registered only for a GPU whose driver can decode H.264.
    if factory_usable("d3d11h264dec") {
        prefer_over_software(&["d3d11h264dec", "d3d11h265dec"]);
        tracing::info!("Hardware acceleration: Direct3D 11");
        return HwAccelKind::D3d11;
    }
    if factory_usable("v4l2h264dec") {
        tracing::info!("Hardware acceleration: V4L2 M2M");
        return HwAccelKind::V4l2M2m;
    }
    // A decoder this user may not open yet: a GPU's render node (VA-API) or
    // an SoC's V4L2 decoder (Raspberry Pi, Rockchip: ARM boards). Without
    // the `render` / `video` group, logind grants them only to a signed-in
    // session, and a service started at boot (lingering) or early in the
    // sign-in runs before that. GStreamer then registers no hardware
    // decoders, for the life of this process.
    let blocked = blocked_nodes(&accelerator_nodes());
    if !blocked.is_empty() {
        tracing::warn!(nodes = ?blocked, "Decoder devices cannot be opened yet");
        let _ = GPU_PENDING.set(blocked);
        tracing::warn!(
            "Hardware acceleration: Software (avdec_h264) for now: the graphics card or video \
             decoder cannot be opened yet (started before sign-in); the backend restarts itself \
             once it can"
        );
        return HwAccelKind::Software;
    }
    tracing::info!("Hardware acceleration: Software (avdec_h264)");
    HwAccelKind::Software
}

const DRI_DIR: &str = "/dev/dri";
const V4L2_SYSFS: &str = "/sys/class/video4linux";
const DEV_DIR: &str = "/dev";
/// How often a backend waiting for the graphics card checks for it.
pub const GPU_ACCESS_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// The DRM render nodes (`renderD*`) in `dir`; none off Linux.
fn render_nodes(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        .map(|entry| entry.path())
        .collect();
    nodes.sort();
    nodes
}

/// V4L2 video decoders (`/dev/videoN`), told from cameras by their driver's
/// name in sysfs: Raspberry Pi's `bcm2835-codec-decode`, Rockchip's
/// `rkvdec`, Hantro and Allwinner VPUs. None off Linux.
fn v4l2_decoder_nodes(sysfs: &Path, dev: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(sysfs) else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("video"))
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("name"))
                .is_ok_and(|name| is_decoder_name(&name))
        })
        .map(|entry| dev.join(entry.file_name()))
        .collect();
    nodes.sort();
    nodes
}

fn is_decoder_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["codec", "dec", "vpu", "hantro", "cedrus"]
        .iter()
        .any(|word| name.contains(word))
}

/// Every hardware decoder device: GPU render nodes and V4L2 decoders.
fn accelerator_nodes() -> Vec<PathBuf> {
    let mut nodes = render_nodes(Path::new(DRI_DIR));
    nodes.extend(v4l2_decoder_nodes(
        Path::new(V4L2_SYSFS),
        Path::new(DEV_DIR),
    ));
    nodes
}

fn openable(node: &Path) -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .is_ok()
}

/// The decoder devices this process cannot open.
fn blocked_nodes(nodes: &[PathBuf]) -> Vec<PathBuf> {
    nodes
        .iter()
        .filter(|node| !openable(node))
        .cloned()
        .collect()
}

/// Hardware decoding fell back to software only because the graphics card
/// could not be opened yet ([`GPU_NOT_ACCESSIBLE`]).
pub fn waiting_for_gpu_access() -> bool {
    GPU_PENDING.get().is_some()
}

/// Resolves once one of the devices that could not be opened at start can
/// be: the moment a restart would find its decoders. Never resolves if none
/// ever can (or none was waited for), so a device that stays closed never
/// restarts the backend, and one that opens restarts it once.
pub async fn gpu_accessible() {
    let Some(nodes) = GPU_PENDING.get() else {
        return std::future::pending().await;
    };
    let mut tick = tokio::time::interval(GPU_ACCESS_POLL);
    loop {
        tick.tick().await;
        if nodes.iter().any(|node| openable(node)) {
            return;
        }
    }
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

/// Remove every hardware decoder plugin from this process's registry, so
/// `decodebin` decodes in software (measurements that must not depend on the
/// machine's GPU). Irreversible for the process: only for a test run alone.
#[cfg(test)]
pub(crate) fn remove_hardware_decoders() -> usize {
    let registry = gstreamer::Registry::get();
    [
        "va",
        "vaapi",
        "nvcodec",
        "v4l2codecs",
        "qsv",
        "d3d11",
        "d3d12",
    ]
    .into_iter()
    .map(|plugin| exclude_plugin_features(&registry, plugin))
    .sum()
}

/// Rank `factories` above the software decoders (`avdec_*`, primary), so the
/// `decodebin` of live view and playback picks them too, not only the explicit
/// `h264_decoder` choice.
fn prefer_over_software(factories: &[&str]) {
    use gstreamer::prelude::PluginFeatureExtManual;
    let rank = gstreamer::Rank::PRIMARY + 1;
    for name in factories {
        if let Some(factory) = gstreamer::ElementFactory::find(name) {
            if factory.rank() < rank {
                factory.set_rank(rank);
            }
        }
    }
}

fn factory_usable(element_name: &str) -> bool {
    gstreamer::ElementFactory::find(element_name).is_some()
}

/// Resolve an H.264 decoder within the chosen family. A family can be available
/// for H.265 only, or supplied by an older plugin; never name a missing factory
/// or borrow a different hardware family to decode an unsupported codec.
pub fn h264_decoder(kind: HwAccelKind) -> &'static str {
    resolve_h264_decoder(kind, factory_usable)
}

fn resolve_h264_decoder(
    kind: HwAccelKind,
    mut available: impl FnMut(&str) -> bool,
) -> &'static str {
    let candidates: &[&str] = match kind {
        HwAccelKind::VaApi => &["vah264dec", "vaapih264dec"],
        HwAccelKind::NvDec => &["nvh264dec", "nvh264sldec"],
        HwAccelKind::D3d11 => &["d3d11h264dec"],
        HwAccelKind::D3d12 => &["d3d12h264dec"],
        HwAccelKind::Qsv => &["qsvh264dec", "msdkh264dec"],
        HwAccelKind::V4l2M2m => &["v4l2h264dec", "v4l2slh264dec"],
        HwAccelKind::Software => &[],
    };
    candidates
        .iter()
        .copied()
        .find(|candidate| available(candidate))
        .unwrap_or("avdec_h264")
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
    use super::{
        blocked_nodes, is_decoder_name, needs_restart, openable, plan, prefer_over_software,
        render_nodes, resolve_h264_decoder, restart_needed, v4l2_decoder_nodes, DecoderFamily,
        HwAccelKind,
    };

    #[cfg(unix)]
    #[test]
    fn a_decoder_that_cannot_be_opened_yet_is_told_apart_from_no_decoder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nw-dri-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (dri, sysfs, dev) = (dir.join("dri"), dir.join("sysfs"), dir.join("dev"));
        for folder in [&dri, &sysfs, &dev] {
            std::fs::create_dir_all(folder).unwrap();
        }
        assert!(render_nodes(&dri).is_empty() && v4l2_decoder_nodes(&sysfs, &dev).is_empty());
        assert!(
            blocked_nodes(&[]).is_empty(),
            "no decoder: nothing to wait for"
        );

        std::fs::write(dri.join("card1"), b"").unwrap();
        let render = dri.join("renderD128");
        std::fs::write(&render, b"").unwrap();
        assert_eq!(
            render_nodes(&dri),
            vec![render.clone()],
            "only render nodes"
        );

        // An ARM board: a V4L2 decoder beside a camera.
        for (node, name) in [
            ("video0", "Integrated_Webcam_HD: Integrate"),
            ("video10", "bcm2835-codec-decode"),
        ] {
            std::fs::create_dir_all(sysfs.join(node)).unwrap();
            std::fs::write(sysfs.join(node).join("name"), format!("{name}\n")).unwrap();
            std::fs::write(dev.join(node), b"").unwrap();
        }
        let decoder = dev.join("video10");
        assert_eq!(
            v4l2_decoder_nodes(&sysfs, &dev),
            vec![decoder.clone()],
            "cameras are not decoders"
        );
        assert!(
            is_decoder_name("rkvdec")
                && is_decoder_name("hantro-vpu")
                && !is_decoder_name("USB2.0 Camera")
        );

        let nodes = [render.clone(), decoder.clone()];
        assert!(blocked_nodes(&nodes).is_empty(), "openable: not waiting");
        std::fs::set_permissions(&decoder, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root = openable(&decoder); // root opens anything; the check is moot then
        assert_eq!(
            blocked_nodes(&nodes),
            if root { vec![] } else { vec![decoder.clone()] },
            "only the decoder that cannot be opened is waited for"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        let selected = super::probe(crate::config::VideoDecoder::Auto);
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
        let h264_decoder = |kind| resolve_h264_decoder(kind, |_| true);
        assert_eq!(h264_decoder(HwAccelKind::VaApi), "vah264dec");
        assert_eq!(h264_decoder(HwAccelKind::NvDec), "nvh264dec");
        assert_eq!(h264_decoder(HwAccelKind::D3d11), "d3d11h264dec");
        assert_eq!(h264_decoder(HwAccelKind::V4l2M2m), "v4l2h264dec");
        assert_eq!(h264_decoder(HwAccelKind::Software), "avdec_h264");
    }

    #[test]
    fn preferred_decoders_outrank_software_and_missing_ones_are_skipped() {
        use gstreamer::prelude::PluginFeatureExtManual;
        gstreamer::init().unwrap();
        // `identity` (rank none) stands in for a hardware decoder.
        prefer_over_software(&["identity", "no-such-decoder"]);
        let identity = gstreamer::ElementFactory::find("identity").unwrap();
        assert_eq!(identity.rank(), gstreamer::Rank::PRIMARY + 1);
    }

    #[test]
    fn decoder_mapping_includes_windows_families() {
        let h264_decoder = |kind| resolve_h264_decoder(kind, |_| true);
        assert_eq!(h264_decoder(HwAccelKind::D3d12), "d3d12h264dec");
        assert_eq!(h264_decoder(HwAccelKind::Qsv), "qsvh264dec");
        assert_eq!(HwAccelKind::D3d12.wire_kind(), "d3d12");
        assert_eq!(HwAccelKind::Qsv.wire_kind(), "qsv");
        assert!(HwAccelKind::D3d12.is_hardware() && HwAccelKind::Qsv.is_hardware());
    }

    #[test]
    fn h264_resolution_uses_available_family_variants_and_cpu_for_missing_codecs() {
        for (kind, legacy) in [
            (HwAccelKind::VaApi, "vaapih264dec"),
            (HwAccelKind::Qsv, "msdkh264dec"),
            (HwAccelKind::V4l2M2m, "v4l2slh264dec"),
            (HwAccelKind::NvDec, "nvh264sldec"),
        ] {
            assert_eq!(resolve_h264_decoder(kind, |name| name == legacy), legacy);
        }
        for kind in [
            HwAccelKind::VaApi,
            HwAccelKind::NvDec,
            HwAccelKind::D3d11,
            HwAccelKind::D3d12,
            HwAccelKind::Qsv,
            HwAccelKind::V4l2M2m,
        ] {
            // An H.265-only GPU family cannot provide this H.264 stream.
            assert_eq!(
                resolve_h264_decoder(kind, |name| name.ends_with("h265dec")),
                "avdec_h264"
            );
        }
        assert_eq!(
            resolve_h264_decoder(HwAccelKind::Qsv, |name| name == "nvh264dec"),
            "avdec_h264",
            "never fall through to another hardware family"
        );
        assert_eq!(
            resolve_h264_decoder(HwAccelKind::Software, |_| panic!("no hardware lookup")),
            "avdec_h264"
        );
    }

    #[test]
    fn h264_resolution_names_a_registered_decoder_on_this_computer() {
        gstreamer::init().unwrap();
        for kind in DecoderFamily::ALL
            .into_iter()
            .map(DecoderFamily::kind)
            .chain([HwAccelKind::Software])
        {
            let decoder = super::h264_decoder(kind);
            let factory = gstreamer::ElementFactory::find(decoder).unwrap_or_else(|| {
                panic!("{kind}: explicit pipeline would name missing {decoder}")
            });
            assert!(factory.has_type(gstreamer::ElementFactoryType::DECODER));
            assert!(factory.can_sink_any_caps(&gstreamer::Caps::new_empty_simple("video/x-h264")));
        }
    }

    #[test]
    fn every_hardware_choice_is_one_family_and_back() {
        use crate::config::VideoDecoder;
        for decoder in VideoDecoder::ALL {
            let family = DecoderFamily::from_preference(decoder);
            assert_eq!(
                family.is_none(),
                matches!(decoder, VideoDecoder::Auto | VideoDecoder::Software),
                "{decoder:?}"
            );
            if let Some(family) = family {
                assert_eq!(family.preference(), decoder);
                assert!(family.kind().is_hardware());
                for plugin in family.plugins() {
                    assert_eq!(DecoderFamily::of_plugin(plugin), Some(family));
                }
            }
        }
        assert_eq!(
            DecoderFamily::of_plugin("libav"),
            None,
            "software is no family"
        );
        assert_eq!(DecoderFamily::V4l2.kind(), HwAccelKind::V4l2M2m);
    }

    #[test]
    fn a_choice_ranks_its_family_removes_the_others_or_falls_back() {
        use crate::config::VideoDecoder;
        use DecoderFamily::{D3d11, NvDec, VaApi};
        let available = [NvDec, D3d11];

        let auto = plan(VideoDecoder::Auto, &available);
        assert_eq!(
            (auto.chosen, auto.remove.len(), auto.fallback),
            (None, 0, false)
        );

        let nvdec = plan(VideoDecoder::Nvdec, &available);
        assert_eq!(nvdec.chosen, Some(NvDec));
        assert!(!nvdec.fallback);
        assert!(!nvdec.remove.contains(&NvDec), "the chosen one stays");
        assert!(
            nvdec.remove.contains(&D3d11) && nvdec.remove.contains(&VaApi),
            "every other hardware family is removed, found or not"
        );
        assert_eq!(nvdec.remove.len(), DecoderFamily::ALL.len() - 1);

        let software = plan(VideoDecoder::Software, &available);
        assert_eq!(software.chosen, None);
        assert_eq!(
            software.remove,
            DecoderFamily::ALL.to_vec(),
            "CPU only: none left"
        );
        assert!(!software.fallback);

        // Not on this computer: decode as if automatic, and say so.
        let missing = plan(VideoDecoder::Vaapi, &available);
        assert_eq!(
            (missing.chosen, missing.remove.len(), missing.fallback),
            (None, 0, true)
        );
    }

    #[test]
    fn only_a_real_change_of_the_applied_choice_restarts() {
        use crate::config::VideoDecoder::{Auto, Nvdec, Software};
        assert!(restart_needed(Auto, Nvdec, Some(Auto)));
        assert!(!restart_needed(Auto, Auto, Some(Auto)), "unchanged");
        assert!(
            !restart_needed(Software, Auto, Some(Auto)),
            "back to what runs: nothing to apply"
        );
        assert!(
            !restart_needed(Auto, Nvdec, None),
            "never probed (unit tests)"
        );
        assert!(!needs_restart(Auto, Software), "tests never probe");
    }

    /// Run `test` in a child test process (registry changes and the probe's
    /// once-only state must not reach other tests); true inside the child.
    fn isolated(test: &str, marker: &str) -> bool {
        if std::env::var_os(marker).is_some() {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("pipeline::hwaccel::tests::{test}"),
                "--nocapture",
            ])
            .env(marker, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        false
    }

    #[test]
    fn chosen_decoders_outrank_all_and_removed_ones_leave_other_elements() {
        use gstreamer::prelude::*;
        if !isolated(
            "chosen_decoders_outrank_all_and_removed_ones_leave_other_elements",
            "NW_TEST_DECODER_RANKING_CHILD",
        ) {
            return;
        }
        gstreamer::init().unwrap();
        // Stand-ins needing no GPU: libav's decoders as the chosen family,
        // the jpeg plugin (a decoder and an encoder) as one removed.
        let ranked = super::rank_decoders(&["libav", "no-such-plugin"], super::CHOSEN_RANK_BOOST);
        assert!(ranked > 0);
        let avdec = gstreamer::ElementFactory::find("avdec_h264").unwrap();
        assert_eq!(avdec.rank(), gstreamer::Rank::PRIMARY + 2);
        let best_h264 = gstreamer::ElementFactory::factories_with_type(
            gstreamer::ElementFactoryType::DECODER,
            gstreamer::Rank::MARGINAL,
        )
        .into_iter()
        .filter(|factory| {
            factory.can_sink_any_caps(&gstreamer::Caps::new_empty_simple("video/x-h264"))
        })
        .max_by_key(|factory| factory.rank())
        .unwrap();
        assert_eq!(best_h264.rank(), gstreamer::Rank::PRIMARY + 2);

        assert!(gstreamer::ElementFactory::find("jpegdec").is_some());
        assert!(super::remove_decoders("jpeg") > 0);
        assert!(gstreamer::ElementFactory::find("jpegdec").is_none());
        assert!(
            gstreamer::ElementFactory::find("jpegenc").is_some(),
            "only decoders are removed"
        );
        assert!(gstreamer::ElementFactory::make("jpegdec").build().is_err());
        assert_eq!(super::remove_decoders("jpeg"), 0, "removed for good");
    }

    #[test]
    fn modern_decoder_plugins_outrank_legacy_alternatives() {
        use gstreamer::prelude::*;
        if !isolated(
            "modern_decoder_plugins_outrank_legacy_alternatives",
            "NW_TEST_DECODER_PLUGIN_ORDER_CHILD",
        ) {
            return;
        }
        gstreamer::init().unwrap();
        // Two CPU JPEG decoders stand in for modern/legacy implementations
        // of one hardware family, so the regression needs no GPU or driver.
        let modern = gstreamer::ElementFactory::find("avdec_mjpeg").unwrap();
        let legacy = gstreamer::ElementFactory::find("jpegdec").unwrap();
        modern.set_rank(gstreamer::Rank::NONE);
        legacy.set_rank(gstreamer::Rank::PRIMARY + 100);
        super::rank_decoders(&["libav", "jpeg"], super::CHOSEN_RANK_BOOST);
        assert!(
            modern.rank() > legacy.rank(),
            "the preferred plugin must beat the legacy decoder, even when its original rank was higher"
        );
        assert!(legacy.rank() >= gstreamer::Rank::PRIMARY + 100);
        let best_jpeg = gstreamer::ElementFactory::factories_with_type(
            gstreamer::ElementFactoryType::DECODER,
            gstreamer::Rank::MARGINAL,
        )
        .into_iter()
        .filter(|factory| {
            factory.can_sink_any_caps(&gstreamer::Caps::new_empty_simple("image/jpeg"))
        })
        .max_by_key(|factory| factory.rank())
        .unwrap();
        assert_eq!(best_jpeg.plugin_name().as_deref(), Some("libav"));
    }

    #[test]
    fn the_automatic_family_wins_a_rank_tie_for_its_codecs_only() {
        use gstreamer::prelude::*;
        if !isolated(
            "the_automatic_family_wins_a_rank_tie_for_its_codecs_only",
            "NW_TEST_DECODER_AUTOMATIC_TIE_CHILD",
        ) {
            return;
        }
        gstreamer::init().unwrap();
        // Two CPU JPEG decoders stand in for VA-API and NVDEC, tied as their
        // H.264 decoders are (primary + 1); decodebin would take the name
        // that sorts first (`avdec_mjpeg` here, `nvh264dec` there).
        let chosen = gstreamer::ElementFactory::find("jpegdec").unwrap();
        let other = gstreamer::ElementFactory::find("avdec_mjpeg").unwrap();
        chosen.set_rank(gstreamer::Rank::PRIMARY + 1);
        other.set_rank(gstreamer::Rank::PRIMARY + 1);
        let h264_before = gstreamer::ElementFactory::find("avdec_h264")
            .unwrap()
            .rank();
        let ranked = super::rank_decoders_where(&["jpeg"], super::CHOSEN_RANK_BOOST, |f| {
            f.can_sink_any_caps(&gstreamer::Caps::new_empty_simple("image/jpeg"))
        });
        assert!(ranked > 0);
        assert!(
            chosen.rank() > other.rank(),
            "the tie is broken for the chosen family"
        );
        assert_eq!(
            other.rank(),
            gstreamer::Rank::PRIMARY + 1,
            "the other stays a fallback"
        );
        let ranked = super::rank_decoders_where(&["libav"], super::CHOSEN_RANK_BOOST, |f| {
            f.can_sink_any_caps(&gstreamer::Caps::new_empty_simple("image/jpeg"))
        });
        assert!(ranked > 0);
        assert_eq!(
            gstreamer::ElementFactory::find("avdec_h264")
                .unwrap()
                .rank(),
            h264_before,
            "a decoder the filter rejects keeps its rank"
        );
    }

    #[test]
    fn cpu_only_leaves_no_hardware_decoder() {
        use crate::config::VideoDecoder;
        use gstreamer::prelude::*;
        if !isolated(
            "cpu_only_leaves_no_hardware_decoder",
            "NW_TEST_CPU_ONLY_CHILD",
        ) {
            return;
        }
        gstreamer::init().unwrap();
        let found = super::available_families();
        assert_eq!(super::probe(VideoDecoder::Software), HwAccelKind::Software);
        assert_eq!(super::applied_preference(), Some(VideoDecoder::Software));
        assert_eq!(
            super::available_decoders(),
            found.as_slice(),
            "listed as found"
        );
        assert!(super::available_families().is_empty());
        let hardware = gstreamer::ElementFactory::factories_with_type(
            gstreamer::ElementFactoryType::DECODER,
            gstreamer::Rank::NONE,
        )
        .into_iter()
        .filter_map(|factory| factory.plugin_name())
        .filter(|plugin| DecoderFamily::of_plugin(plugin).is_some())
        .count();
        assert_eq!(hardware, 0, "no hardware decoder is left for decodebin");
        assert!(super::preference_warning().is_none());
        assert!(
            !super::waiting_for_gpu_access(),
            "CPU chosen: no waiting for the graphics card"
        );
        let status = crate::ipc::handler::acceleration_json(HwAccelKind::Software);
        assert_eq!(status["decoder_preference"], "software");
    }

    #[test]
    fn a_choice_not_on_this_computer_falls_back_to_automatic() {
        if !isolated(
            "a_choice_not_on_this_computer_falls_back_to_automatic",
            "NW_TEST_DECODER_FALLBACK_CHILD",
        ) {
            return;
        }
        gstreamer::init().unwrap();
        let found = super::available_families();
        let Some(missing) = DecoderFamily::ALL
            .into_iter()
            .find(|family| !found.contains(family))
        else {
            return; // every family is here: nothing to fall back from
        };
        super::probe(missing.preference());
        assert_eq!(super::applied_preference(), Some(missing.preference()));
        let warning = super::preference_warning().unwrap();
        assert!(warning.contains(missing.label()), "{warning}");
        assert_eq!(super::available_families(), found, "nothing was removed");
        let status = crate::ipc::handler::acceleration_json(HwAccelKind::Software);
        assert_eq!(status["decoder_preference_warning"], warning.as_str());
    }

    #[test]
    fn acceleration_kinds_have_stable_wire_names() {
        assert_eq!(HwAccelKind::VaApi.wire_kind(), "vaapi");
        assert_eq!(HwAccelKind::NvDec.wire_kind(), "nvdec");
        assert_eq!(HwAccelKind::D3d11.wire_kind(), "d3d11");
        assert!(HwAccelKind::D3d11.is_hardware());
        assert_eq!(HwAccelKind::V4l2M2m.wire_kind(), "v4l2_m2m");
        assert_eq!(HwAccelKind::Software.wire_kind(), "software");
        assert!(HwAccelKind::VaApi.is_hardware());
        assert!(!HwAccelKind::Software.is_hardware());
    }
}
