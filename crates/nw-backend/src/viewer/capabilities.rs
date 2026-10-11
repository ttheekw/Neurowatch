//! Probe GStreamer for a usable native `VideoOverlay` sink.
//!
//! `glimagesink` and `xvimagesink` implement the `GstVideoOverlay` interface;
//! both let the backend render directly into a UI-owned window handle so the
//! UI process never sees decoded frames.

use serde::Serialize;

/// Reported to the UI so it can decide between the native viewer path and
/// the legacy Python `FrameReaderThread`/SHM fallback.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ViewerCapabilities {
    /// True when an overlay sink is registered and the display environment
    /// supports window handles shared between the UI and backend processes.
    pub available: bool,
    /// Name of the preferred sink factory (e.g. `"glimagesink"`), if any.
    pub preferred_sink: Option<String>,
    /// All `VideoOverlay`-capable sinks that were found (probe order).
    pub sinks: Vec<String>,
    /// Human-readable classification when `available == false`.
    ///
    /// Never contains file paths, URLs, or credentials — only a small
    /// vocabulary such as `"no_display_server"` or `"no_overlay_sink"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ViewerCapabilities {
    pub fn unavailable(reason: &'static str) -> Self {
        Self {
            available: false,
            preferred_sink: None,
            sinks: Vec::new(),
            reason: Some(reason.to_owned()),
        }
    }
}

/// Ordered list of overlay sinks; earlier = preferred.
const OVERLAY_SINKS: &[&str] = &["glimagesink", "xvimagesink", "ximagesink"];

/// Detect whether the local environment can host a native viewer session.
pub fn probe_capabilities() -> ViewerCapabilities {
    let nonempty_env = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
    if let Some(reason) = display_unavailable_reason(
        nonempty_env("DISPLAY").is_some(),
        nonempty_env("WAYLAND_DISPLAY").is_some(),
        nonempty_env("XDG_SESSION_TYPE").as_deref(),
        nonempty_env("GST_GL_WINDOW").as_deref(),
    ) {
        return ViewerCapabilities::unavailable(reason);
    }

    let sinks: Vec<String> = OVERLAY_SINKS
        .iter()
        .filter(|name| gstreamer::ElementFactory::find(name).is_some())
        .map(|name| (*name).to_owned())
        .collect();

    if sinks.is_empty() {
        return ViewerCapabilities::unavailable("no_overlay_sink");
    }

    ViewerCapabilities {
        available: true,
        preferred_sink: sinks.first().cloned(),
        sinks,
        reason: None,
    }
}

fn display_unavailable_reason(
    has_x11: bool,
    has_wayland: bool,
    session_type: Option<&str>,
    gl_window: Option<&str>,
) -> Option<&'static str> {
    // A Wayland wl_surface is a process-local pointer, not a shareable XID.
    // glimagesink may select Wayland even when DISPLAY also exists (XWayland).
    // Never pass a UI handle to that backend: it can segfault asynchronously.
    if has_wayland || session_type == Some("wayland") || gl_window == Some("wayland") {
        Some("cross_process_wayland_unsupported")
    } else if !has_x11 {
        Some("no_display_server")
    } else if gl_window.is_some_and(|window| window != "x11") {
        Some("unsupported_window_system")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{display_unavailable_reason, probe_capabilities, ViewerCapabilities};

    #[test]
    fn wayland_is_unavailable_even_with_xwayland_display() {
        for has_x11 in [false, true] {
            assert_eq!(
                display_unavailable_reason(has_x11, true, None, None),
                Some("cross_process_wayland_unsupported")
            );
        }
        assert_eq!(
            display_unavailable_reason(true, false, Some("wayland"), None),
            Some("cross_process_wayland_unsupported")
        );
        assert_eq!(
            display_unavailable_reason(true, false, None, Some("wayland")),
            Some("cross_process_wayland_unsupported")
        );
    }

    #[test]
    fn only_x11_window_environment_accepts_cross_process_handles() {
        assert_eq!(
            display_unavailable_reason(true, false, Some("x11"), None),
            None
        );
        assert_eq!(
            display_unavailable_reason(true, false, None, Some("x11")),
            None
        );
        assert_eq!(
            display_unavailable_reason(false, false, None, None),
            Some("no_display_server")
        );
        assert_eq!(
            display_unavailable_reason(true, false, None, Some("surfaceless")),
            Some("unsupported_window_system")
        );
    }

    #[test]
    fn capabilities_reason_never_leaks_paths_or_urls() {
        let _ = gstreamer::init();
        let caps = probe_capabilities();
        if let Some(reason) = &caps.reason {
            for forbidden in ['/', ':', '@'] {
                assert!(
                    !reason.contains(forbidden),
                    "capability reason must be a class token, got: {reason:?}"
                );
            }
        }
    }

    #[test]
    fn unavailable_helper_marks_available_false() {
        let caps = ViewerCapabilities::unavailable("no_overlay_sink");
        assert!(!caps.available);
        assert_eq!(caps.reason.as_deref(), Some("no_overlay_sink"));
        assert!(caps.sinks.is_empty());
        assert!(caps.preferred_sink.is_none());
    }
}
