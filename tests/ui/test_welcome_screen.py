"""Startup welcome screen: public lifecycle and main-window startup sequencing."""

from __future__ import annotations

import pytest
from PySide6.QtCore import QElapsedTimer, QEvent, QEventLoop, QObject, Qt, QThread, QTimer
from PySide6.QtWidgets import QApplication, QFrame, QLabel, QWidget

from neurowatch import theme
from neurowatch.ipc.sync_client import BackendOfflineError, IpcError
from neurowatch.widgets.welcome_screen import StartupStage, WelcomeScreen


@pytest.fixture
def welcome(qtbot):
    screen = WelcomeScreen(version="9.9.9-test")
    qtbot.addWidget(screen)
    yield screen
    screen.close()


# ── Welcome screen widget ───────────────────────────────────────────────


def _card(welcome: WelcomeScreen) -> QFrame:
    card = welcome.findChild(QFrame, "welcomeCard")
    assert card is not None
    return card


def test_welcome_screen_is_a_compact_branded_card_over_a_transparent_backdrop(welcome, qtbot):
    welcome.present()
    qtbot.waitExposed(welcome)
    flags = welcome.windowFlags()
    assert flags & Qt.WindowType.FramelessWindowHint
    assert flags & Qt.WindowType.SplashScreen
    # The desktop stays visible: only the card is painted.
    assert welcome.testAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
    card = _card(welcome)
    assert card.width() <= 640 and card.height() <= 400
    assert "NeuroWatch" in welcome.title_text()
    assert "9.9.9-test" in welcome.version_text()
    assert theme.COLORS.surface in welcome.styleSheet()
    assert theme.COLORS.accent in welcome.styleSheet()


def test_welcome_screen_only_takes_clicks_on_the_card(welcome, qtbot):
    welcome.present()
    qtbot.waitExposed(welcome)
    assert welcome.mask().boundingRect() == _card(welcome).geometry()


@pytest.mark.parametrize("can_position", [True, False], ids=["x11", "wayland"])
def test_welcome_card_is_centred_without_blacking_out_the_screen(
    welcome, qtbot, monkeypatch, can_position
):
    """Wayland ignores move(), and GNOME fills behind fullscreen windows with black.

    So the splash never goes fullscreen: on X11 it is a card-sized window moved
    to the centre; on Wayland a transparent maximized window centres the card.
    """
    import neurowatch.widgets.welcome_screen as ws

    monkeypatch.setattr(ws, "_can_position_windows", lambda: can_position)
    if not can_position:
        monkeypatch.setattr(welcome, "move", lambda *args: None)
    welcome.present()
    qtbot.waitExposed(welcome)
    if not can_position:
        qtbot.waitUntil(welcome.isMaximized)
    assert not welcome.isFullScreen()
    card = _card(welcome)
    area = welcome.screen().availableGeometry()
    centre = card.mapToGlobal(card.rect().center())
    assert abs(centre.x() - area.center().x()) <= 2
    assert abs(centre.y() - area.center().y()) <= 2
    if can_position:
        assert welcome.size() == card.size(), "X11 needs no transparent backdrop"


def test_stages_are_ordered_real_startup_steps():
    labels = [stage.label for stage in StartupStage]
    assert labels[0] == "Starting NeuroWatch"
    assert "Connecting to backend" in labels
    assert "Preparing workspace" in labels
    assert "Loading cameras" in labels
    assert labels[-1] == "Opening control room"


def test_set_stage_updates_text_progress_and_processes_events(welcome):
    welcome.show()
    ran: list[bool] = []
    QTimer.singleShot(0, lambda: ran.append(True))

    welcome.set_stage(StartupStage.CONNECTING, "Attempt 2 of 3")

    assert ran == [True], "set_stage must pump the Qt event loop"
    assert welcome.current_stage is StartupStage.CONNECTING
    assert welcome.stage_text() == "Connecting to backend"
    assert welcome.detail_text() == "Attempt 2 of 3"
    total = len(StartupStage)
    index = list(StartupStage).index(StartupStage.CONNECTING) + 1
    assert welcome.progress() == (index, total)
    assert welcome.step_text() == f"Step {index} of {total}"

    welcome.set_stage(StartupStage.PREPARING_WORKSPACE)
    assert welcome.detail_text() == ""


