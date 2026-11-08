//! Which GStreamer a copy started by hand runs on.
//!
//! Started from a NeuroWatch checkout, this binary links the system's GStreamer,
//! which may be too old (Ubuntu 24.04's 1.24.2 has the VA-API leak, so VA-API is
//! refused). The checkout's release SDK (`scripts/install_gstreamer_dev_sdk.sh`,
//! `.cache/gstreamer-sdk-<version>`) is what NeuroWatch's packages ship, so it is
//! used when it is there. GStreamer's libraries load when the process starts:
//! taking another means starting again with `LD_LIBRARY_PATH` set.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// What the process should do about its GStreamer.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    /// Keep what it has.
    Stay,
    /// Start again on this SDK.
    Use(PathBuf),
}

/// `exe`: this binary. `plugin_path`: `GST_PLUGIN_SYSTEM_PATH_1_0` (set: someone
/// already chose, such as NeuroWatch for the copy it runs, or an earlier start).
/// `requested`: `NW_GSTREAMER_SDK`: `0` keeps the system's, a directory names one,
/// `1` or nothing looks for one. A directory that is not an SDK is ignored here;
/// `is_sdk` is checked by `find`.
pub fn choose(exe: &Path, plugin_path: Option<&OsStr>, requested: Option<&OsStr>) -> Choice {
    if plugin_path.is_some() {
        return Choice::Stay;
    }
    // A release bundle: found through its own RPATH (`use_bundled_gstreamer`).
    if exe
        .parent()
        .and_then(Path::parent)
        .is_some_and(|root| is_sdk(&root.join("gstreamer")))
    {
        return Choice::Stay;
    }
    match requested.map(OsStr::to_string_lossy).as_deref() {
        Some("0") => Choice::Stay,
        Some("1") | Some("") | None => find(exe).map_or(Choice::Stay, Choice::Use),
        Some(path) => {
            let path = PathBuf::from(path);
            if is_sdk(&path) {
                Choice::Use(path)
            } else {
                Choice::Stay
            }
        }
    }
}

fn is_sdk(path: &Path) -> bool {
    path.join("lib").join("gstreamer-1.0").is_dir()
}

/// The newest `.cache/gstreamer-sdk-<version>` of a NeuroWatch checkout: one this
/// binary is inside, or the `Neurowatch` beside a plugin checkout that holds it.
fn find(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .skip(1)
        .flat_map(|dir| [dir.join("Neurowatch"), dir.to_path_buf()])
        .find_map(|checkout| newest_sdk(&checkout.join(".cache")))
}

fn newest_sdk(cache: &Path) -> Option<PathBuf> {
    std::fs::read_dir(cache)
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let version = version_of(path.file_name()?.to_str()?)?;
            is_sdk(&path).then_some((version, path))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path)| path)
}

/// `gstreamer-sdk-1.28.8` → `[1, 28, 8]`: 1.28.10 is newer than 1.28.8.
fn version_of(name: &str) -> Option<Vec<u32>> {
    name.strip_prefix("gstreamer-sdk-")?
        .split('.')
        .map(|part| part.parse().ok())
        .collect()
}

