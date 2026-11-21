//! Why a graphics card that can decode video does not: its VA-API driver
//! (or NVIDIA's decode library) is missing.
//!
//! Without a working libva driver GStreamer's `va` plugin registers no
//! decoders, and the backend ends on software decoding exactly as on a PC
//! with no graphics card at all. [`super::hwaccel::probe`] tells the two
//! apart once at startup: each render node's PCI vendor and kernel driver
//! (sysfs), whether the `va` plugin is there (registry), and whether the
//! card's libva driver file is installed (the libva driver folders) decide,
//! in the pure [`classify`], what is missing and which packages bring it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `video_acceleration.reason_code`: a VA-API card whose driver (or
/// GStreamer's `va` plugin) is missing.
pub const VAAPI_DRIVER_MISSING: &str = "VAAPI_DRIVER_MISSING";
/// `video_acceleration.reason_code`: an NVIDIA card on its own driver
/// without NVIDIA's decode library (or GStreamer's `nvcodec` plugin).
pub const NVDEC_LIBRARY_MISSING: &str = "NVDEC_LIBRARY_MISSING";

/// Where each DRM node's sysfs entry is (`<node>/device/vendor`, `driver`).
pub const DRM_SYSFS: &str = "/sys/class/drm";

/// libva's own search folders when `LIBVA_DRIVERS_PATH` is unset (Debian
/// multiarch, Fedora, Arch; Fedora's RPM Fusion Mesa in `dri-freeworld`).
const VA_DRIVER_DIRS: [&str; 6] = [
    "/usr/lib/x86_64-linux-gnu/dri",
    "/usr/lib/aarch64-linux-gnu/dri",
    "/usr/lib64/dri",
    "/usr/lib64/dri-freeworld",
    "/usr/lib/dri",
    "/usr/lib/dri-freeworld",
];

/// The graphics card's maker, from its PCI vendor id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Intel,
    Amd,
    Nvidia,
}

impl Vendor {
    /// `0x8086` (sysfs `device/vendor`) and the like.
    pub fn from_pci_id(id: &str) -> Option<Self> {
        let id = id.trim();
        let hex = id
            .strip_prefix("0x")
            .or_else(|| id.strip_prefix("0X"))
            .unwrap_or(id);
        match u16::from_str_radix(hex, 16).ok()? {
            0x8086 => Some(Self::Intel),
            0x1002 => Some(Self::Amd),
            0x10de => Some(Self::Nvidia),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Intel => "Intel",
            Self::Amd => "AMD",
            Self::Nvidia => "NVIDIA",
        }
    }
}

/// One DRM render node and the card behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderGpu {
    pub node: PathBuf,
    /// None: not Intel, AMD or NVIDIA (a virtual GPU, for one), or unreadable.
    pub vendor: Option<Vendor>,
    /// The kernel driver bound to the card (`i915`, `xe`, `amdgpu`, `radeon`,
    /// `nouveau`, `nvidia`).
    pub kernel_driver: Option<String>,
    pub openable: bool,
}

/// The card behind each of `nodes` (`/dev/dri/renderD*`), read from
/// `drm_sysfs` (`/sys/class/drm`): `<node>/device/vendor` and the name of
/// the `<node>/device/driver` link.
pub fn read_render_gpus(
    drm_sysfs: &Path,
    nodes: &[PathBuf],
    openable: impl Fn(&Path) -> bool,
) -> Vec<RenderGpu> {
    nodes
        .iter()
        .map(|node| {
            let device = node
                .file_name()
                .map(|name| drm_sysfs.join(name).join("device"));
            let vendor = device
                .as_ref()
                .and_then(|device| std::fs::read_to_string(device.join("vendor")).ok())
                .and_then(|id| Vendor::from_pci_id(&id));
            let kernel_driver = device
                .as_ref()
                .and_then(|device| std::fs::read_link(device.join("driver")).ok())
                .and_then(|link| {
                    link.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                });
            RenderGpu {
                node: node.clone(),
                vendor,
                kernel_driver,
                openable: openable(node),
            }
        })
        .collect()
}

