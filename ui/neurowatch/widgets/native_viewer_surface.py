"""A QWidget backed by a native window handle for GStreamer VideoOverlay.

The backend's ``viewer.attach`` binds a GStreamer decode+render pipeline to
the ``winId()`` of an instance of this widget.  No pixel data flows through
Python — the GStreamer sink draws directly into this widget's native
subwindow.
"""

from __future__ import annotations

import logging
from enum import StrEnum

from PySide6.QtCore import Qt
from PySide6.QtGui import QCloseEvent, QColor, QPainter, QPaintEvent, QShowEvent
from PySide6.QtWidgets import QWidget

from neurowatch.ipc.viewer_client import NativeViewerClient, ViewerUnavailableError

logger = logging.getLogger(__name__)


class BindResult(StrEnum):
    """Outcome of a ``NativeViewerSurface.bind()`` call.

    Distinguishing ``PENDING`` (no window handle yet — the attach will
    happen from ``showEvent``) from ``FAILED`` (backend refused — the
    caller must fall back to the raw-frame path) is required so the
    video wall does not silently install an inert surface.
    """

    ATTACHED = "attached"
    PENDING = "pending"
    FAILED = "failed"


class NativeViewerSurface(QWidget):
    """Native-window QWidget that owns one ``viewer.attach`` session.

    Contract:
      * ``bind(cam_id)`` requests an attach the moment a native window handle
        is available.  Safe to call before the widget is shown.
      * ``release()`` tears down the backend session; automatically called
        on close/destroy.
      * ``is_native_active`` is True only while a backend session is live.

    The widget deliberately paints a solid black background so that during
    the brief window between attach and the first decoded frame the cell
    does not flicker to the parent's grid-pattern placeholder.
    """

    def __init__(
        self,
        client: NativeViewerClient,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        # WA_NativeWindow forces Qt to allocate a real X11/Wayland window
        # (a "native" widget) — required for VideoOverlay to have something
        # to attach to.  WA_DontCreateNativeAncestors keeps ancestors from
        # being promoted implicitly (harmless but avoids relayout surprises).
        self.setAttribute(Qt.WidgetAttribute.WA_NativeWindow, True)
        self.setAttribute(Qt.WidgetAttribute.WA_DontCreateNativeAncestors, True)
        # Let the parent CellWidget receive clicks and double-clicks through
        # the native video child. Otherwise an expanded native tile traps the
        # pointer and cannot toggle back to the video wall.
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents, True)
        # OpaquePaintEvent tells Qt we fully paint the widget ourselves, so
        # it does not need to blit the parent behind us on every repaint.
        self.setAttribute(Qt.WidgetAttribute.WA_OpaquePaintEvent, True)
        # NoSystemBackground avoids the flash of the platform default fill
        # between show() and first frame — we paint black in paintEvent.
        self.setAttribute(Qt.WidgetAttribute.WA_NoSystemBackground, True)
        # Never opt into painting on top of the native subwindow — that
        # would race with GStreamer.
        self.setAutoFillBackground(False)
        self._client = client
        self._cam_id: str | None = None
        self._session_id: str | None = None
        self._pending_cam_id: str | None = None
        # Stream selector for the overlay; None keeps the backend default.
        self._stream: str | None = None

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    @property
    def is_native_active(self) -> bool:
        return self._session_id is not None

    @property
    def cam_id(self) -> str | None:
        return self._cam_id

    def bind(self, cam_id: str, stream: str | None = None) -> BindResult:
        """Request a native session for ``cam_id``.

        Returns ``BindResult.ATTACHED`` when the backend accepted the
        attach, ``PENDING`` when the widget has no window handle yet
        (attach will retry from ``showEvent``), or ``FAILED`` when the
        backend rejected the attach and the caller must fall back to the
        raw-frame path.  In every case the caller may inspect
        ``is_native_active`` to distinguish live sessions from pending
        ones.
        """
        if self._session_id is not None and self._cam_id == cam_id and self._stream == stream:
            return BindResult.ATTACHED
        if self._session_id is not None:
            self.release()

        self._cam_id = cam_id
        self._stream = stream
        # If we don't yet have a native window handle (widget not shown),
        # remember the intent — attach will run from showEvent().
        handle = int(self.winId())
        if handle <= 0:
            self._pending_cam_id = cam_id
            return BindResult.PENDING

        return BindResult.ATTACHED if self._try_attach(cam_id, handle) else BindResult.FAILED

    def release(self) -> None:
        if self._session_id is not None:
            logger.debug(
                "Releasing native viewer session cam_id=%s session_id=%s",
                self._cam_id,
                self._session_id,
            )
            self._client.detach(self._session_id)
        self._session_id = None
        self._cam_id = None
        self._pending_cam_id = None

    # ------------------------------------------------------------------
    # Qt event overrides
    # ------------------------------------------------------------------

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        pending = self._pending_cam_id
        if pending is not None and self._session_id is None:
            self._pending_cam_id = None
            handle = int(self.winId())
            if handle > 0:
                self._try_attach(pending, handle)

    def closeEvent(self, event: QCloseEvent) -> None:
        self.release()
        super().closeEvent(event)

    def paintEvent(self, event: QPaintEvent) -> None:
        if self._session_id is not None:
            # GStreamer owns the native subwindow — do not paint over it.
            return
        painter = QPainter(self)
        painter.fillRect(self.rect(), QColor(0, 0, 0))
        painter.end()

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _try_attach(self, cam_id: str, handle: int) -> bool:
        try:
            if self._stream is None:
                session_id = self._client.attach(cam_id, handle)
            else:
                session_id = self._client.attach(cam_id, handle, stream=self._stream)
        except ViewerUnavailableError as e:
            logger.info(
                "Native viewer attach failed for cam_id=%s (%s); falling back",
                cam_id,
                e,
            )
            self._session_id = None
            return False
        self._session_id = session_id
        return True
