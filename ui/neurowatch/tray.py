"""NeuroWatch's taskbar / system tray icon (Windows and Linux).

A small process of its own (``python -m neurowatch.tray``, or
``neurowatch-ui --tray`` in a packaged build) that shows whether the
background service is running. Hovering shows its status (cameras online,
recording, decoder, and what extensions add); the menu repeats the status, opens the app, and
starts, stops or restarts the service. It starts at login (see
``neurowatch.service.manager``) and makes sure the service is running then.

Linux desktops show it through the StatusNotifierItem protocol: KDE, Xfce,
Cinnamon and Ubuntu's GNOME (its AppIndicator extension) do; plain GNOME
needs the AppIndicator extension. Desktops that do not show tray tooltips
still show the status at the top of the menu.
"""

from __future__ import annotations

import logging
import subprocess
import sys
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any

from PySide6.QtCore import QObject, QPointF, QRectF, Qt, QThread, Signal
from PySide6.QtGui import QAction, QColor, QIcon, QPainter, QPainterPath, QPen, QPixmap
from PySide6.QtWidgets import QApplication, QMenu, QSystemTrayIcon

from neurowatch import extensions
from neurowatch.models.camera import connection_status
from neurowatch.service import probe
from neurowatch.service.lock import InstanceLock
from neurowatch.service.manager import ServiceController
from neurowatch.service.paths import IS_WINDOWS, lock_path, log_path, service_endpoint, ui_command
from neurowatch.theme import apply_app_theme, menu_stylesheet
from neurowatch.widgets.centered_prompt import CenteredPrompt

logger = logging.getLogger(__name__)

POLL_MS = 3000
#: At login the tray may start before the desktop's tray host.
TRAY_HOST_WAIT_MS = 60_000

# Icon states: the dot at the bottom right shows the overall health.
RUNNING, WARNING, CRITICAL, STOPPED, BUSY = "running", "warning", "critical", "stopped", "busy"
_BUSY_TEXT = {
    "start": "Starting the service…",
    "stop": "Stopping the service…",
    "restart": "Restarting the service…",
}
_STATE_COLORS = {
    RUNNING: "#22c55e",
    WARNING: "#f59e0b",
    CRITICAL: "#ef4444",
    STOPPED: "#ef4444",
    BUSY: "#94a3b8",
}
_ALERT_COLOR = "#ef4444"
#: Diagnostics bottlenecks in words that do not change from poll to poll
#: (the backend's explanation carries live percentages; it goes in the menu).
_BOTTLENECK_LABELS = {
    "cpu": "High CPU load",
    "gpu": "GPU overloaded",
    "ram": "Memory running low",
    "ipc": "Backend link slow",
    "decode_render": "Video decoding falling behind",
}
#: Problems shown on the tooltip's last line, at most.
_TOOLTIP_PROBLEMS = 2


@dataclass(frozen=True)
class Problem:
    """Something that needs attention while the service runs."""

    label: str  # short and stable: the tooltip
    detail: str = ""  # may change from poll to poll: the menu


