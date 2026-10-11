"""Optional extensions (neurowatch.extensions): loading, failures, and the
places in the app they can add to."""

from __future__ import annotations

import sys
import types

import pytest

from neurowatch import extensions


@pytest.fixture
def real_loader(monkeypatch):
    """The real loader (conftest turns extensions off), with a fresh cache."""
    monkeypatch.setattr(extensions, "load", extensions._discover)
    extensions.reset()
    yield
    extensions.reset()


def test_modules_named_in_the_environment_are_loaded(monkeypatch, real_loader):
    module = types.ModuleType("nw_example_extension")
    module.commands = ("example",)
    monkeypatch.setitem(sys.modules, "nw_example_extension", module)
    monkeypatch.setenv(extensions.ENV, "nw_example_extension, missing_extension_module")

    assert module in extensions.load()
    assert extensions.commands() == {"example"}


def test_a_failing_hook_is_logged_and_skipped(monkeypatch, caplog):
    def broken() -> str:
        raise RuntimeError("broken")

    ok = types.SimpleNamespace(status=lambda: "fine")
    monkeypatch.setattr(extensions, "load", lambda: (types.SimpleNamespace(status=broken), ok))

    assert extensions.call_each("status") == ["fine"]
    assert "failed in status" in caplog.text


def test_an_extension_indicator_sits_before_the_version(qtbot):
    from PySide6.QtWidgets import QLabel

    from neurowatch.widgets.status_bar_widget import StatusBarWidget

    bar = StatusBarWidget()
    qtbot.addWidget(bar)
    light = QLabel("● Example")
    bar.add_indicator(light)

    layout = bar.layout()
    assert (
        layout.indexOf(bar._backend_lbl) < layout.indexOf(light) < layout.indexOf(bar._version_lbl)
    )