def test_finish_closes_immediately_when_window_already_shown(welcome, qtbot):
    welcome.show()
    window = QWidget()
    qtbot.addWidget(window)
    window.show()
    with qtbot.waitSignal(welcome.dismissed, timeout=1000):
        welcome.finish(window)
    assert welcome.is_dismissed
    assert not welcome.isVisible()


def test_finish_waits_until_window_is_shown(welcome, qtbot):
    welcome.show()
    window = QWidget()
    qtbot.addWidget(window)
    welcome.finish(window)
    assert welcome.isVisible()
    assert not welcome.is_dismissed
    with qtbot.waitSignal(welcome.dismissed, timeout=1000):
        window.show()
    assert not welcome.isVisible()


def test_set_stage_after_dismiss_is_ignored(welcome):
    welcome.show()
    welcome.set_stage(StartupStage.LOADING_CAMERAS)
    welcome.dismiss()
    welcome.set_stage(StartupStage.CONNECTING)
    assert welcome.current_stage is StartupStage.LOADING_CAMERAS
    assert not welcome.isVisible()


# ── main.py startup sequencing ──────────────────────────────────────────


class _RecordingWelcome(WelcomeScreen):
    instances: list[_RecordingWelcome] = []

    def __init__(self, *args, **kwargs) -> None:
        super().__init__(*args, **kwargs)
        self.stages: list[StartupStage] = []
        _RecordingWelcome.instances.append(self)

    def set_stage(self, stage: StartupStage, detail: str = "") -> None:
        self.stages.append(stage)
        super().set_stage(stage, detail)


@pytest.fixture
def recording_welcome(monkeypatch):
    import neurowatch.main as main_mod

    _RecordingWelcome.instances = []
    monkeypatch.setattr(main_mod, "WelcomeScreen", _RecordingWelcome)
    yield _RecordingWelcome
    for inst in _RecordingWelcome.instances:
        inst.close()


def test_start_shows_welcome_before_window_and_closes_after_show(
    app, qtbot, monkeypatch, recording_welcome
):
    import neurowatch.main as main_mod

    seen: dict[str, object] = {}

    class FakeWindow(QWidget):
        def __init__(self, progress=None) -> None:
            super().__init__()
            welcome = recording_welcome.instances[0]
            seen["welcome_visible_during_init"] = welcome.isVisible()
            progress(StartupStage.CONNECTING, "")
            progress(StartupStage.PREPARING_WORKSPACE, "")
            progress(StartupStage.LOADING_CAMERAS, "")
            seen["dismissed_during_init"] = welcome.is_dismissed

        def shutdown(self) -> None:
            pass

        def report_startup_error(self) -> None:
            pass

        def restore_windows(self) -> None:
            pass

    monkeypatch.setattr(main_mod, "MainWindow", FakeWindow)
    window = main_mod._start_main_window(app, "1.0.0-test")
    qtbot.addWidget(window)

    welcome = recording_welcome.instances[0]
    assert seen == {"welcome_visible_during_init": True, "dismissed_during_init": False}
    assert window.isVisible()
    assert welcome.is_dismissed and not welcome.isVisible()
    assert welcome.stages == [
        StartupStage.STARTING,
        StartupStage.CONNECTING,
        StartupStage.PREPARING_WORKSPACE,
        StartupStage.LOADING_CAMERAS,
        StartupStage.OPENING,
    ]
    window.close()


def test_backend_unavailable_closes_welcome_before_error_popup(app, monkeypatch, recording_welcome):
    import neurowatch.main as main_mod

    class FailingWindow:
        def __init__(self, progress=None) -> None:
            progress(StartupStage.CONNECTING, "")
            raise main_mod.BackendUnavailableError("unix:/nonexistent/test.sock")

    popups: list[tuple[bool, str]] = []

    def _critical(parent, title, text, *args, **kwargs):
        popups.append((recording_welcome.instances[0].isVisible(), title))
        assert "unix:/nonexistent/test.sock" in text

    monkeypatch.setattr(main_mod, "MainWindow", FailingWindow)
    monkeypatch.setattr(main_mod, "show_message", _critical)

    with pytest.raises(SystemExit) as exc:
        main_mod._start_main_window(app, "1.0.0-test")

    assert exc.value.code == 1
    assert popups == [(False, "Backend Not Running")]
    assert recording_welcome.instances[0].is_dismissed


