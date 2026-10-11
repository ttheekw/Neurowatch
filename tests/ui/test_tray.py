"""The tray icon: status on hover and in the menu, service actions, launching."""

from __future__ import annotations

from typing import Any

import pytest
from PySide6.QtWidgets import QApplication

from neurowatch import app_launch
from neurowatch.tray import (
    BUSY,
    CRITICAL,
    RUNNING,
    STOPPED,
    WARNING,
    Problem,
    ServiceStatus,
    TrayApp,
    find_problems,
    format_uptime,
    read_status,
    tray_icon,
)


def _backend(**overrides: Any):
    answers: dict[str, Any] = {
        "system.status": {
            "status": "running",
            "cameras": 3,
            "site_name": "Main Gate",
            "video_acceleration": {"label": "VA-API"},
            "version": "0.1.0",
            "uptime_sec": 7500,
        },
        "camera.status_all": {"a": "streaming", "b": "recording_ready", "c": "offline"},
        "recording.status_all": {"a": "recording", "b": "recording", "c": "idle"},
    }
    answers.update(overrides)

    def request(method: str, **_kwargs: Any) -> Any:
        return answers.get(method)

    return request


def test_status_from_the_backend() -> None:
    status = read_status(_backend())
    assert status.running and status.state == WARNING, "one camera offline: amber"
    assert status.alert, "and the red badge"
    assert (status.cameras, status.online, status.recording) == (3, 2, 2)
    assert status.lines() == [
        "Running · Main Gate",
        "Cameras 2/3 online · 2 recording",
        "VA-API",
        "⚠ 1 camera offline",
    ]
    assert status.tooltip() == "NeuroWatch\n" + "\n".join(status.lines())
    # The menu adds the uptime; the tooltip leaves out what changes by itself.
    assert status.menu_lines() == [
        "Running · Main Gate · up 2 h 5 min",
        "Cameras 2/3 online · 2 recording",
        "VA-API",
        "⚠ 1 camera offline",
    ]


def test_extensions_add_a_detail_and_problems(monkeypatch) -> None:
    from types import SimpleNamespace

    from neurowatch import extensions

    def tray_status(request: Any, endpoint: str) -> extensions.ExtensionStatus:
        state = request("example.status", endpoint=endpoint)
        return extensions.ExtensionStatus(
            detail=f"Example: {state}", problems=(("Example down", "Example is down"),)
        )

    monkeypatch.setattr(extensions, "load", lambda: (SimpleNamespace(tray_status=tray_status),))

    status = read_status(_backend(**{"example.status": "on"}), endpoint="unix:/x")

    assert status.lines()[2] == "Example: on · VA-API"
    assert [problem.label for problem in status.problems] == ["1 camera offline", "Example down"]


def test_problems_that_raise_the_badge() -> None:
    def problems(**kw: Any) -> list[str]:
        args: dict[str, Any] = {
            "cameras": 4,
            "online": 4,
            "recording": {},
            "performance": None,
        }
        args.update(kw)
        return [p.label for p in find_problems(**args)]

    assert problems() == []
    assert problems(online=1) == ["3 cameras offline"]
    assert problems(recording={"a": "failed", "b": "recording"}) == ["Recording failed on 1 camera"]
    warning = {"kind": "ram", "severity": "warning", "explanation": "memory high at 87%"}
    critical = {"kind": "cpu", "severity": "critical", "explanation": "CPU at 99%"}
    assert problems(performance={"bottleneck": warning}) == ["Memory running low"]
    assert problems(performance={"bottleneck": critical}) == ["High CPU load (critical)"]
    ok = {"kind": "none", "severity": "ok", "explanation": "all good"}
    assert problems(performance={"bottleneck": ok}) == []
    found = find_problems(
        cameras=1, online=1, recording={}, performance={"bottleneck": warning}
    )
    assert found == (Problem("Memory running low", "Memory running low: memory high at 87%"),)


def test_the_tooltip_names_two_problems_and_the_menu_all() -> None:
    status = ServiceStatus(
        running=True,
        cameras=2,
        online=0,
        problems=(Problem("2 cameras offline"), Problem("Example down"), Problem("GPU X")),
    )
    assert status.lines()[-1] == "⚠ 2 cameras offline · Example down · 1 more"
    assert status.menu_lines()[-3:] == [
        "⚠ 2 cameras offline",
        "⚠ Example down",
        "⚠ GPU X",
    ]
    stopped = ServiceStatus(running=False, problems=(Problem("x"),))
    assert not stopped.alert, "stopped has its own red dot"