@dataclass(frozen=True)
class ServiceStatus:
    """What the tray shows; built from the backend's status calls."""

    running: bool
    site: str = ""
    cameras: int = 0
    online: int = 0
    recording: int = 0
    # Extension details (see ``neurowatch.extensions``), e.g. "Name: state".
    details: tuple[str, ...] = ()
    decoder: str = ""
    version: str = ""
    uptime_sec: int = 0
    problems: tuple[Problem, ...] = ()

    @property
    def state(self) -> str:
        """The bottom-right dot: green when all is well, amber with a
        problem, red when stopped or when no camera can record (all
        offline). A running service with nothing recording is not green."""
        if not self.running:
            return STOPPED
        if self.cameras and self.online == 0:
            return CRITICAL
        if self.problems:
            return WARNING
        return RUNNING

    @property
    def alert(self) -> bool:
        """Show the red problem badge (a stopped service has its red dot)."""
        return self.running and bool(self.problems)

    def lines(self) -> list[str]:
        """The status in three short lines (the hover tooltip), and one
        naming the problems. Nothing that changes by itself, such as the
        uptime: a tooltip re-set while shown redraws, and would flicker
        under the mouse."""
        if not self.running:
            return ["Stopped — cameras are not recording"]
        lines = ["Running" + (f" · {self.site}" if self.site else "")]
        if self.cameras:
            lines.append(
                f"Cameras {self.online}/{self.cameras} online · {self.recording} recording"
            )
        else:
            lines.append("No cameras added yet")
        details = [part for part in (*self.details, self.decoder) if part]
        if details:
            lines.append(" · ".join(details))
        if self.problems:
            shown = [problem.label for problem in self.problems[:_TOOLTIP_PROBLEMS]]
            more = len(self.problems) - len(shown)
            lines.append("⚠ " + " · ".join(shown) + (f" · {more} more" if more else ""))
        return lines

    def menu_lines(self) -> list[str]:
        """The status with the uptime, and each problem in full (the menu)."""
        lines = self.lines()
        if self.problems:
            lines.pop()
        if self.running and self.uptime_sec:
            lines[0] += f" · up {format_uptime(self.uptime_sec)}"
        lines.extend(f"⚠ {problem.detail or problem.label}" for problem in self.problems)
        return lines

    def tooltip(self) -> str:
        return "\n".join(["NeuroWatch", *self.lines()])


def format_uptime(seconds: int) -> str:
    minutes, _ = divmod(max(0, int(seconds)), 60)
    hours, minutes = divmod(minutes, 60)
    days, hours = divmod(hours, 24)
    if days:
        return f"{days} d {hours} h"
    if hours:
        return f"{hours} h {minutes} min"
    return f"{minutes} min"


def _plural(count: int, noun: str) -> str:
    return f"{count} {noun}{'' if count == 1 else 's'}"


def find_problems(
    *,
    cameras: int,
    online: int,
    recording: Mapping[str, Any],
    performance: Any,
) -> tuple[Problem, ...]:
    """What needs attention: cameras offline, recordings that failed, and
    diagnostics warnings. Extensions add their own (see ``read_status``)."""
    problems: list[Problem] = []
    offline = cameras - online
    if offline > 0:
        problems.append(Problem(f"{_plural(offline, 'camera')} offline"))
    failed = sum(1 for state in recording.values() if state == "failed")
    if failed:
        problems.append(Problem(f"Recording failed on {_plural(failed, 'camera')}"))
    bottleneck = performance.get("bottleneck") if isinstance(performance, Mapping) else None
    if isinstance(bottleneck, Mapping) and bottleneck.get("severity") in ("warning", "critical"):
        label = _BOTTLENECK_LABELS.get(str(bottleneck.get("kind")), "Performance problem")
        if bottleneck.get("severity") == "critical":
            label += " (critical)"
        explanation = str(bottleneck.get("explanation") or "")
        problems.append(Problem(label, f"{label}: {explanation}" if explanation else ""))
    return tuple(problems)


def read_status(
    request: Callable[..., Any] = probe.request, endpoint: str | None = None
) -> ServiceStatus:
    """Ask the backend for everything the tray shows (quietly when stopped)."""
    target = endpoint or service_endpoint()
    system = request("system.status", endpoint=target, timeout=1.5)
    if not isinstance(system, Mapping) or system.get("status") != "running":
        return ServiceStatus(running=False)
    states = request("camera.status_all", endpoint=target, timeout=1.5)
    recording = request("recording.status_all", endpoint=target, timeout=1.5)
    performance = request("stats.get_performance", endpoint=target, timeout=1.5)
    states = states if isinstance(states, Mapping) else {}
    recording = recording if isinstance(recording, Mapping) else {}
    acceleration = system.get("video_acceleration")
    decoder = acceleration.get("label", "") if isinstance(acceleration, Mapping) else ""
    cameras = int(system.get("cameras") or len(states))
    online = sum(1 for state in states.values() if connection_status(str(state)) == "streaming")
    details: list[str] = []
    extra_problems: list[Problem] = []
    for extra in extensions.call_each("tray_status", request, target):
        if isinstance(extra, extensions.ExtensionStatus):
            if extra.detail:
                details.append(extra.detail)
            extra_problems += [Problem(label, explanation) for label, explanation in extra.problems]
    return ServiceStatus(
        running=True,
        site=str(system.get("site_name") or ""),
        cameras=cameras,
        online=online,
        recording=sum(1 for state in recording.values() if state == "recording"),
        details=tuple(details),
        decoder=str(decoder),
        version=str(system.get("version") or ""),
        uptime_sec=int(system.get("uptime_sec") or 0),
        problems=find_problems(
            cameras=cameras,
            online=online,
            recording=recording,
            performance=performance,
        )
        + tuple(extra_problems),
    )