def test_unexpected_startup_error_closes_welcome_and_propagates(
    app, monkeypatch, recording_welcome
):
    import neurowatch.main as main_mod

    class BrokenWindow:
        def __init__(self, progress=None) -> None:
            raise RuntimeError("boom")

    monkeypatch.setattr(main_mod, "MainWindow", BrokenWindow)
    with pytest.raises(RuntimeError, match="boom"):
        main_mod._start_main_window(app, "1.0.0-test")
    welcome = recording_welcome.instances[0]
    assert welcome.is_dismissed and not welcome.isVisible()


# ── Real MainWindow progress reporting ──────────────────────────────────


class _OnlineClient:
    _endpoint = "unix:/nonexistent/test-welcome.sock"

    def is_online(self) -> bool:
        return True

    def call(self, method, params=None):
        if method == "config.get":
            return {"wall": {"rows": 1, "cols": 1}}
        if method == "camera.list":
            return []
        return {}


def test_main_window_reports_real_stages_in_order(qtbot, monkeypatch):
    import neurowatch.main as main_mod

    monkeypatch.setattr(main_mod, "SyncIpcClient", _OnlineClient)
    reported: list[StartupStage] = []
    window = main_mod.MainWindow(progress=lambda stage, detail="": reported.append(stage))
    qtbot.addWidget(window)
    try:
        assert reported == [
            StartupStage.CONNECTING,
            StartupStage.PREPARING_WORKSPACE,
            StartupStage.LOADING_CAMERAS,
        ]
    finally:
        window.close()


def test_main_window_raises_backend_unavailable_after_retries(monkeypatch):
    import neurowatch.main as main_mod

    class OfflineClient(_OnlineClient):
        def is_online(self) -> bool:
            return False

    monkeypatch.setattr(main_mod, "SyncIpcClient", OfflineClient)
    timeline: list[str] = []
    from PySide6.QtCore import QThread

    monkeypatch.setattr(
        QThread, "msleep", staticmethod(lambda ms: timeline.append(f"wait {ms}"))
    )
    monkeypatch.setattr(
        main_mod,
        "_wait_responsively",
        lambda ms: timeline.append(f"wait {ms}"),
        raising=False,
    )
    popups: list[str] = []
    monkeypatch.setattr(main_mod, "show_message", lambda *a, **k: popups.append(a[1]))

    with pytest.raises(main_mod.BackendUnavailableError) as exc:
        main_mod.MainWindow(progress=lambda stage, detail="": timeline.append(detail))

    assert exc.value.endpoint == OfflineClient._endpoint
    assert popups == [], "the popup is main()'s job, after the welcome screen closes"
    # Each retry is announced before its wait, and the final failure has no wait.
    assert timeline == [
        "",
        "Backend not reachable — retrying (attempt 2 of 3)",
        "wait 1000",
        "Backend not reachable — retrying (attempt 3 of 3)",
        "wait 1000",
    ]


def test_retry_wait_keeps_event_loop_responsive(app):
    import neurowatch.main as main_mod

    ran: list[bool] = []
    QTimer.singleShot(0, lambda: ran.append(True))
    elapsed = QElapsedTimer()
    elapsed.start()

    main_mod._wait_responsively(60)

    assert ran == [True], "events must be processed during the retry wait"
    assert elapsed.elapsed() >= 55
    assert QThread.currentThread().loopLevel() == 0


# ── Load failure after connectivity succeeds ────────────────────────────


class _FailingLoadClient(_OnlineClient):
    failure: BaseException = IpcError(-32000, "config unavailable")
    fail = True

    def call(self, method, params=None):
        if _FailingLoadClient.fail and method in ("config.get", "camera.list"):
            raise _FailingLoadClient.failure
        return super().call(method, params)


def _record_popups(monkeypatch, main_mod, welcome_cls, windows):
    popups: list[tuple[bool, bool, str]] = []

    def _popup(parent, title, text, *args, **kwargs):
        welcome_visible = bool(welcome_cls.instances) and welcome_cls.instances[0].isVisible()
        window_visible = bool(windows) and windows[0].isVisible()
        popups.append((welcome_visible, window_visible, title))

    monkeypatch.setattr(main_mod, "show_message", _popup)
    return popups


