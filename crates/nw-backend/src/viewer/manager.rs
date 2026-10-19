//! Session registry — one entry per active native viewer session.

use std::collections::HashMap;
use std::sync::Mutex;

use nw_core::camera::CameraConfig;
use uuid::Uuid;

use super::capabilities::ViewerCapabilities;
use super::session::{ViewerSession, ViewerSessionId};
use super::ViewerError;
use crate::pipeline::hwaccel::HwAccelKind;

/// Platform-native window handle passed from the UI.
///
/// The exact interpretation matches `GstVideoOverlay::set_window_handle`:
///   - X11: XID
///   - Win32: HWND
///
/// Wayland surface pointers are process-local and are not supported by this IPC.
///
/// The variant carries a `u64` so it round-trips through JSON without loss
/// on all supported platforms.
#[derive(Debug, Clone, Copy)]
pub enum WindowHandle {
    /// Platform-agnostic raw handle.
    Raw(u64),
}

impl WindowHandle {
    pub fn raw(&self) -> usize {
        match self {
            Self::Raw(h) => *h as usize,
        }
    }
}

pub struct ViewerManager {
    hwaccel: HwAccelKind,
    capabilities: ViewerCapabilities,
    sessions: Mutex<HashMap<ViewerSessionId, ViewerSession>>,
    /// The desktop UI client that attached each session, so its sessions
    /// end with its view lease (a UI that crashed leaves no decoder behind).
    owners: Mutex<HashMap<ViewerSessionId, String>>,
    /// Shared camera connections: an RTSP/RTMP session decodes its stream's
    /// hub instead of connecting itself.
    hubs: Option<std::sync::Arc<crate::streams::StreamHubs>>,
}

impl ViewerManager {
    pub fn new(hwaccel: HwAccelKind, capabilities: ViewerCapabilities) -> Self {
        Self {
            hwaccel,
            capabilities,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            hubs: None,
        }
    }

    /// Decode RTSP/RTMP cameras from their shared connection (`hubs`).
    pub fn with_stream_hubs(mut self, hubs: std::sync::Arc<crate::streams::StreamHubs>) -> Self {
        self.hubs = Some(hubs);
        self
    }

    pub fn capabilities(&self) -> &ViewerCapabilities {
        &self.capabilities
    }

    /// Create a new session bound to `handle` for `camera`.  Returns the
    /// generated session id.
    pub fn attach(
        &self,
        camera: &CameraConfig,
        handle: WindowHandle,
    ) -> Result<ViewerSessionId, ViewerError> {
        if !self.capabilities.available {
            return Err(ViewerError::Unavailable {
                reason: self
                    .capabilities
                    .reason
                    .clone()
                    .unwrap_or_else(|| "unavailable".to_owned()),
            });
        }
        let sink_factory =
            self.capabilities
                .preferred_sink
                .as_deref()
                .ok_or(ViewerError::Unavailable {
                    reason: "no_overlay_sink".to_owned(),
                })?;
        let session_id = format!("vw-{}", Uuid::new_v4());
        let session = ViewerSession::launch(
            session_id.clone(),
            camera,
            self.hwaccel,
            sink_factory,
            handle,
            self.hubs.as_ref(),
        )?;
        self.sessions
            .lock()
            .expect("viewer session registry poisoned")
            .insert(session_id.clone(), session);
        tracing::info!(
            camera_id = %camera.id,
            session_id = %session_id,
            sink = %sink_factory,
            "viewer session attached",
        );
        Ok(session_id)
    }

    /// Rebind an existing session to a new window handle.
    pub fn set_window(&self, session_id: &str, handle: WindowHandle) -> Result<(), ViewerError> {
        let sessions = self
            .sessions
            .lock()
            .expect("viewer session registry poisoned");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| ViewerError::UnknownSession {
                session_id: session_id.to_owned(),
            })?;
        session.set_window(handle)
    }

    /// Record the desktop UI client a session belongs to.
    pub fn set_owner(&self, session_id: &str, client_id: &str) {
        if let Ok(mut owners) = self.owners.lock() {
            owners.insert(session_id.to_owned(), client_id.to_owned());
        }
    }

    /// Stop every session `client_id` attached; returns how many.
    pub fn detach_owner(&self, client_id: &str) -> usize {
        let owned: Vec<String> = self
            .owners
            .lock()
            .map(|owners| {
                owners
                    .iter()
                    .filter(|(_, owner)| owner.as_str() == client_id)
                    .map(|(session, _)| session.clone())
                    .collect()
            })
            .unwrap_or_default();
        owned
            .iter()
            .filter(|session| self.detach(session).is_ok())
            .count()
    }

    /// Stop and forget a session.  Errors if `session_id` is unknown.
    pub fn detach(&self, session_id: &str) -> Result<(), ViewerError> {
        if let Ok(mut owners) = self.owners.lock() {
            owners.remove(session_id);
        }
        let mut sessions = self
            .sessions
            .lock()
            .expect("viewer session registry poisoned");
        if sessions.remove(session_id).is_some() {
            tracing::info!(session_id = %session_id, "viewer session detached");
            Ok(())
        } else {
            Err(ViewerError::UnknownSession {
                session_id: session_id.to_owned(),
            })
        }
    }

    /// Count of live sessions: each decodes one camera for the desktop.
    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .expect("viewer session registry poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::{ViewerCapabilities, ViewerManager, WindowHandle};
    use crate::pipeline::hwaccel::HwAccelKind;
    use crate::viewer::ViewerError;
    use nw_core::camera::CameraConfig;

    fn unavailable_manager() -> ViewerManager {
        ViewerManager::new(
            HwAccelKind::Software,
            ViewerCapabilities::unavailable("no_overlay_sink"),
        )
    }

    #[test]
    fn attach_when_unavailable_returns_unavailable_error_without_leaking_url() {
        let mgr = unavailable_manager();
        let mut cam = CameraConfig::new("Cam", "rtsp://alice:s3cret@camera.example/live");
        cam.username = Some("alice".to_owned());
        cam.password_b64 = Some("czNjcmV0".to_owned());

        let err = mgr
            .attach(&cam, WindowHandle::Raw(1234))
            .expect_err("must fail when viewer is unavailable");

        let msg = err.to_string();
        for forbidden in ["alice", "s3cret", "czNjcmV0", "camera.example", "rtsp://"] {
            assert!(
                !msg.contains(forbidden),
                "ViewerError leaked {forbidden:?}: {msg}"
            );
        }
        assert!(matches!(err, ViewerError::Unavailable { .. }));
    }

    #[test]
    fn detach_unknown_session_returns_typed_error() {
        let mgr = unavailable_manager();
        let err = mgr
            .detach("vw-does-not-exist")
            .expect_err("detach must fail for unknown session");
        match err {
            ViewerError::UnknownSession { session_id } => {
                assert_eq!(session_id, "vw-does-not-exist");
            }
            other => panic!("expected UnknownSession, got {other:?}"),
        }
    }

    #[test]
    fn set_window_unknown_session_returns_typed_error() {
        let mgr = unavailable_manager();
        let err = mgr
            .set_window("vw-missing", WindowHandle::Raw(1))
            .expect_err("set_window must fail for unknown session");
        assert!(matches!(err, ViewerError::UnknownSession { .. }));
    }

    #[test]
    fn session_count_starts_at_zero() {
        let mgr = unavailable_manager();
        assert_eq!(mgr.session_count(), 0);
    }
}