def tray_icon(state: str, alert: bool = False, size: int = 64) -> QIcon:
    """A camera lens on a dark tile; a health dot at the bottom right (see
    ``ServiceStatus.state``), and a small red badge at the top right while
    something needs attention."""
    pixmap = QPixmap(size, size)
    pixmap.fill(Qt.GlobalColor.transparent)
    painter = QPainter(pixmap)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing)
    s = size / 64.0
    tile = QPainterPath()
    tile.addRoundedRect(QRectF(4 * s, 4 * s, 56 * s, 56 * s), 14 * s, 14 * s)
    painter.fillPath(tile, QColor("#1d3a6e"))
    painter.setPen(QPen(QColor("#e2e8f0"), 5 * s))
    painter.setBrush(Qt.BrushStyle.NoBrush)
    painter.drawEllipse(QPointF(30 * s, 32 * s), 15 * s, 15 * s)
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(QColor("#e2e8f0"))
    painter.drawEllipse(QPointF(30 * s, 32 * s), 6 * s, 6 * s)
    painter.setBrush(QColor("#0b1220"))
    painter.drawEllipse(QPointF(48 * s, 48 * s), 13 * s, 13 * s)
    painter.setBrush(QColor(_STATE_COLORS.get(state, _STATE_COLORS[BUSY])))
    painter.drawEllipse(QPointF(48 * s, 48 * s), 9 * s, 9 * s)
    if alert:
        painter.setBrush(QColor("#0b1220"))
        painter.drawEllipse(QPointF(51 * s, 13 * s), 11 * s, 11 * s)
        painter.setBrush(QColor(_ALERT_COLOR))
        painter.drawEllipse(QPointF(51 * s, 13 * s), 8 * s, 8 * s)
    painter.end()
    return QIcon(pixmap)