def test_a_stopped_service_reads_as_stopped() -> None:
    status = read_status(lambda *_a, **_k: None)
    assert not status.running and status.state == STOPPED
    assert status.lines() == ["Stopped — cameras are not recording"]
    all_online = read_status(
        _backend(**{"camera.status_all": {"a": "streaming", "b": "streaming", "c": "streaming"}})
    )
    assert all_online.state == RUNNING and not all_online.alert
    # Running, but no camera can record: the dot is red, not green.
    all_offline = read_status(
        _backend(**{"camera.status_all": {"a": "offline", "b": "offline", "c": "offline"}})
    )
    assert all_offline.running and all_offline.state == CRITICAL and all_offline.alert


def test_uptime_reads_naturally() -> None:
    assert format_uptime(59) == "0 min"
    assert format_uptime(3 * 3600 + 120) == "3 h 2 min"
    assert format_uptime(2 * 86400 + 5 * 3600) == "2 d 5 h"


def test_icons_differ_by_state(qapp) -> None:
    states = (RUNNING, WARNING, CRITICAL, STOPPED)
    images = {state: tray_icon(state).pixmap(64, 64).toImage() for state in states}
    assert images[RUNNING] != images[WARNING] != images[CRITICAL]
    assert images[RUNNING].pixelColor(48, 48).green() > 150, "green dot"
    assert images[WARNING].pixelColor(48, 48).red() > 200, "amber dot"
    assert not tray_icon(BUSY).isNull()
    badge = tray_icon(RUNNING, alert=True).pixmap(64, 64).toImage()
    assert badge != images[RUNNING]
    corner = badge.pixelColor(51, 13)  # the badge, top right
    assert corner.red() > 200 and corner.green() < 100
    assert images[RUNNING].pixelColor(51, 13) != corner


class FakeController:
    def __init__(self) -> None:
        self.calls: list[str] = []
        self.endpoint = "unix:/tmp/x.sock"

    def ensure_running(self, *, autostart: bool, mode: str | None = None) -> bool:
        self.calls.append(f"start(autostart={autostart})" + (f", {mode}" if mode else ""))
        return True

    def stop(self) -> bool:
        self.calls.append("stop")
        return True

    def restart(self) -> bool:
        self.calls.append("restart")
        return True


@pytest.fixture
def tray(qapp):
    controller = FakeController()
    opened: list[bool] = []
    app = TrayApp(
        controller=controller,  # type: ignore[arg-type]
        read=lambda: read_status(_backend()),
        launch_ui=lambda: opened.append(True),
        ensure_service=False,
    )
    app.controller = controller  # type: ignore[attr-defined]
    app.opened = opened  # type: ignore[attr-defined]
    yield app
    app.shutdown()


def test_hover_and_menu_show_the_status(tray) -> None:
    tray.show_status(read_status(_backend()))
    tooltip = tray.icon.toolTip()
    assert "Cameras 2/3 online · 2 recording" in tooltip
    assert tooltip.splitlines()[-1] == "⚠ 1 camera offline"
    assert len(tooltip.splitlines()) == 5, "NeuroWatch, three short lines, the problems"
    texts = [a.text() for a in tray.menu.actions()]
    assert texts[0] == "Running · Main Gate · up 2 h 5 min"
    assert "Open NeuroWatch" in texts and "Restart service" in texts and "Exit" in texts
    status_items = [a for a in tray.menu.actions() if a.text().startswith("Cameras")]
    assert status_items and not status_items[0].isEnabled(), "status lines are not clickable"
    assert tray.stop_action.isVisible() and not tray.start_action.isVisible()

    tray.show_status(ServiceStatus(running=False))
    assert "Stopped" in tray.icon.toolTip()
    assert tray.start_action.isVisible() and not tray.stop_action.isVisible()
    assert not tray.restart_action.isEnabled()


def test_polling_an_unchanged_status_does_not_touch_the_tooltip(tray, monkeypatch) -> None:
    """A tooltip re-set while it is shown redraws: it flickered every poll."""
    status = read_status(_backend())
    tray.show_status(status)
    calls: list[str] = []
    monkeypatch.setattr(tray.icon, "setToolTip", lambda text: calls.append("tooltip"))
    monkeypatch.setattr(tray.icon, "setIcon", lambda icon: calls.append("icon"))
    actions = list(tray._status_actions)
    for _ in range(3):
        tray.show_status(read_status(_backend()))
    assert calls == [], "nothing changed, nothing re-set"
    assert tray._status_actions == actions, "menu lines kept, not rebuilt"

    uptime_only = read_status(
        _backend(**{"system.status": {**_backend()("system.status"), "uptime_sec": 9000}})
    )
    tray.show_status(uptime_only)
    assert calls == [], "the uptime is not in the tooltip"
    assert tray._status_actions[0].text().endswith("up 2 h 30 min")

    tray.show_status(ServiceStatus(running=False))
    assert calls == ["icon", "tooltip"]