@pytest.mark.parametrize(
    ("failure", "title"),
    [
        (IpcError(-32000, "config unavailable"), "Backend Error"),
        (BackendOfflineError("gone"), "Backend Offline"),
    ],
)
def test_startup_load_error_dismisses_welcome_before_popup(
    app, qtbot, monkeypatch, recording_welcome, failure, title
):
    import neurowatch.main as main_mod

    _FailingLoadClient.failure = failure
    _FailingLoadClient.fail = True
    monkeypatch.setattr(main_mod, "SyncIpcClient", _FailingLoadClient)
    windows: list[QWidget] = []
    real_init = main_mod.MainWindow.__init__

    def _tracking_init(self, *args, **kwargs):
        windows.append(self)
        real_init(self, *args, **kwargs)

    monkeypatch.setattr(main_mod.MainWindow, "__init__", _tracking_init)
    popups = _record_popups(monkeypatch, main_mod, recording_welcome, windows)

    window = main_mod._start_main_window(app, "1.0.0-test")
    qtbot.addWidget(window)
    try:
        welcome = recording_welcome.instances[0]
        # Popup only after startup finished: welcome gone, main window up.
        assert popups == [(False, True, title)]
        assert welcome.is_dismissed
        assert welcome.stages[-1] is StartupStage.OPENING
    finally:
        window.close()


def test_refresh_error_after_startup_still_shows_popup_immediately(
    app, qtbot, monkeypatch, recording_welcome
):
    import neurowatch.main as main_mod

    _FailingLoadClient.failure = IpcError(-32000, "config unavailable")
    _FailingLoadClient.fail = False
    monkeypatch.setattr(main_mod, "SyncIpcClient", _FailingLoadClient)
    windows: list[QWidget] = []
    popups = _record_popups(monkeypatch, main_mod, recording_welcome, windows)

    window = main_mod._start_main_window(app, "1.0.0-test")
    qtbot.addWidget(window)
    windows.append(window)
    try:
        assert popups == []
        _FailingLoadClient.fail = True
        window._load_from_backend()
        assert popups == [(False, True, "Backend Error")]
    finally:
        _FailingLoadClient.fail = False
        window.close()


# ── First paint ──────────────────────────────────────────────────────────


class _NoPump:
    """Stands in for QApplication so only forced (synchronous) paints count."""

    @staticmethod
    def processEvents(*_args, **_kwargs) -> None:  # noqa: N802
        return None


def _paint_log(widget: QWidget, label_name: str = "welcomeStage") -> list[str]:
    """Record a label's text each time that label itself is painted."""
    log: list[str] = []
    label = widget.findChild(QLabel, label_name)

    class _Spy(QObject):
        def eventFilter(self, watched, event):  # noqa: N802
            if event.type() == QEvent.Type.Paint:
                log.append(label.text())
            return False

    spy = _Spy(label)
    label.installEventFilter(spy)
    return log


def test_set_stage_forces_synchronous_repaint(welcome, monkeypatch):
    import neurowatch.widgets.welcome_screen as ws

    welcome.show()
    welcome.set_stage(StartupStage.CONNECTING)
    log = _paint_log(welcome, "welcomeDetail")
    monkeypatch.setattr(ws, "QApplication", _NoPump)

    # Same stage, new detail (the retry case): progress does not move, so
    # nothing else incidentally flushes the pending label update.
    welcome.set_stage(StartupStage.CONNECTING, "retrying")

    assert "retrying" in log


def test_present_exposes_and_paints_before_returning(welcome):
    ran: list[bool] = []
    QTimer.singleShot(0, lambda: ran.append(True))
    log = _paint_log(welcome)

    welcome.present()

    assert welcome.isVisible()
    assert welcome.windowHandle() is not None and welcome.windowHandle().isExposed()
    assert ran == [True], "present must give the window system a chance to expose"
    assert log, "present must paint the splash before returning"


def test_welcome_is_painted_with_first_stage_before_main_window_builds(
    app, qtbot, monkeypatch, recording_welcome
):
    import neurowatch.main as main_mod
    import neurowatch.widgets.welcome_screen as ws

    monkeypatch.setattr(ws, "QApplication", _NoPump)
    logs: list[list[str]] = []
    real_init = recording_welcome.__init__

    def _init(self, *args, **kwargs):
        real_init(self, *args, **kwargs)
        logs.append(_paint_log(self))

    monkeypatch.setattr(recording_welcome, "__init__", _init)
    seen: dict[str, list[str]] = {}

    class FakeWindow(QWidget):
        def __init__(self, progress=None) -> None:
            super().__init__()
            seen["paints"] = list(logs[0])

        def shutdown(self) -> None:
            pass

        def report_startup_error(self) -> None:
            pass

        def restore_windows(self) -> None:
            pass

    monkeypatch.setattr(main_mod, "MainWindow", FakeWindow)
    window = main_mod._start_main_window(app, "1.0.0-test")
    qtbot.addWidget(window)
    window.close()
    assert "Starting NeuroWatch" in seen["paints"]


