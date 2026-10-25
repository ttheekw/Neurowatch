"""Bottom-bar cloud indicator: red offline, green connected, blinking green in use."""

from __future__ import annotations

import pytest

from neurowatch.cloud.cloud_link import CloudIndicator, cloud_indicator
from neurowatch.cloud.desktop import CloudIndicatorLabel

LINKED = {"phase": "linked", "cloud_reachable": True, "media_publish": {"sessions": {}}}


@pytest.mark.parametrize(
    ("status", "state"),
    [
        (None, "offline"),  # backend unreachable or no cloud support
        ({"phase": "unlinked", "cloud_reachable": False}, "offline"),
        ({**LINKED, "cloud_reachable": False}, "offline"),
        (LINKED, "connected"),
        ({**LINKED, "media_publish": {"sessions": {"s1": "stopped", "s2": "failed"}}}, "connected"),
        ({**LINKED, "media_publish": {"sessions": {"s1": "publishing"}}}, "active"),
        # Publishing while the cloud is unreachable is not "in use and connected".
        (
            {
                **LINKED,
                "cloud_reachable": False,
                "media_publish": {"sessions": {"s": "publishing"}},
            },
            "offline",
        ),
    ],
)
def test_indicator_state_follows_link_reachability_and_publishing(status, state):
    assert cloud_indicator(status).state == state


def test_indicator_tooltip_says_why_it_is_red():
    assert "not linked" in cloud_indicator({"phase": "unlinked"}).tooltip.lower()
    assert "can't reach" in cloud_indicator({**LINKED, "cloud_reachable": False}).tooltip.lower()
    active = cloud_indicator(
        {**LINKED, "media_publish": {"sessions": {"a": "publishing", "b": "publishing"}}}
    )
    assert "2" in active.tooltip


def _color(widget: CloudIndicatorLabel) -> str:
    return widget.styleSheet()


def test_status_bar_shows_red_green_and_blinking_green(qtbot):
    bar = CloudIndicatorLabel()
    qtbot.addWidget(bar)

    bar.set_indicator(CloudIndicator("offline", "Not linked"))
    assert "#ef4444" in _color(bar) and not bar._blink.isActive()
    assert bar.toolTip() == "Not linked\nClick for Settings → Cloud"

    bar.set_indicator(CloudIndicator("connected", "Connected"))
    assert "#22c55e" in _color(bar) and not bar._blink.isActive()

    bar.set_indicator(CloudIndicator("active", "Streaming"))
    assert bar._blink.isActive()
    colours = set()
    for _ in range(4):
        bar._blink.timeout.emit()
        colours.add(_color(bar))
    assert len(colours) == 2, "alternates between bright and dim green"

    bar.set_indicator(CloudIndicator("connected", "Connected"))
    assert not bar._blink.isActive() and "#22c55e" in _color(bar)


def test_repeating_the_same_state_does_not_restart_the_blink(qtbot):
    bar = CloudIndicatorLabel()
    qtbot.addWidget(bar)
    bar.set_indicator(CloudIndicator("active", "Streaming"))
    bar._blink.timeout.emit()
    dimmed = _color(bar)
    bar.set_indicator(CloudIndicator("active", "Streaming"))  # next 2 s poll
    assert _color(bar) == dimmed


@pytest.mark.parametrize("cloud", [LINKED, None], ids=["cloud-status", "no-cloud-support"])
def test_the_cloud_poll_reports_the_cloud_status(qtbot, monkeypatch, cloud):
    from neurowatch.cloud import desktop

    class _FakeClient:
        def __init__(self, endpoint=None):
            pass

        def call(self, method, params=None):
            assert method == "cloud.status"
            if cloud is None:
                raise RuntimeError("method not found")
            return cloud

    monkeypatch.setattr(desktop, "SyncIpcClient", _FakeClient)
    worker = desktop._CloudPoll("unix:/unused")
    with qtbot.waitSignal(worker.polled, timeout=3000) as blocker:
        worker.start()
    worker.requestInterruption()
    worker.wait()
    assert blocker.args == [cloud]


def test_install_adds_the_light_that_opens_settings_and_stops_with_the_window(qtbot, monkeypatch):
    from PySide6.QtCore import Qt
    from PySide6.QtWidgets import QWidget

    from neurowatch.cloud import desktop

    monkeypatch.setattr(desktop._CloudPoll, "run", lambda self: None)

    class Window(QWidget):
        ipc_endpoint = "unix:/unused"

        def __init__(self) -> None:
            super().__init__()
            self.indicators: list[QWidget] = []
            self.on_shutdown: list = []

        def add_status_indicator(self, widget: QWidget) -> None:
            self.indicators.append(widget)

        def add_shutdown_callback(self, callback) -> None:
            self.on_shutdown.append(callback)

    window = Window()
    qtbot.addWidget(window)
    opened: list[str] = []
    desktop.install(window, lambda: opened.append("settings"))

    [light] = window.indicators
    assert isinstance(light, CloudIndicatorLabel)
    qtbot.mouseClick(light, Qt.MouseButton.LeftButton)
    assert opened == ["settings"], "the light opens Settings → Cloud"
    for callback in window.on_shutdown:
        callback()  # joins the poll thread


def test_the_cloud_light_is_a_status_bar_indicator_level_with_its_text(qtbot):
    """A ● glyph sat below the words; the bar's indicator draws its mark
    level with the text, like Backend's."""
    from neurowatch.widgets.status_bar_widget import StatusIndicator

    light = CloudIndicatorLabel()
    qtbot.addWidget(light)
    assert isinstance(light, StatusIndicator)
    assert (light.text(), light.mark()) == ("Cloud", "dot")


def test_the_status_bar_tooltip_carries_the_clock_warning():
    from neurowatch.cloud.desktop import desktop_indicator

    warning = "This computer's clock is 5 h 30 min ahead of NeuroWatch Cloud."
    indicator = desktop_indicator({**LINKED, "clock_warning": warning})
    assert indicator.state == "connected"
    assert indicator.tooltip == f"NeuroWatch Cloud: connected\n{warning}"
    assert desktop_indicator(LINKED) == cloud_indicator(LINKED)