def test_the_badge_follows_problems_without_redrawing_for_percentages(tray, monkeypatch) -> None:
    def with_memory(percent: int) -> ServiceStatus:
        return read_status(
            _backend(
                **{
                    "camera.status_all": {"a": "streaming", "b": "streaming", "c": "streaming"},
                    "stats.get_performance": {
                        "bottleneck": {
                            "kind": "ram",
                            "severity": "warning",
                            "explanation": f"memory high at {percent}%",
                        }
                    },
                }
            )
        )

    icons: list[tuple[str, bool]] = []  # (dot, badge)
    original = tray_icon

    def recorded(state: str, alert: bool = False, size: int = 64):
        icons.append((state, alert))
        return original(state, alert, size)

    monkeypatch.setattr("neurowatch.tray.tray_icon", recorded)
    tray.show_status(with_memory(87))
    assert icons == [("warning", True)]
    assert tray.icon.toolTip().endswith("⚠ Memory running low")
    assert tray._status_actions[-1].text() == "⚠ Memory running low: memory high at 87%"

    tooltips: list[str] = []
    monkeypatch.setattr(tray.icon, "setToolTip", tooltips.append)
    tray.show_status(with_memory(91))
    assert tooltips == [] and icons == [("warning", True)], "same problem: nothing redrawn"
    assert tray._status_actions[-1].text().endswith("91%"), "the menu has the details"

    healthy = read_status(
        _backend(**{"camera.status_all": {"a": "streaming", "b": "streaming", "c": "streaming"}})
    )
    tray.show_status(healthy)
    assert icons[-1] == ("running", False), "problem gone: badge gone"


def test_stop_asks_first_in_the_app_style(tray, monkeypatch, qtbot) -> None:
    from neurowatch.widgets.centered_prompt import CenteredPrompt

    tray.show_status(read_status(_backend()))
    asked: list[str] = []

    def choose(prompt: CenteredPrompt) -> str:
        asked.append(prompt.windowTitle())
        return "stop"

    monkeypatch.setattr(CenteredPrompt, "choose", choose)
    tray.stop_action.trigger()
    qtbot.waitUntil(lambda: tray._busy is None, timeout=3000)
    assert asked == ["Stop NeuroWatch"] and tray.controller.calls == ["stop"]


def test_menu_actions_drive_the_service(tray, qtbot) -> None:
    tray.show_status(read_status(_backend()))
    tray.run_action("restart")
    assert "Restarting the service" in tray.icon.toolTip()
    assert not tray.restart_action.isEnabled(), "one action at a time"
    qtbot.waitUntil(lambda: tray._busy is None, timeout=3000)
    assert tray.controller.calls == ["restart"]

    tray.open_action.trigger()
    assert tray.opened == [True]


def test_opening_from_the_tray_starts_the_service(qapp, qtbot) -> None:
    controller = FakeController()
    app = TrayApp(
        controller=controller,  # type: ignore[arg-type]
        read=lambda: ServiceStatus(running=False),
        launch_ui=lambda: None,
    )
    try:
        qtbot.waitUntil(lambda: app._busy is None, timeout=3000)
        assert controller.calls == ["start(autostart=False)"], "at login: start, register nothing"
    finally:
        app.shutdown()


# ---------------------------------------------------------------------------
# Opening the app
# ---------------------------------------------------------------------------


def test_launch_arguments(monkeypatch) -> None:
    parse = app_launch.parse_launch_args
    monkeypatch.delenv("NW_SERVICE")
    assert parse(["neurowatch-ui"]) == app_launch.LaunchOptions()
    assert parse(["neurowatch-ui"]).manage_service
    monkeypatch.setenv("NW_SERVICE", "external")
    assert not parse(["neurowatch-ui"]).manage_service, "something else runs the backend"
    assert not parse(["neurowatch-ui", "--no-autostart"]).autostart
    assert parse(["neurowatch-ui", "--tray"]).mode == "tray"
    headless = parse(["neurowatch-ui", "--headless", "--foreground"])
    assert headless.mode == "headless" and headless.rest == ("--foreground",)
    service = parse(["neurowatch-ui", "service", "stop"])
    assert service.mode == "service" and service.rest == ("stop",)
    # An extension's command goes to the headless commands as it is.
    from types import SimpleNamespace

    from neurowatch import extensions

    assert parse(["neurowatch-ui", "example", "go"]).mode == "ui", "no such command"
    monkeypatch.setattr(extensions, "load", lambda: (SimpleNamespace(commands=("example",)),))
    command = parse(["neurowatch-ui", "example", "go", "--name", "Lab"])
    assert command.mode == "command" and command.rest == ("example", "go", "--name", "Lab")