class _StatusWorker(QThread):
    """Reads the service's status every few seconds, off the GUI thread."""

    status_read = Signal(object)  # ServiceStatus

    def __init__(self, read: Callable[[], ServiceStatus], parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._read = read
        self._wake = False

    def refresh_now(self) -> None:
        self._wake = True

    def run(self) -> None:
        while not self.isInterruptionRequested():
            try:
                self.status_read.emit(self._read())
            except Exception:
                logger.exception("Reading the service status failed")
            waited = 0
            while waited < POLL_MS and not self._wake and not self.isInterruptionRequested():
                QThread.msleep(100)
                waited += 100
            self._wake = False


class _ServiceAction(QThread):
    """Starts, stops or restarts the service off the GUI thread."""

    finished_with = Signal(str, bool)  # action, ok

    def __init__(self, action: str, controller: ServiceController) -> None:
        super().__init__()
        self._action = action
        self._controller = controller

    def run(self) -> None:
        ok = False
        try:
            if self._action == "start":
                ok = self._controller.ensure_running(autostart=False)
            elif self._action == "stop":
                ok = self._controller.stop()
            elif self._action == "restart":
                ok = self._controller.restart()
        except Exception:
            logger.exception("Service %s failed", self._action)
        self.finished_with.emit(self._action, ok)


class TrayApp(QObject):
    """The tray icon, its menu, and the status it shows."""

    def __init__(
        self,
        controller: ServiceController | None = None,
        read: Callable[[], ServiceStatus] | None = None,
        launch_ui: Callable[[], None] | None = None,
        ensure_service: bool = True,
    ) -> None:
        super().__init__()
        self._controller = controller or ServiceController(sleep=_qt_sleep)
        self._launch_ui = launch_ui or _launch_ui
        self._status = ServiceStatus(running=False)
        self._busy: str | None = None
        self._action: _ServiceAction | None = None
        self._shown_state: tuple[str, bool] | None = None
        self._shown_tooltip = ""

        self.icon = QSystemTrayIcon(tray_icon(BUSY))
        self.icon.setToolTip("NeuroWatch\nChecking the service…")
        self.icon.activated.connect(self._on_activated)
        self.menu = QMenu()
        self.menu.setStyleSheet(menu_stylesheet())
        self._status_actions: list[QAction] = []
        self._status_anchor = self.menu.addSeparator()
        self.open_action = self.menu.addAction("Open NeuroWatch")
        self.open_action.triggered.connect(self._launch_ui)
        self.menu.addSeparator()
        self.start_action = self.menu.addAction("Start service")
        self.start_action.triggered.connect(lambda: self.run_action("start"))
        self.stop_action = self.menu.addAction("Stop service")
        self.stop_action.triggered.connect(self._confirm_stop)
        self.restart_action = self.menu.addAction("Restart service")
        self.restart_action.triggered.connect(lambda: self.run_action("restart"))
        self.menu.addSeparator()
        self.log_action = self.menu.addAction("Service log location…")
        self.log_action.triggered.connect(self._show_log_location)
        self.exit_action = self.menu.addAction("Exit")
        self.exit_action.triggered.connect(self._confirm_exit)
        self.icon.setContextMenu(self.menu)

        self._worker = _StatusWorker(read or read_status, self)
        self._worker.status_read.connect(self.show_status)
        self.show_status(self._status, checking=True)
        if ensure_service:
            self.run_action("start")

    def start(self) -> None:
        self.icon.show()
        self._worker.start()

    def shutdown(self) -> None:
        self._worker.requestInterruption()
        self._worker.wait(3000)
        if self._action is not None:
            self._action.wait(5000)
        self.icon.hide()

    # -- status --------------------------------------------------------

    def show_status(self, status: ServiceStatus, checking: bool = False) -> None:
        """Show ``status``; only what changed is touched. Re-setting the same
        tooltip or icon every poll made a tooltip under the mouse flicker."""
        self._status = status
        state = BUSY if (self._busy or checking) else status.state
        alert = status.alert and state != BUSY
        if self._busy:
            lines = menu_lines = [_BUSY_TEXT.get(self._busy, "Working…")]
        elif checking:
            lines = menu_lines = ["Checking the service…"]
        else:
            lines, menu_lines = status.lines(), status.menu_lines()
        if (state, alert) != self._shown_state:
            self._shown_state = (state, alert)
            self.icon.setIcon(tray_icon(state, alert))
        tooltip = "\n".join(["NeuroWatch", *lines])
        if tooltip != self._shown_tooltip:
            self._shown_tooltip = tooltip
            self.icon.setToolTip(tooltip)
        self._show_menu_lines(menu_lines)
        idle = self._busy is None and not checking
        self.start_action.setVisible(not status.running)
        self.start_action.setEnabled(idle)
        self.stop_action.setVisible(status.running)
        self.stop_action.setEnabled(idle)
        self.restart_action.setEnabled(idle and status.running)

    def _show_menu_lines(self, lines: list[str]) -> None:
        texts = [action.text() for action in self._status_actions]
        if texts == lines:
            return
        if len(texts) == len(lines):
            for action, line in zip(self._status_actions, lines, strict=True):
                action.setText(line)
            return
        for action in self._status_actions:
            self.menu.removeAction(action)
        self._status_actions = []
        for line in lines:
            action = QAction(line, self.menu)
            action.setEnabled(False)
            self.menu.insertAction(self._status_anchor, action)
            self._status_actions.append(action)

    @property
    def status(self) -> ServiceStatus:
        return self._status

    # -- actions -------------------------------------------------------

    def run_action(self, action: str) -> None:
        if self._busy is not None:
            return
        self._busy = action
        self.show_status(self._status)
        worker = _ServiceAction(action, self._controller)
        worker.finished_with.connect(self._on_action_done)
        self._action = worker
        worker.start()

    def _on_action_done(self, action: str, ok: bool) -> None:
        self._busy = None
        if self._action is not None:
            self._action.wait()
            self._action.deleteLater()
            self._action = None
        if not ok:
            logger.warning("Service %s did not complete", action)
            self.icon.showMessage(
                "NeuroWatch",
                f"The service did not {action}. Log: {log_path('service')}",
                QSystemTrayIcon.MessageIcon.Warning,
            )
        self.show_status(self._status)
        self._worker.refresh_now()

    def _confirm_stop(self) -> None:
        prompt = CenteredPrompt(
            "Stop NeuroWatch",
            "Stop the NeuroWatch service?",
            "Cameras stop recording until the service starts again.",
        )
        prompt.add_button("cancel", "Cancel")
        prompt.add_button("stop", "Stop service", "danger")
        prompt.set_default("cancel")
        if prompt.choose() == "stop":
            self.run_action("stop")

    def _confirm_exit(self) -> None:
        prompt = CenteredPrompt(
            "Exit NeuroWatch tray",
            "Remove the NeuroWatch icon?",
            "The service can keep recording in the background, or stop with the icon.",
        )
        prompt.add_button("cancel", "Cancel")
        prompt.add_button("stop", "Stop service and exit", "danger")
        prompt.add_button("keep", "Exit, keep recording", "primary")
        prompt.set_default("keep")
        choice = prompt.choose()
        if choice == "keep":
            QApplication.quit()
        elif choice == "stop":
            self._busy = "stop"
            self.show_status(self._status)
            worker = _ServiceAction("stop", self._controller)
            worker.finished_with.connect(lambda *_: QApplication.quit())
            self._action = worker
            worker.start()

    def _show_log_location(self) -> None:
        prompt = CenteredPrompt(
            "NeuroWatch service log",
            "The service writes its log to:",
            str(log_path("service")),
        )
        prompt.add_button("ok", "OK", "primary")
        prompt.set_default("ok")
        prompt.choose()

    def _on_activated(self, reason: QSystemTrayIcon.ActivationReason) -> None:
        if reason in (
            QSystemTrayIcon.ActivationReason.Trigger,
            QSystemTrayIcon.ActivationReason.DoubleClick,
        ):
            self._launch_ui()


def _qt_sleep(seconds: float) -> None:
    QThread.msleep(max(1, int(seconds * 1000)))


def _launch_ui() -> None:
    """Open the desktop app (a running one comes to the front)."""
    kwargs: dict[str, Any] = {"stdin": subprocess.DEVNULL, "close_fds": True}
    if IS_WINDOWS:
        kwargs["creationflags"] = subprocess.DETACHED_PROCESS  # type: ignore[attr-defined]
    else:
        kwargs["start_new_session"] = True
    try:
        subprocess.Popen(ui_command(), **kwargs)
    except OSError:
        logger.exception("Could not open NeuroWatch")


def run(argv: list[str] | None = None) -> int:
    """Run the tray until Exit; one per user (a second one exits at once)."""
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)-7s tray: %(message)s")
    lock = InstanceLock(lock_path("tray"))
    if not lock.acquire():
        logger.info("The NeuroWatch tray is already running")
        return 0
    app = QApplication.instance() or QApplication(argv if argv is not None else sys.argv)
    assert isinstance(app, QApplication)
    app.setApplicationName("NeuroWatch")
    app.setQuitOnLastWindowClosed(False)  # dialogs closing must not end the tray
    apply_app_theme(app)

    waited = 0
    while not QSystemTrayIcon.isSystemTrayAvailable() and waited < TRAY_HOST_WAIT_MS:
        QThread.msleep(500)
        waited += 500
    if not QSystemTrayIcon.isSystemTrayAvailable():
        logger.error(
            "No system tray on this desktop (on GNOME, enable the AppIndicator extension);"
            " the service keeps running without an icon"
        )
        ServiceController(sleep=_qt_sleep).ensure_running(autostart=False)
        lock.release()
        return 1

    tray = TrayApp()
    tray.start()
    app.aboutToQuit.connect(tray.shutdown)
    try:
        return app.exec()
    finally:
        lock.release()


if __name__ == "__main__":
    sys.exit(run())
