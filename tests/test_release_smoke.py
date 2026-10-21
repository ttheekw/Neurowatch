"""Release validation must also check plugins unused by the offscreen UI."""

from __future__ import annotations

import importlib.util
import os
import subprocess
from pathlib import Path

import pytest


@pytest.fixture
def smoke_module():
    path = Path(__file__).resolve().parents[1] / "scripts/smoke_release.py"
    spec = importlib.util.spec_from_file_location("smoke_release", path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.fixture
def qt_bundle(tmp_path: Path) -> Path:
    ui = tmp_path / "app/neurowatch-ui"
    plugins = ui / "_internal/PySide6/Qt/plugins"
    for name in (
        "platforms/libqxcb.so",
        "xcbglintegrations/libqxcb-egl-integration.so",
        "xcbglintegrations/libqxcb-glx-integration.so",
    ):
        plugin = plugins / name
        plugin.parent.mkdir(parents=True, exist_ok=True)
        plugin.write_bytes(b"test plugin")
    return ui


def test_xcb_check_covers_platform_and_gl_plugins_using_the_bundled_qt(
    smoke_module, qt_bundle: Path, monkeypatch
) -> None:
    seen: set[str] = set()
    original = {
        "PATH": "/usr/bin",
        "QT_QPA_PLATFORM": "offscreen",
        "LD_LIBRARY_PATH": "/old/qt",
        "LC_ALL": "ja_JP.UTF-8",
    }

    def resolved(command, *, env, **kwargs):
        assert command[0] == "ldd"
        seen.add(Path(command[1]).name)
        assert env["LD_LIBRARY_PATH"].split(os.pathsep) == [
            str(qt_bundle / "_internal"),
            str(qt_bundle / "_internal/PySide6/Qt/lib"),
        ]
        assert env["QT_QPA_PLATFORM"] == "offscreen"
        assert env["LC_ALL"] == "C"
        assert 0 < kwargs["timeout"] <= 30
        return subprocess.CompletedProcess(command, 0, "libxcb.so.1 => /usr/lib/libxcb.so.1\n", "")

    monkeypatch.setattr(smoke_module.subprocess, "run", resolved)
    smoke_module.check_qt_x11_dependencies(qt_bundle, original)
    assert seen == {"libqxcb.so", "libqxcb-egl-integration.so", "libqxcb-glx-integration.so"}
    assert original["LD_LIBRARY_PATH"] == "/old/qt"
    assert original["LC_ALL"] == "ja_JP.UTF-8"


@pytest.mark.parametrize(
    "broken", ["libqxcb.so", "libqxcb-egl-integration.so", "libqxcb-glx-integration.so"]
)
def test_missing_xcb_library_is_fatal_even_when_ldd_returns_success(
    smoke_module, qt_bundle: Path, monkeypatch, broken: str
) -> None:
    def missing(command, **kwargs):
        output = "libxcb-icccm.so.4 => not found\n" if Path(command[1]).name == broken else ""
        return subprocess.CompletedProcess(command, 0, output, "")

    monkeypatch.setattr(smoke_module.subprocess, "run", missing)
    with pytest.raises(RuntimeError, match=r"libxcb-icccm\.so\.4 => not found"):
        smoke_module.check_qt_x11_dependencies(qt_bundle, {})


def test_ldd_failure_is_not_treated_as_a_dependency_pass(smoke_module, qt_bundle, monkeypatch):
    monkeypatch.setattr(
        smoke_module.subprocess,
        "run",
        lambda command, **kwargs: subprocess.CompletedProcess(command, 1, "", "invalid ELF"),
    )
    with pytest.raises(RuntimeError, match="invalid ELF"):
        smoke_module.check_qt_x11_dependencies(qt_bundle, {})


def test_missing_xcb_plugin_is_not_a_successful_offscreen_check(
    smoke_module, tmp_path, monkeypatch
):
    def unexpected(*args, **kwargs):
        pytest.fail("no dependency process should start without a plugin")

    monkeypatch.setattr(smoke_module.subprocess, "run", unexpected)
    with pytest.raises(RuntimeError, match="platform plugin is missing"):
        smoke_module.check_qt_x11_dependencies(tmp_path, {})


@pytest.mark.skipif(os.name == "nt", reason="Linux release preflight")
def test_smoke_checks_xcb_before_starting_the_ui_or_backend(smoke_module, tmp_path, monkeypatch):
    def missing(ui_dir, env):
        assert ui_dir == tmp_path / "app/neurowatch-ui"
        raise RuntimeError("xcb preflight failed")

    def unexpected(*args, **kwargs):
        pytest.fail("a broken desktop bundle must fail before any service command")

    monkeypatch.setattr(smoke_module, "check_qt_x11_dependencies", missing)
    monkeypatch.setattr(smoke_module.subprocess, "run", unexpected)
    with pytest.raises(RuntimeError, match="xcb preflight failed"):
        smoke_module.smoke(tmp_path)