def test_opening_the_app_again_brings_the_window_forward(qapp, qtbot, tmp_path) -> None:
    name = str(tmp_path / "ui.sock")
    assert not app_launch.activate_running_instance(name), "nothing open yet"
    instance = app_launch.SingleInstance(name)
    assert instance.listen()
    try:
        with qtbot.waitSignal(instance.activate_requested, timeout=3000):
            assert app_launch.activate_running_instance(name)
            QApplication.processEvents()
    finally:
        instance.close()


def test_ensure_service_waits_off_the_gui_thread(qapp, monkeypatch) -> None:
    monkeypatch.setattr(app_launch.probe, "backend_status", lambda _endpoint, timeout=1.0: None)
    monkeypatch.setattr(app_launch, "dev_mode", lambda: False)
    shown: list[str] = []
    controller = FakeController()
    ok, error = app_launch.ensure_service(
        shown.append,
        autostart=True,
        controller=controller,  # type: ignore[arg-type]
    )
    assert ok and not error
    assert controller.calls == ["start(autostart=True), desktop"], "the app asks for desktop"
    assert shown == ["Starting the NeuroWatch background service"]


# ---------------------------------------------------------------------------
# Prompts from the tray: the app's dialog style, centred
# ---------------------------------------------------------------------------


def test_prompt_returns_the_choice_or_none(qapp, qtbot) -> None:
    from PySide6.QtCore import QTimer

    from neurowatch.widgets.centered_prompt import CenteredPrompt

    prompt = CenteredPrompt("Stop NeuroWatch", "Stop the service?", "Cameras stop recording.")
    qtbot.addWidget(prompt)
    prompt.add_button("cancel", "Cancel")
    stop = prompt.add_button("stop", "Stop service", "danger")
    assert stop.property("variant") == "danger"
    assert prompt.property("nwThemed"), "the app's dialog theme"
    QTimer.singleShot(0, stop.click)
    assert prompt.choose() == "stop"
    QTimer.singleShot(0, prompt.reject)  # Escape / the header's close button
    assert prompt.choose() is None


def test_prompt_text_wraps_without_clipping(qapp, qtbot) -> None:
    from neurowatch.widgets.centered_prompt import CenteredPrompt

    long = "The service can keep recording in the background, or stop with the icon. " * 3
    prompt = CenteredPrompt("Title", "Short", long)
    qtbot.addWidget(prompt)
    label = prompt.detail_label
    assert label.height() >= label.heightForWidth(label.width()) > label.fontMetrics().height()


def test_prompt_is_centred_on_the_screen(qapp, qtbot, monkeypatch) -> None:
    from PySide6.QtCore import Qt

    from neurowatch.widgets import centered_prompt
    from neurowatch.widgets.centered_prompt import CenteredPrompt

    prompt = CenteredPrompt("Title", "Text")
    qtbot.addWidget(prompt)
    prompt.add_button("ok", "OK", "primary")
    prompt._present()
    screen = prompt.screen().availableGeometry()
    centre = prompt.frameGeometry().center()
    assert abs(centre.x() - screen.center().x()) <= 1
    assert abs(centre.y() - screen.center().y()) <= 1

    # Wayland ignores window positions: the prompt fills the screen, the card
    # is centred in it, and only the card takes clicks.
    monkeypatch.setattr(centered_prompt, "positions_windows", lambda: False)
    wayland = CenteredPrompt("Title", "Text")
    qtbot.addWidget(wayland)
    wayland._present()
    assert wayland.windowState() & Qt.WindowState.WindowMaximized
    assert wayland.testAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
    wayland.resize(screen.size())
    wayland.show()
    qtbot.waitExposed(wayland)
    card = wayland._shell.geometry()
    assert abs(card.center().x() - wayland.rect().center().x()) <= 1
    assert wayland.mask().boundingRect() == card
    wayland.close()


def test_closing_the_app_stops_a_development_service(monkeypatch) -> None:
    monkeypatch.delenv("NW_DEV_KEEP_SERVICE", raising=False)
    monkeypatch.setattr(app_launch, "dev_mode", lambda: True)
    assert app_launch.service_follows_ui()
    monkeypatch.setattr(app_launch, "dev_mode", lambda: False)
    assert not app_launch.service_follows_ui(), "installed builds keep the service"

    controller = FakeController()
    assert app_launch.stop_service(controller)  # type: ignore[arg-type]
    assert controller.calls == ["stop"]