/// The folders libva loads drivers from: `LIBVA_DRIVERS_PATH` when set (it
/// replaces the defaults), else the usual system folders.
pub fn va_driver_dirs(drivers_path: Option<OsString>) -> Vec<PathBuf> {
    match drivers_path.filter(|path| !path.is_empty()) {
        Some(path) => std::env::split_paths(&path).collect(),
        None => VA_DRIVER_DIRS.iter().map(PathBuf::from).collect(),
    }
}

/// The libva drivers installed in `dirs` (`iHD` for `iHD_drv_video.so`),
/// sorted, each once.
pub fn installed_va_drivers(dirs: &[PathBuf]) -> Vec<String> {
    let mut found: Vec<String> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix("_drv_video.so").map(str::to_owned)
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// What the probe found, for [`classify`].
#[derive(Debug, Default)]
pub struct Facts<'a> {
    pub gpus: &'a [RenderGpu],
    /// A decoder device this process cannot open yet (GPU access pending:
    /// another reason, `GPU_NOT_ACCESSIBLE`).
    pub nodes_blocked: bool,
    /// The `va` plugin was taken out for a known bug (another reason).
    pub live_view_blocked: bool,
    /// The registry has a VA-API H.264 or H.265 decoder.
    pub vaapi_decodes: bool,
    /// The registry has an NVDEC H.264 or H.265 decoder.
    pub nvdec_decodes: bool,
    /// GStreamer's `va` plugin is installed.
    pub va_plugin: bool,
    /// GStreamer's `nvcodec` plugin is installed.
    pub nvcodec_plugin: bool,
    /// [`installed_va_drivers`].
    pub installed_va_drivers: &'a [String],
}

/// What is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// GStreamer's `va` plugin is not installed (a system GStreamer: release
    /// builds bring their own).
    VaPluginMissing,
    /// The card's libva driver is not installed.
    DriverNotInstalled,
    /// The driver is installed, yet it offered no H.264 or H.265 decoder
    /// (too old for the card, or built without them: Fedora's own Mesa).
    DriverFailed,
    /// GStreamer's `nvcodec` plugin is not installed (NVIDIA's own driver).
    NvcodecPluginMissing,
    /// NVIDIA's decode library (`libnvcuvid`) is not installed.
    NvdecLibraryMissing,
}

impl Cause {
    pub const fn id(self) -> &'static str {
        match self {
            Self::VaPluginMissing => "va_plugin_missing",
            Self::DriverNotInstalled => "driver_not_installed",
            Self::DriverFailed => "driver_failed",
            Self::NvcodecPluginMissing => "nvcodec_plugin_missing",
            Self::NvdecLibraryMissing => "nvdec_library_missing",
        }
    }
}

/// The packages that bring what is missing, per package family (each list
/// installed together).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Packages {
    /// Debian, Ubuntu.
    pub deb: Vec<&'static str>,
    /// Fedora (and RPM Fusion).
    pub rpm: Vec<&'static str>,
    /// Arch Linux.
    pub arch: Vec<&'static str>,
}

