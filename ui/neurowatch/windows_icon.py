"""The NeuroWatch logo on Windows taskbar buttons, set through Win32.

Qt gives a frameless window its icon through its own conversion of the
window icon; the taskbar showed the desktop's generic icon instead. Here each
top-level window, as it is shown, gets the logo from ``neurowatch.ico`` (it
ships in the package) at the sizes its screen's scaling needs: ``WM_SETICON``
for the window and the window class's icons, which Windows falls back to.
No-op elsewhere.
"""

from __future__ import annotations

import logging
import sys

from PySide6.QtCore import QEvent, QObject
from PySide6.QtWidgets import QApplication, QWidget

logger = logging.getLogger(__name__)

_WM_SETICON = 0x0080
_ICON_SMALL, _ICON_BIG = 0, 1
_IMAGE_ICON = 1
_LR_LOADFROMFILE = 0x0010
_GCLP_HICON, _GCLP_HICONSM = -14, -34
_APPLIED = "nwTaskbarIcon"
#: Icon sizes at 100% scaling (96 DPI): taskbar / Alt+Tab, and title bar.
_BIG, _SMALL = 32, 16


def icon_sizes(dpi: int) -> tuple[int, int]:
    """(big, small) icon sizes in pixels for a window at ``dpi``."""
    scale = (dpi or 96) / 96
    return round(_BIG * scale), round(_SMALL * scale)


class _Win32:
    """The few user32 calls needed, with 64-bit handle types declared (ctypes
    would otherwise truncate handles to 32-bit ints)."""

    def __init__(self) -> None:
        import ctypes
        from ctypes import wintypes

        # 64-bit Windows has one calling convention, so CDLL fits user32.
        user32 = ctypes.CDLL("user32")
        handle = ctypes.c_void_p
        self.load_image = user32.LoadImageW
        self.load_image.argtypes = [
            wintypes.HINSTANCE,
            wintypes.LPCWSTR,
            wintypes.UINT,
            ctypes.c_int,
            ctypes.c_int,
            wintypes.UINT,
        ]
        self.load_image.restype = handle
        self.send_message = user32.SendMessageW
        self.send_message.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, handle]
        self.send_message.restype = handle
        self.set_class_long = user32.SetClassLongPtrW
        self.set_class_long.argtypes = [wintypes.HWND, ctypes.c_int, handle]
        self.set_class_long.restype = handle
        self.dpi_for_window = user32.GetDpiForWindow
        self.dpi_for_window.argtypes = [wintypes.HWND]
        self.dpi_for_window.restype = wintypes.UINT


def apply_icon(window: QWidget, ico_path: str, win32: _Win32 | None = None) -> bool:
    """Give ``window``'s taskbar button and title the logo. True when set."""
    if sys.platform != "win32" and win32 is None:
        return False
    try:
        api = win32 or _Win32()
        hwnd = int(window.winId())
        big, small = icon_sizes(int(api.dpi_for_window(hwnd)))
        icons = {
            size: api.load_image(None, ico_path, _IMAGE_ICON, size, size, _LR_LOADFROMFILE)
            for size in (big, small)
        }
        if not icons[big] or not icons[small]:
            logger.warning("Could not load the taskbar icon from %s", ico_path)
            return False
        api.send_message(hwnd, _WM_SETICON, _ICON_BIG, icons[big])
        api.send_message(hwnd, _WM_SETICON, _ICON_SMALL, icons[small])
        api.set_class_long(hwnd, _GCLP_HICON, icons[big])
        api.set_class_long(hwnd, _GCLP_HICONSM, icons[small])
    except (AttributeError, OSError, TypeError, ValueError) as e:
        logger.warning("Could not set the taskbar icon: %s", e)
        return False
    return True


class _TaskbarIcons(QObject):
    """Applies the logo to every top-level window when it is first shown."""

    def __init__(self, app: QApplication, ico_path: str) -> None:
        super().__init__(app)
        self._ico_path = ico_path
        app.installEventFilter(self)

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if (
            event.type() == QEvent.Type.Show
            and isinstance(watched, QWidget)
            and watched.isWindow()
            and not watched.property(_APPLIED)
        ):
            watched.setProperty(_APPLIED, True)
            apply_icon(watched, self._ico_path)
        return False


def install(app: QApplication, ico_path: str) -> bool:
    """On Windows, give every NeuroWatch window the logo on the taskbar."""
    if sys.platform != "win32":
        return False
    _TaskbarIcons(app, ico_path)
    logger.info("Taskbar icon: %s", ico_path)
    return True