# ── Shutdown signals ─────────────────────────────────────────────────────


def test_signal_during_startup_quits_running_app_not_pre_exec_pump(app, qtbot, monkeypatch):
    import signal

    import neurowatch.main as main_mod

    quits: list[int] = []
    loop = QEventLoop()
    monkeypatch.setattr(app, "closeAllWindows", lambda: None)

    def _quit() -> None:
        quits.append(QThread.currentThread().loopLevel())
        loop.quit()

    monkeypatch.setattr(app, "quit", _quit)

    with main_mod._shutdown_signals(app):
        signal.raise_signal(signal.SIGINT)
        # Startup pumping (processEvents, no running loop) must not consume it.
        pumped = QElapsedTimer()
        pumped.start()
        while pumped.elapsed() < 350:
            QApplication.processEvents()
            QThread.msleep(10)
        assert quits == []
        QTimer.singleShot(2000, loop.quit)  # safety net for a lost signal
        loop.exec()

    assert quits and quits[0] >= 1, "shutdown must be delivered to the running loop"


@pytest.mark.parametrize("signame", ["SIGINT", "SIGTERM"])
def test_signal_during_pre_exec_startup_popup_terminates_cleanly(signame, tmp_path):
    import os
    import subprocess
    import sys
    from pathlib import Path

    probe = Path(__file__).parents[1] / "helpers" / "ui_startup_signal_probe.py"
    env = {
        **os.environ,
        "QT_QPA_PLATFORM": "offscreen",
        "HOME": str(tmp_path),
        "PYTHONPATH": os.pathsep.join(sys.path),
    }
    env.pop("NW_IPC_ENDPOINT", None)
    try:
        result = subprocess.run(
            [sys.executable, str(probe), signame],
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except subprocess.TimeoutExpired as exc:
        pytest.fail(f"startup hung after {signame} during popup: {exc.stdout!r}")

    lines = result.stdout.splitlines()
    assert result.returncode == 0, result.stdout + result.stderr
    # The signal dismisses the popup, is not lost, and startup ends without app.exec().
    assert lines.index("POPUP OPEN") < lines.index("POPUP RETURNED")
    assert "APP EXEC" not in lines
    assert "WINDOW SHUTDOWN" in lines
    assert lines[-1] == "EXIT 0"
    assert "KeyboardInterrupt" not in result.stderr
    assert "QThread: Destroyed" not in result.stderr


@pytest.mark.parametrize(
    ("keep", "autostart", "tray"),
    [(None, False, []), ("1", True, ["tray"])],
    ids=["dev-service-follows-the-app", "NW_DEV_KEEP_SERVICE"],
)
def test_development_service_registers_nothing_and_starts_no_tray(
    app, qtbot, monkeypatch, recording_welcome, keep, autostart, tray
):
    import neurowatch.app_launch as app_launch
    import neurowatch.main as main_mod

    class FakeWindow(QWidget):
        def __init__(self, progress=None) -> None:
            super().__init__()

        def shutdown(self) -> None:
            pass

        def report_startup_error(self) -> None:
            pass

        def restore_windows(self) -> None:
            pass

    calls: list[object] = []
    if keep is None:
        monkeypatch.delenv("NW_DEV_KEEP_SERVICE", raising=False)
    else:
        monkeypatch.setenv("NW_DEV_KEEP_SERVICE", keep)
    monkeypatch.setattr(app_launch, "dev_mode", lambda: True)
    monkeypatch.setattr(main_mod, "MainWindow", FakeWindow)
    monkeypatch.setattr(
        main_mod,
        "ensure_service",
        lambda _progress, *, autostart: (calls.append(autostart), (True, ""))[1],
    )
    monkeypatch.setattr(main_mod, "start_tray", lambda: calls.append("tray"))

    window = main_mod._start_main_window(app, "1.0.0-test", main_mod.LaunchOptions())
    qtbot.addWidget(window)
    assert calls == [autostart, *tray]
    assert main_mod.service_follows_ui() is (keep is None)