impl Packages {
    fn of(deb: &[&'static str], rpm: &[&'static str], arch: &[&'static str]) -> Self {
        Self {
            deb: deb.to_vec(),
            rpm: rpm.to_vec(),
            arch: arch.to_vec(),
        }
    }

    fn and(mut self, other: Self) -> Self {
        self.deb.extend(other.deb);
        self.rpm.extend(other.rpm);
        self.arch.extend(other.arch);
        self
    }

    /// "Install a (Debian/Ubuntu), b (Fedora) or c (Arch)".
    fn install_sentence(&self) -> String {
        let join = |packages: &[&str]| packages.join(" and ");
        format!(
            "Install {} (Debian/Ubuntu), {} (Fedora) or {} (Arch), then restart NeuroWatch.",
            join(&self.deb),
            join(&self.rpm),
            join(&self.arch)
        )
    }
}

fn intel_media() -> Packages {
    Packages::of(
        &["intel-media-va-driver"],
        &["libva-intel-media-driver"],
        &["intel-media-driver"],
    )
}

fn intel_i965() -> Packages {
    Packages::of(
        &["i965-va-driver"],
        &["libva-intel-driver"],
        &["libva-intel-driver"],
    )
}

/// Mesa's VA drivers: Fedora's own `mesa-va-drivers` leaves H.264 and H.265
/// out; RPM Fusion's `-freeworld` build has them.
fn mesa_va() -> Packages {
    Packages::of(
        &["mesa-va-drivers"],
        &["mesa-va-drivers-freeworld"],
        &["mesa"],
    )
}

fn gstreamer_bad() -> Packages {
    Packages::of(
        &["gstreamer1.0-plugins-bad"],
        &["gstreamer1-plugins-bad-free"],
        &["gst-plugins-bad"],
    )
}

fn nvidia_decode() -> Packages {
    Packages::of(
        &["libnvcuvid1"],
        &["xorg-x11-drv-nvidia-cuda-libs"],
        &["nvidia-utils"],
    )
}

/// The graphics card could decode video but nothing decodes on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDriver {
    /// [`VAAPI_DRIVER_MISSING`] or [`NVDEC_LIBRARY_MISSING`].
    pub reason_code: &'static str,
    pub vendor: Vendor,
    /// "" when unknown.
    pub kernel_driver: String,
    pub render_node: PathBuf,
    pub cause: Cause,
    /// The libva driver the card needs (`iHD`, `radeonsi`); "" for NVDEC.
    pub va_driver: &'static str,
    pub packages: Packages,
    /// What the user reads: what is missing, what to install.
    pub reason: String,
}

/// The VA-API card to report, Intel and AMD before nouveau.
fn va_card(gpu: &RenderGpu) -> Option<(u8, &'static [&'static str])> {
    let driver = gpu.kernel_driver.as_deref();
    match gpu.vendor? {
        Vendor::Intel => Some((0, &["iHD", "i965"])),
        Vendor::Amd if driver == Some("radeon") => Some((0, &["r600"])),
        Vendor::Amd => Some((0, &["radeonsi"])),
        Vendor::Nvidia if driver == Some("nouveau") => Some((1, &["nouveau"])),
        Vendor::Nvidia => None,
    }
}

/// Whether a graphics card that can decode video has no working decoder,
/// and why. None while another reason applies (devices not openable yet, the
/// `va` plugin taken out), when a decoder works, or with no such card.
pub fn classify(facts: &Facts<'_>) -> Option<MissingDriver> {
    if facts.nodes_blocked || facts.live_view_blocked {
        return None;
    }
    let usable = || facts.gpus.iter().filter(|gpu| gpu.openable);
    if !facts.vaapi_decodes {
        let card = usable()
            .filter_map(|gpu| va_card(gpu).map(|(rank, drivers)| (rank, drivers, gpu)))
            .min_by_key(|(rank, _, _)| *rank);
        if let Some((_, drivers, gpu)) = card {
            return Some(va_missing(gpu, drivers, facts));
        }
    }
    if !facts.nvdec_decodes {
        let nvidia = usable().find(|gpu| {
            gpu.vendor == Some(Vendor::Nvidia) && gpu.kernel_driver.as_deref() == Some("nvidia")
        });
        if let Some(gpu) = nvidia {
            return Some(nvdec_missing(gpu, facts));
        }
    }
    None
}

fn graphics(gpu: &RenderGpu, vendor: Vendor) -> String {
    if gpu.kernel_driver.as_deref() == Some("nouveau") {
        "NVIDIA graphics (the open-source nouveau driver)".to_owned()
    } else {
        format!("{} graphics", vendor.name())
    }
}

const OLDER_INTEL: &str = "Intel graphics from before 2014 (older than Broadwell) use the i965 \
                           driver instead";
const NEWER_INTEL: &str = "Intel graphics from 2014 on (Broadwell and newer) need the iHD driver";
const VAINFO: &str = "The vainfo command shows what the driver reports.";