/// The variables that run this process on `sdk`, as NeuroWatch's own
/// `service/manager.py` `gstreamer_sdk_env` sets them for its backend.
pub fn environment(
    sdk: &Path,
    cache: &Path,
    library_path: Option<&OsStr>,
) -> Vec<(&'static str, OsString)> {
    let plugins = sdk.join("lib").join("gstreamer-1.0").into_os_string();
    let mut libraries = sdk.join("lib").into_os_string();
    if let Some(existing) = library_path.filter(|path| !path.is_empty()) {
        libraries.push(":");
        libraries.push(existing);
    }
    let name = sdk
        .file_name()
        .map_or_else(|| "sdk".to_owned(), |n| n.to_string_lossy().into_owned());
    vec![
        ("LD_LIBRARY_PATH", libraries),
        ("GST_PLUGIN_PATH_1_0", plugins.clone()),
        ("GST_PLUGIN_SYSTEM_PATH_1_0", plugins),
        (
            "GST_PLUGIN_SCANNER_1_0",
            sdk.join("libexec")
                .join("gstreamer-1.0")
                .join("gst-plugin-scanner")
                .into_os_string(),
        ),
        (
            "GST_REGISTRY_1_0",
            cache
                .join(format!("gstreamer-registry-{name}.bin"))
                .into_os_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);
    impl Tree {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("nw-motion-gst-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn sdk(&self, at: &str) -> PathBuf {
            let path = self.0.join(at);
            std::fs::create_dir_all(path.join("lib/gstreamer-1.0")).unwrap();
            path
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_plugin_beside_a_checkout_finds_its_newest_sdk() {
        let tree = Tree::new("beside");
        tree.sdk("Neurowatch/.cache/gstreamer-sdk-1.28.8");
        tree.sdk("Neurowatch/.cache/gstreamer-sdk-1.28.10");
        tree.sdk("Neurowatch/.cache/gstreamer-sdk-1.9.0");
        let exe = tree.0.join("Neurowatch-Motion/target/release/nw-motion");
        assert_eq!(
            choose(&exe, None, None),
            Choice::Use(tree.0.join("Neurowatch/.cache/gstreamer-sdk-1.28.10")),
            "by number: 1.28.10 is newer than 1.28.8 and 1.9.0"
        );
    }

    #[test]
    fn a_copy_inside_a_checkout_finds_it_too() {
        let tree = Tree::new("inside");
        tree.sdk(".cache/gstreamer-sdk-1.28.8");
        let exe = tree.0.join("target/debug/nw-motion");
        assert_eq!(
            choose(&exe, None, None),
            Choice::Use(tree.0.join(".cache/gstreamer-sdk-1.28.8"))
        );
    }

    #[test]
    fn someone_elses_choice_wins_and_nothing_found_stays() {
        let tree = Tree::new("others");
        let sdk = tree.sdk("Neurowatch/.cache/gstreamer-sdk-1.28.8");
        let exe = tree.0.join("Neurowatch-Motion/target/release/nw-motion");
        // NeuroWatch chose for the copy it runs, or this is the second start:
        // this is also what stops it starting again and again.
        assert_eq!(choose(&exe, Some(OsStr::new("/x")), None), Choice::Stay);
        assert_eq!(
            choose(&exe, None, Some(OsStr::new("0"))),
            Choice::Stay,
            "asked for the system's"
        );
        assert_eq!(
            choose(&exe, None, Some(OsStr::new("1"))),
            Choice::Use(sdk.clone())
        );
        let other = tree.sdk("elsewhere");
        assert_eq!(
            choose(&exe, None, Some(other.as_os_str())),
            Choice::Use(other)
        );
        assert_eq!(
            choose(&exe, None, Some(OsStr::new("/no/such/sdk"))),
            Choice::Stay,
            "a wrong path does not stop analysis"
        );
        let lone = Tree::new("lone");
        assert_eq!(
            choose(&lone.0.join("bin/nw-motion"), None, None),
            Choice::Stay
        );
        // A partial folder is not an SDK.
        std::fs::create_dir_all(lone.0.join(".cache/gstreamer-sdk-1.28.8")).unwrap();
        assert_eq!(
            choose(&lone.0.join("bin/nw-motion"), None, None),
            Choice::Stay
        );
    }

    #[test]
    fn a_release_bundle_keeps_its_own() {
        let tree = Tree::new("bundle");
        tree.sdk("opt/gstreamer");
        tree.sdk("Neurowatch/.cache/gstreamer-sdk-1.28.8");
        let exe = tree.0.join("opt/bin/nw-motion");
        assert_eq!(choose(&exe, None, None), Choice::Stay);
    }

    #[test]
    fn the_environment_matches_neurowatchs_for_its_backend() {
        let env = environment(
            Path::new("/c/gstreamer-sdk-1.28.8"),
            Path::new("/cache"),
            Some(OsStr::new("/already")),
        );
        let value = |name: &str| env.iter().find(|(n, _)| *n == name).unwrap().1.clone();
        assert_eq!(
            value("LD_LIBRARY_PATH"),
            "/c/gstreamer-sdk-1.28.8/lib:/already"
        );
        assert_eq!(
            value("GST_PLUGIN_SYSTEM_PATH_1_0"),
            "/c/gstreamer-sdk-1.28.8/lib/gstreamer-1.0"
        );
        assert_eq!(
            value("GST_PLUGIN_PATH_1_0"),
            "/c/gstreamer-sdk-1.28.8/lib/gstreamer-1.0"
        );
        assert_eq!(
            value("GST_PLUGIN_SCANNER_1_0"),
            "/c/gstreamer-sdk-1.28.8/libexec/gstreamer-1.0/gst-plugin-scanner"
        );
        assert_eq!(
            value("GST_REGISTRY_1_0"),
            "/cache/gstreamer-registry-gstreamer-sdk-1.28.8.bin"
        );
        let alone = environment(Path::new("/c/s"), Path::new("/cache"), None);
        assert_eq!(alone[0].1, "/c/s/lib");
    }
}
