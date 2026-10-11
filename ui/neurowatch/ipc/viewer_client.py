"""Native viewer IPC client.

Wraps the ``viewer.*`` JSON-RPC surface exposed by nw-backend so the UI can
attach an active camera cell to a native GStreamer ``VideoOverlay`` pipeline
instead of consuming raw RGB frames via SHM.

The client is deliberately thin — no threading, no state — so callers can
choose their own concurrency model (the current live wall calls attach/detach
on the Qt main thread, which is fine at grid-assignment cadence).
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Any

from PySide6.QtGui import QGuiApplication

from neurowatch.ipc import view_lease
from neurowatch.ipc.sync_client import BackendOfflineError, IpcError, SyncIpcClient

logger = logging.getLogger(__name__)


@dataclass(frozen=True)
class ViewerCapabilities:
    """Subset of the backend capability report the UI actually cares about."""

    available: bool
    preferred_sink: str | None
    sinks: tuple[str, ...]
    reason: str | None

    @classmethod
    def from_dict(cls, raw: dict[str, Any] | None) -> ViewerCapabilities:
        raw = raw or {}
        sinks_raw = raw.get("sinks") or []
        return cls(
            available=bool(raw.get("available", False)),
            preferred_sink=raw.get("preferred_sink"),
            sinks=tuple(str(s) for s in sinks_raw),
            reason=raw.get("reason"),
        )


class ViewerUnavailableError(RuntimeError):
    """Raised when a native viewer session cannot be established."""


class NativeViewerClient:
    """Thin wrapper around the ``viewer.*`` JSON-RPC methods."""

    def __init__(self, ipc: SyncIpcClient | None = None) -> None:
        self._ipc = ipc or SyncIpcClient()
        self._capabilities: ViewerCapabilities | None = None

    # ------------------------------------------------------------------
    # Capability discovery — the UI reads this once per session to decide
    # between the native path and the legacy FrameReaderThread fallback.
    # ------------------------------------------------------------------

    def capabilities(self, *, refresh: bool = False) -> ViewerCapabilities:
        if self._capabilities is not None and not refresh:
            return self._capabilities
        try:
            raw = self._ipc.call("viewer.capabilities")
        except (BackendOfflineError, IpcError) as e:
            logger.warning("viewer.capabilities failed (%s); assuming unavailable", e)
            caps = ViewerCapabilities(
                available=False,
                preferred_sink=None,
                sinks=(),
                reason="backend_error",
            )
        else:
            caps = ViewerCapabilities.from_dict(raw if isinstance(raw, dict) else None)
        self._capabilities = caps
        logger.info(
            "Viewer capabilities: available=%s sink=%s reason=%s",
            caps.available,
            caps.preferred_sink,
            caps.reason,
        )
        return caps

    def is_available(self) -> bool:
        # Only X11 IDs can currently be shared with our Linux backend. Qt's
        # Wayland/offscreen winId() values are not valid backend window handles.
        return QGuiApplication.platformName() == "xcb" and self.capabilities().available

    # ------------------------------------------------------------------
    # Session lifecycle
    # ------------------------------------------------------------------

    def attach(self, camera_id: str, window_handle: int, stream: str | None = None) -> str:
        """Attach a native viewer session; returns the session id.

        ``stream`` ("main", "sub" or a stream id) picks what the overlay
        decodes; omitted, the backend uses the camera's main stream.
        """
        if QGuiApplication.platformName() != "xcb":
            raise ViewerUnavailableError("cross-process viewer requires the xcb platform")
        if window_handle <= 0:
            raise ViewerUnavailableError("invalid window handle")
        # The session ends with this UI's view lease, so a crash leaves no decoder.
        params: dict[str, object] = {
            "camera_id": camera_id,
            "window_handle": int(window_handle),
            "client_id": view_lease.CLIENT_ID,
        }
        if stream is not None:
            params["stream"] = stream
        try:
            result = self._ipc.call("viewer.attach", params)
        except (BackendOfflineError, IpcError) as e:
            logger.info(
                "viewer.attach for cam_id=%s failed (%s); UI will fall back",
                camera_id,
                e,
            )
            raise ViewerUnavailableError(str(e)) from e
        if not isinstance(result, dict) or "session_id" not in result:
            raise ViewerUnavailableError("malformed viewer.attach response")
        session_id = str(result["session_id"])
        logger.info("viewer session attached: cam_id=%s session_id=%s", camera_id, session_id)
        return session_id

    def detach(self, session_id: str) -> None:
        try:
            self._ipc.call("viewer.detach", {"session_id": session_id})
        except (BackendOfflineError, IpcError) as e:
            # Detach is best-effort on shutdown — never raise.
            logger.debug("viewer.detach for %s ignored (%s)", session_id, e)

    def set_window(self, session_id: str, window_handle: int) -> None:
        if QGuiApplication.platformName() != "xcb":
            raise ViewerUnavailableError("cross-process viewer requires the xcb platform")
        if window_handle <= 0:
            raise ViewerUnavailableError("invalid window handle")
        try:
            self._ipc.call(
                "viewer.set_window",
                {"session_id": session_id, "window_handle": int(window_handle)},
            )
        except (BackendOfflineError, IpcError) as e:
            raise ViewerUnavailableError(str(e)) from e