fn va_missing(gpu: &RenderGpu, drivers: &[&'static str], facts: &Facts<'_>) -> MissingDriver {
    let vendor = gpu.vendor.unwrap_or(Vendor::Intel);
    let intel = vendor == Vendor::Intel;
    let installed = |driver: &str| facts.installed_va_drivers.iter().any(|d| d == driver);
    let present: Vec<&'static str> = drivers.iter().copied().filter(|d| installed(d)).collect();
    let primary = drivers.first().copied().unwrap_or("");
    let driver_packages = if intel { intel_media() } else { mesa_va() };
    let head = format!(
        "This PC has {} that can decode video, but",
        graphics(gpu, vendor)
    );
    let cpu = "so video is decoded on the CPU.";
    let (cause, va_driver, packages, reason) = if !facts.va_plugin {
        let packages = if present.is_empty() {
            gstreamer_bad().and(driver_packages)
        } else {
            gstreamer_bad()
        };
        let reason = format!(
            "{head} GStreamer's VA plugin (va, part of the \"bad\" plugins) isn't installed, \
             {cpu} {}",
            packages.install_sentence()
        );
        (Cause::VaPluginMissing, primary, packages, reason)
    } else if present.is_empty() {
        let mut reason = format!(
            "{head} its VA-API driver ({primary}) isn't installed, {cpu} {}",
            driver_packages.install_sentence()
        );
        if intel {
            reason.push_str(&format!(
                " {OLDER_INTEL}: i965-va-driver (Debian/Ubuntu) or libva-intel-driver \
                 (Fedora, Arch)."
            ));
        }
        (Cause::DriverNotInstalled, primary, driver_packages, reason)
    } else {
        // Installed, yet no decoder: the wrong Intel driver for the card's
        // age, a Mesa built without H.264/H.265, or one too old for the card.
        let tried = present[0];
        let (hint, packages) = match (intel, present.as_slice()) {
            (true, ["iHD"]) => (format!("{OLDER_INTEL}."), intel_i965()),
            (true, ["i965"]) => (format!("{NEWER_INTEL}."), intel_media()),
            (true, _) => (
                "The driver may be too old for this graphics card: update it.".to_owned(),
                intel_media(),
            ),
            (false, _) => (
                "The driver may be too old for this graphics card, or built without these \
                 codecs (Fedora's own Mesa leaves them out; RPM Fusion's \
                 mesa-va-drivers-freeworld has them)."
                    .to_owned(),
                mesa_va(),
            ),
        };
        let reason = format!(
            "{head} its VA-API driver ({tried}) is installed yet decodes no H.264 or H.265 \
             here, {cpu} {hint} {} {VAINFO}",
            packages.install_sentence()
        );
        (Cause::DriverFailed, tried, packages, reason)
    };
    MissingDriver {
        reason_code: VAAPI_DRIVER_MISSING,
        vendor,
        kernel_driver: gpu.kernel_driver.clone().unwrap_or_default(),
        render_node: gpu.node.clone(),
        cause,
        va_driver,
        packages,
        reason,
    }
}

fn nvdec_missing(gpu: &RenderGpu, facts: &Facts<'_>) -> MissingDriver {
    let head = "This PC has NVIDIA graphics that can decode video, but";
    let cpu = "so video is decoded on the CPU.";
    let (cause, packages, reason) = if facts.nvcodec_plugin {
        let packages = nvidia_decode();
        let reason = format!(
            "{head} NVIDIA's video decoding library (libnvcuvid) isn't installed, {cpu} \
             Install libnvidia-decode-<driver version> (Ubuntu), {} (Debian), {} (Fedora, \
             from RPM Fusion) or {} (Arch), then restart NeuroWatch.",
            packages.deb.join(" and "),
            packages.rpm.join(" and "),
            packages.arch.join(" and ")
        );
        (Cause::NvdecLibraryMissing, packages, reason)
    } else {
        let packages = gstreamer_bad();
        let reason = format!(
            "{head} GStreamer's NVIDIA plugin (nvcodec, part of the \"bad\" plugins) isn't \
             installed, {cpu} {}",
            packages.install_sentence()
        );
        (Cause::NvcodecPluginMissing, packages, reason)
    };
    MissingDriver {
        reason_code: NVDEC_LIBRARY_MISSING,
        vendor: Vendor::Nvidia,
        kernel_driver: gpu.kernel_driver.clone().unwrap_or_default(),
        render_node: gpu.node.clone(),
        cause,
        va_driver: "",
        packages,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        classify, installed_va_drivers, read_render_gpus, va_driver_dirs, Cause, Facts, RenderGpu,
        Vendor, NVDEC_LIBRARY_MISSING, VAAPI_DRIVER_MISSING,
    };

    fn gpu(vendor: Option<Vendor>, driver: &str) -> RenderGpu {
        RenderGpu {
            node: PathBuf::from("/dev/dri/renderD128"),
            vendor,
            kernel_driver: Some(driver.to_owned()),
            openable: true,
        }
    }

    fn facts<'a>(gpus: &'a [RenderGpu], installed: &'a [String]) -> Facts<'a> {
        Facts {
            gpus,
            va_plugin: true,
            nvcodec_plugin: true,
            installed_va_drivers: installed,
            ..Facts::default()
        }
    }

    fn names(drivers: &[&str]) -> Vec<String> {
        drivers.iter().map(|d| (*d).to_owned()).collect()
    }

    #[test]
    fn pci_vendor_ids_name_the_maker() {
        assert_eq!(Vendor::from_pci_id("0x8086\n"), Some(Vendor::Intel));
        assert_eq!(Vendor::from_pci_id("0x1002"), Some(Vendor::Amd));
        assert_eq!(Vendor::from_pci_id("0X10DE"), Some(Vendor::Nvidia));
        assert_eq!(Vendor::from_pci_id("0x1af4"), None, "virtio GPU");
        assert_eq!(Vendor::from_pci_id("garbage"), None);
    }

    #[cfg(unix)]
    #[test]
    fn render_nodes_are_read_from_sysfs() {
        let dir = std::env::temp_dir().join(format!("nw-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let drm = dir.join("drm");
        for (node, vendor, driver) in [
            ("renderD128", "0x8086\n", "i915"),
            ("renderD129", "0x10de\n", "nvidia"),
        ] {
            let device = drm.join(node).join("device");
            std::fs::create_dir_all(&device).unwrap();
            std::fs::write(device.join("vendor"), vendor).unwrap();
            // A dangling link, as read_link sees sysfs's.
            std::os::unix::fs::symlink(
                format!("../../../bus/pci/drivers/{driver}"),
                device.join("driver"),
            )
            .unwrap();
        }
        let nodes = [
            PathBuf::from("/dev/dri/renderD128"),
            PathBuf::from("/dev/dri/renderD129"),
            PathBuf::from("/dev/dri/renderD130"), // no sysfs entry
        ];
        let gpus = read_render_gpus(&drm, &nodes, |node: &Path| node.ends_with("renderD128"));
        assert_eq!(
            gpus,
            vec![
                RenderGpu {
                    node: nodes[0].clone(),
                    vendor: Some(Vendor::Intel),
                    kernel_driver: Some("i915".into()),
                    openable: true,
                },
                RenderGpu {
                    node: nodes[1].clone(),
                    vendor: Some(Vendor::Nvidia),
                    kernel_driver: Some("nvidia".into()),
                    openable: false,
                },
                RenderGpu {
                    node: nodes[2].clone(),
                    vendor: None,
                    kernel_driver: None,
                    openable: false,
                },
            ]
        );

        let (first, second) = (dir.join("lib1"), dir.join("lib2"));
        for folder in [&first, &second] {
            std::fs::create_dir_all(folder).unwrap();
        }
        for (folder, file) in [
            (&first, "radeonsi_drv_video.so"),
            (&first, "nouveau_drv_video.so"),
            (&first, "radeonsi_dri.so"),
            (&second, "iHD_drv_video.so"),
            (&second, "radeonsi_drv_video.so"),
        ] {
            std::fs::write(folder.join(file), b"").unwrap();
        }
        assert_eq!(
            installed_va_drivers(&[first.clone(), second.clone(), dir.join("missing")]),
            names(&["iHD", "nouveau", "radeonsi"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn libva_drivers_path_replaces_the_default_folders() {
        assert!(va_driver_dirs(None).contains(&PathBuf::from("/usr/lib/x86_64-linux-gnu/dri")));
        assert_eq!(va_driver_dirs(Some("".into())), va_driver_dirs(None));
        assert_eq!(
            va_driver_dirs(Some("/opt/a:/opt/b".into())),
            vec![PathBuf::from("/opt/a"), PathBuf::from("/opt/b")]
        );
    }

    #[test]
    fn intel_without_its_driver_is_told_to_install_it() {
        let gpus = [gpu(Some(Vendor::Intel), "i915")];
        let found = classify(&facts(&gpus, &names(&["radeonsi", "nouveau"]))).unwrap();
        assert_eq!(found.reason_code, VAAPI_DRIVER_MISSING);
        assert_eq!(found.cause, Cause::DriverNotInstalled);
        assert_eq!(found.va_driver, "iHD");
        assert_eq!(found.kernel_driver, "i915");
        assert_eq!(found.packages.deb, ["intel-media-va-driver"]);
        assert_eq!(found.packages.rpm, ["libva-intel-media-driver"]);
        assert_eq!(found.packages.arch, ["intel-media-driver"]);
        assert_eq!(
            found.reason,
            "This PC has Intel graphics that can decode video, but its VA-API driver (iHD) \
             isn't installed, so video is decoded on the CPU. Install intel-media-va-driver \
             (Debian/Ubuntu), libva-intel-media-driver (Fedora) or intel-media-driver (Arch), \
             then restart NeuroWatch. Intel graphics from before 2014 (older than Broadwell) \
             use the i965 driver instead: i965-va-driver (Debian/Ubuntu) or libva-intel-driver \
             (Fedora, Arch)."
        );
        let xe = [gpu(Some(Vendor::Intel), "xe")];
        assert_eq!(classify(&facts(&xe, &[])).unwrap().kernel_driver, "xe");
    }

    #[test]
    fn an_installed_driver_that_decodes_nothing_points_at_the_other_one() {
        let gpus = [gpu(Some(Vendor::Intel), "i915")];
        let ihd_only = classify(&facts(&gpus, &names(&["iHD"]))).unwrap();
        assert_eq!(ihd_only.cause, Cause::DriverFailed);
        assert_eq!(ihd_only.va_driver, "iHD");
        assert_eq!(ihd_only.packages.deb, ["i965-va-driver"], "an older Intel");
        assert!(ihd_only.reason.contains("vainfo"));
        let i965_only = classify(&facts(&gpus, &names(&["i965"]))).unwrap();
        assert_eq!(i965_only.va_driver, "i965");
        assert_eq!(i965_only.packages.deb, ["intel-media-va-driver"]);
        let both = classify(&facts(&gpus, &names(&["i965", "iHD"]))).unwrap();
        assert_eq!(both.packages.deb, ["intel-media-va-driver"]);
    }

    #[test]
    fn amd_cards_need_mesa_and_fedora_the_freeworld_build() {
        let gpus = [gpu(Some(Vendor::Amd), "amdgpu")];
        let missing = classify(&facts(&gpus, &[])).unwrap();
        assert_eq!(
            (missing.cause, missing.va_driver),
            (Cause::DriverNotInstalled, "radeonsi")
        );
        assert_eq!(missing.packages.deb, ["mesa-va-drivers"]);
        assert_eq!(missing.packages.rpm, ["mesa-va-drivers-freeworld"]);
        assert_eq!(missing.packages.arch, ["mesa"]);
        let fedora = classify(&facts(&gpus, &names(&["radeonsi"]))).unwrap();
        assert_eq!(fedora.cause, Cause::DriverFailed);
        assert!(fedora.reason.contains("Fedora's own Mesa"));
        let old = [gpu(Some(Vendor::Amd), "radeon")];
        assert_eq!(classify(&facts(&old, &[])).unwrap().va_driver, "r600");
    }

    #[test]
    fn without_the_va_plugin_gstreamer_is_installed_too() {
        let gpus = [gpu(Some(Vendor::Intel), "i915")];
        let mut without = facts(&gpus, &[]);
        without.va_plugin = false;
        let found = classify(&without).unwrap();
        assert_eq!(found.cause, Cause::VaPluginMissing);
        assert_eq!(
            found.packages.deb,
            ["gstreamer1.0-plugins-bad", "intel-media-va-driver"]
        );
        assert_eq!(
            found.packages.rpm,
            ["gstreamer1-plugins-bad-free", "libva-intel-media-driver"]
        );
        assert_eq!(
            found.packages.arch,
            ["gst-plugins-bad", "intel-media-driver"]
        );
        let installed = names(&["iHD"]);
        let mut driver_there = facts(&gpus, &installed);
        driver_there.va_plugin = false;
        assert_eq!(
            classify(&driver_there).unwrap().packages.deb,
            ["gstreamer1.0-plugins-bad"]
        );
    }

    #[test]
    fn nvidia_on_its_own_driver_needs_the_decode_library() {
        let gpus = [gpu(Some(Vendor::Nvidia), "nvidia")];
        let found = classify(&facts(&gpus, &[])).unwrap();
        assert_eq!(found.reason_code, NVDEC_LIBRARY_MISSING);
        assert_eq!(found.cause, Cause::NvdecLibraryMissing);
        assert_eq!(found.packages.rpm, ["xorg-x11-drv-nvidia-cuda-libs"]);
        assert!(found.reason.contains("libnvidia-decode"));
        let mut without = facts(&gpus, &[]);
        without.nvcodec_plugin = false;
        assert_eq!(
            classify(&without).unwrap().cause,
            Cause::NvcodecPluginMissing
        );
        let mut decoding = facts(&gpus, &[]);
        decoding.nvdec_decodes = true;
        assert_eq!(classify(&decoding), None);
        let nouveau = [gpu(Some(Vendor::Nvidia), "nouveau")];
        let open = classify(&facts(&nouveau, &[])).unwrap();
        assert_eq!(
            (open.reason_code, open.va_driver),
            (VAAPI_DRIVER_MISSING, "nouveau")
        );
        assert!(open.reason.contains("nouveau driver"));
    }

    #[test]
    fn intel_or_amd_is_reported_before_nouveau_and_nvidia() {
        let mut nvidia = gpu(Some(Vendor::Nvidia), "nvidia");
        nvidia.node = PathBuf::from("/dev/dri/renderD128");
        let mut intel = gpu(Some(Vendor::Intel), "i915");
        intel.node = PathBuf::from("/dev/dri/renderD129");
        let gpus = [nvidia, intel];
        let found = classify(&facts(&gpus, &[])).unwrap();
        assert_eq!(found.vendor, Vendor::Intel);
        assert_eq!(found.render_node, PathBuf::from("/dev/dri/renderD129"));
    }

    #[test]
    fn nothing_to_say_when_decoding_works_another_reason_applies_or_no_card() {
        let gpus = [gpu(Some(Vendor::Intel), "i915")];
        let installed = names(&["iHD"]);
        let mut working = facts(&gpus, &installed);
        working.vaapi_decodes = true;
        assert_eq!(classify(&working), None, "the driver decodes");
        let mut blocked = facts(&gpus, &[]);
        blocked.nodes_blocked = true;
        assert_eq!(classify(&blocked), None, "GPU access pending says that");
        let mut excluded = facts(&gpus, &[]);
        excluded.live_view_blocked = true;
        assert_eq!(classify(&excluded), None, "the VA plugin was taken out");
        let mut closed = gpu(Some(Vendor::Intel), "i915");
        closed.openable = false;
        assert_eq!(classify(&facts(&[closed], &[])), None);
        assert_eq!(classify(&facts(&[], &[])), None, "no graphics card");
        let virtual_gpu = [gpu(None, "virtio-pci")];
        assert_eq!(classify(&facts(&virtual_gpu, &[])), None, "a VM's GPU");
    }
}
