"""The background service: install, start, stop, the dev supervisor, probes."""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any

import pytest

from neurowatch.service import cli, probe
from neurowatch.service.dev import DevSupervisor, changed_files, source_snapshot
from neurowatch.service.lock import InstanceLock
from neurowatch.service.manager import (
    LoginItems,
    ServiceCommand,
    ServiceController,
    SystemdUser,
    service_command,
    source_gstreamer_env,
    systemd_unit,
    tray_desktop_entry,
)


@pytest.fixture(autouse=True)
def _state_dir(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path / "state"))
    monkeypatch.setenv("NW_IPC_ENDPOINT", f"unix:{tmp_path}/control.sock")


@pytest.fixture
def packaged(tmp_path, monkeypatch) -> Path:
    """Packaged mode: no source checkout, a backend binary next to the app."""
    backend = tmp_path / "bin" / "nw-backend"
    backend.parent.mkdir()
    backend.write_text("#!/bin/sh\n")
    monkeypatch.setenv("NW_SOURCE_ROOT", str(tmp_path / "not-a-checkout"))
    monkeypatch.setenv("NW_BACKEND_BIN", str(backend))
    return backend


class FakeSystemctl:
    """Records ``systemctl --user`` calls; answers like a user manager."""

    def __init__(self, *, available: bool = True) -> None:
        self.calls: list[list[str]] = []
        self.available = available
        self.enabled = False
        self.active = False
        self.linger = False
        self.linger_allowed = True

    def __call__(self, argv: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
        if argv[0] == "loginctl":
            self.calls.append(argv)
            if argv[1] == "enable-linger":
                if not self.linger_allowed:
                    return subprocess.CompletedProcess(argv, 1, "", "Access denied")
                self.linger = True
            if argv[1] == "disable-linger":
                self.linger = False
            out = ("yes" if self.linger else "no") if argv[1] == "show-user" else ""
            return subprocess.CompletedProcess(argv, 0, out, "")
        args = argv[2:]
        self.calls.append(args)
        out, code = "", 0
        if args[0] == "show-environment":
            code = 0 if self.available else 1
        elif args[0] == "is-enabled":
            out = "enabled" if self.enabled else "disabled"
        elif args[0] == "enable":
            self.enabled = True
        elif args[0] == "is-active":
            out = "active" if self.active else "inactive"
        elif args[0] == "is-failed":
            out = "active"
        elif args[0] == "start":
            self.active = True
        elif args[0] == "stop":
            self.active = False
        return subprocess.CompletedProcess(argv, code, out, "")

    def verbs(self) -> list[str]:
        return [call[0] for call in self.calls]


class FakeBackend:
    """A service whose status the controller polls."""

    def __init__(self, running: bool = False, headless: bool = False) -> None:
        self.running = running
        self.headless = headless
        self.shutdowns = 0
        self.starts: list[bool] = []  # headless or not, per start

    def status(self, _endpoint: str) -> dict[str, Any] | None:
        if not self.running:
            return None
        return {"status": "running", "cameras": 2, "headless": self.headless}

    def shutdown(self, _endpoint: str) -> None:
        self.shutdowns += 1
        self.running = False

    def started(self, headless: bool) -> None:
        self.running, self.headless = True, headless
        self.starts.append(headless)


def _controller(tmp_path, systemctl: FakeSystemctl, backend: FakeBackend, **kwargs):
    spawned: list[ServiceCommand] = []

    def spawn(command: ServiceCommand, _log: Path) -> None:
        spawned.append(command)
        backend.started(command.env["NW_HEADLESS"] == "1")

    if systemctl.available:
        # A started unit answers as the backend does, in the unit's mode.
        original = systemctl.__call__

        def run(argv, **kw):
            result = original(argv, **kw)
            if argv[0] == "systemctl" and argv[2] == "start":
                unit = (tmp_path / "units" / "neurowatch-backend.service").read_text()
                backend.started('"NW_HEADLESS=1"' in unit)
            if argv[0] == "systemctl" and argv[2] == "stop":
                backend.running = False
            return result

        runner: Any = run
    else:
        runner = systemctl
    controller = ServiceController(
        systemd=SystemdUser(runner=runner, unit_dir=tmp_path / "units"),
        login_items=LoginItems(autostart_dir=tmp_path / "autostart"),
        spawn=spawn,
        status=backend.status,
        shutdown=backend.shutdown,
        sleep=lambda _s: None,
        **kwargs,
    )
    return controller, spawned


# ---------------------------------------------------------------------------
# What runs, and the files that register it
# ---------------------------------------------------------------------------


def test_packaged_service_runs_the_bundled_backend(packaged) -> None:
    command = service_command("unix:/run/user/1000/neurowatch/control.sock")
    assert command.argv == [str(packaged)] and not command.dev
    assert command.env["NW_IPC_ENDPOINT"] == "unix:/run/user/1000/neurowatch/control.sock"


def test_a_bundle_with_its_own_gstreamer_passes_it_to_the_backend(packaged) -> None:
    # Windows bundles ship GStreamer; the tray may start the service at login,
    # without the bundle's launcher, so the service sets the runtime up itself.
    base = packaged.parent.parent
    assert "GST_PLUGIN_PATH_1_0" not in service_command().env, "Linux: the system's"
    (base / "gstreamer" / "bin").mkdir(parents=True)
    env = service_command().env
    assert env["GST_PLUGIN_PATH_1_0"] == str(base / "gstreamer" / "lib" / "gstreamer-1.0")
    assert env["PATH"].startswith(str(base / "gstreamer" / "bin"))
    assert env["GST_PLUGIN_SCANNER_1_0"].startswith(str(base / "gstreamer" / "libexec"))


def test_a_source_checkout_runs_the_dev_supervisor(monkeypatch) -> None:
    monkeypatch.delenv("NW_SOURCE_ROOT", raising=False)
    monkeypatch.setenv("CXX", "clang++-21")
    command = service_command()
    assert command.dev
    assert command.argv[-3:] == ["-m", "neurowatch.service", "supervise"]
    assert command.cwd is not None and (command.cwd / "Cargo.toml").is_file()
    # A systemd unit does not inherit the session: the compiler and PATH go along.
    assert command.env["CXX"] == "clang++-21" and "PATH" in command.env


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="Linux runtime overlay")
def test_source_checkout_prefers_installed_va_bugfix_runtime(tmp_path, monkeypatch) -> None:
    plugin_dir = tmp_path / ".cache/gstreamer-1.24.13/lib/x86_64-linux-gnu/gstreamer-1.0"
    plugin_dir.mkdir(parents=True)
    (plugin_dir / "libgstva.so").touch()
    monkeypatch.setenv("LD_LIBRARY_PATH", "/existing/lib")
    monkeypatch.setenv("GST_PLUGIN_PATH_1_0", "/existing/plugins")

    env = source_gstreamer_env(tmp_path)

    assert env["LD_LIBRARY_PATH"].split(os.pathsep) == [
        str(plugin_dir.parent),
        "/existing/lib",
    ]
    assert env["GST_PLUGIN_PATH_1_0"].split(os.pathsep) == [
        str(plugin_dir),
        "/existing/plugins",
    ]
    assert env["GST_REGISTRY_1_0"].endswith("gstreamer-registry-1.24.13.bin")


def _release_sdk(base: Path) -> Path:
    """A release GStreamer SDK's layout, as install_gstreamer_dev_sdk.sh builds it."""
    (base / "lib" / "gstreamer-1.0").mkdir(parents=True)
    (base / "libexec" / "gstreamer-1.0").mkdir(parents=True)
    return base


def test_a_checkout_runs_on_the_release_gstreamer_when_it_has_one(tmp_path, monkeypatch) -> None:
    from neurowatch.service.manager import ServiceError, source_gstreamer_sdk

    root = tmp_path / "checkout"
    (root / "scripts").mkdir(parents=True)
    (root / "scripts/build_release_gstreamer.sh").write_text("GST_VERSION=1.28.8\n")
    for unset in ("", "0"):
        monkeypatch.setenv("NW_GSTREAMER_SDK", unset)
        assert source_gstreamer_sdk(root) is None, "not built yet: the system's"
    monkeypatch.setenv("NW_GSTREAMER_SDK", "1")
    with pytest.raises(ServiceError, match=r"install_gstreamer_dev_sdk\.sh"):
        source_gstreamer_sdk(root)  # asked for: never a silent fallback
    default = _release_sdk(root / ".cache/gstreamer-sdk-1.28.8")
    assert source_gstreamer_sdk(root) == default
    monkeypatch.setenv("NW_GSTREAMER_SDK", "")
    assert source_gstreamer_sdk(root) == default, "what the packages ship, without asking"
    monkeypatch.delenv("NW_GSTREAMER_SDK")
    assert source_gstreamer_sdk(root) == default
    monkeypatch.setenv("NW_GSTREAMER_SDK", "0")
    assert source_gstreamer_sdk(root) is None, "the way out"
    other = _release_sdk(tmp_path / "elsewhere")
    monkeypatch.setenv("NW_GSTREAMER_SDK", str(other))
    assert source_gstreamer_sdk(root) == other


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="Linux runtime overlay")
def test_a_release_sdk_replaces_the_va_runtime_in_the_dev_service(tmp_path, monkeypatch) -> None:
    from neurowatch.service.manager import ServiceError

    monkeypatch.delenv("NW_SOURCE_ROOT", raising=False)
    sdk = _release_sdk(tmp_path / "sdk")
    monkeypatch.setenv("NW_GSTREAMER_SDK", str(sdk))
    env = service_command().env
    # Forwarded with the backend's settings; the supervisor applies it to the backend.
    assert env["NW_GSTREAMER_SDK"] == str(sdk)
    assert "GST_REGISTRY_1_0" not in env and "GST_PLUGIN_PATH_1_0" not in env
    monkeypatch.setenv("NW_GSTREAMER_SDK", str(tmp_path / "missing"))
    with pytest.raises(ServiceError):
        service_command()


def test_the_dev_supervisor_runs_the_backend_on_the_release_sdk_but_builds_as_usual(
    tmp_path, monkeypatch
) -> None:
    from neurowatch.service import dev

    sdk = _release_sdk(tmp_path / "sdk")
    monkeypatch.setenv("NW_GSTREAMER_SDK", str(sdk))
    monkeypatch.setenv("LD_LIBRARY_PATH", "/from/the/session")
    seen: dict[str, dict[str, str]] = {}

    def run(command, **kwargs):
        if command[0] == "cargo":  # not select_cxx's compiler probe
            seen["build"] = kwargs["env"]
        return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

    def popen(command, **kwargs):
        seen["backend"] = kwargs["env"]
        return object()

    monkeypatch.setattr(dev.subprocess, "run", run)
    monkeypatch.setattr(dev.subprocess, "Popen", popen)
    monkeypatch.setattr(dev, "dev_backend", lambda _workspace: tmp_path / "nw-backend")
    supervisor = DevSupervisor(tmp_path, endpoint="unix:/tmp/test.sock")
    assert supervisor._run_cargo()
    supervisor._launch_backend()

    backend, build = seen["backend"], seen["build"]
    plugins = str(sdk / "lib" / "gstreamer-1.0")
    assert backend["GST_PLUGIN_SYSTEM_PATH_1_0"] == plugins == backend["GST_PLUGIN_PATH_1_0"]
    assert backend["LD_LIBRARY_PATH"].split(os.pathsep)[0] == str(sdk / "lib")
    assert backend["GST_REGISTRY_1_0"] == str(tmp_path / ".cache/gstreamer-registry-sdk.bin")
    assert backend["NW_IPC_ENDPOINT"] == "unix:/tmp/test.sock"
    # The build links the system's development files: none of the SDK's settings.
    assert "GST_PLUGIN_SYSTEM_PATH_1_0" not in build
    assert build["LD_LIBRARY_PATH"] == "/from/the/session"


def test_the_service_gets_the_backend_settings_of_its_session(packaged, monkeypatch) -> None:
    # A development script sets backend settings, then (re)starts the service.
    monkeypatch.setenv("NW_EXAMPLE_ENDPOINT", "http://127.0.0.1:8000")
    monkeypatch.setenv(
        "LIVEKIT_URL", "ws://127.0.0.1:7880"
    )  # not the backend's: the cloud sends it
    monkeypatch.setenv("RUST_LOG", "debug")
    monkeypatch.setenv("NW_HEADLESS", "1")
    monkeypatch.setenv("NW_SERVICE", "external")
    monkeypatch.setenv("UNRELATED", "x")
    env = service_command().env
    assert env["NW_EXAMPLE_ENDPOINT"] == "http://127.0.0.1:8000"
    assert env["RUST_LOG"] == "debug" and "LIVEKIT_URL" not in env
    assert env["NW_HEADLESS"] == "0", "the mode decides, not the shell's NW_HEADLESS=1"
    assert "NW_SERVICE" not in env and "UNRELATED" not in env


def test_systemd_unit_restarts_on_failure_and_starts_at_login() -> None:
    unit = systemd_unit(
        ServiceCommand(
            ["/opt/neurowatch/bin/nw-backend"],
            cwd=Path("/opt/neurowatch/bin"),
            env={"NW_IPC_ENDPOINT": "unix:/run/user/1000/nw.sock", "PATH": "/a b:%x"},
        )
    )
    assert "ExecStart=/opt/neurowatch/bin/nw-backend" in unit
    assert "--supervise" not in unit, "systemd restarts it itself"
    assert "WorkingDirectory=/opt/neurowatch/bin" in unit
    assert 'Environment="PATH=/a b:%%x"' in unit, "spaces quoted, % escaped for systemd"
    assert "Restart=on-failure" in unit
    assert "WantedBy=default.target" in unit


def test_only_a_packaged_backend_supervises_itself(packaged, monkeypatch) -> None:
    from neurowatch.service.manager import supervised

    packaged_command = supervised(service_command())
    assert packaged_command.argv == [str(packaged), "--supervise"]
    assert supervised(packaged_command).argv == packaged_command.argv, "once"
    monkeypatch.delenv("NW_SOURCE_ROOT", raising=False)
    dev = service_command()
    assert supervised(dev).argv == dev.argv, "the dev supervisor restarts it already"


def test_stopping_waits_for_the_backend_and_its_supervisor_to_exit(tmp_path, packaged) -> None:
    """The backend stops answering before it has finished its recordings; an
    update must not find its files in use, nor kill a backend its supervisor
    would then start again."""

    class Supervised(FakeBackend):
        def status(self, endpoint: str) -> dict[str, Any] | None:
            status = super().status(endpoint)
            return None if status is None else {**status, "pid": 4242, "supervisor_pid": 4241}

    running = {4242: 3, 4241: 5}  # pid: polls until it has exited

    def alive(pid: int) -> bool:
        running[pid] = max(0, running[pid] - 1)
        return running[pid] > 0

    backend = Supervised(running=True)
    controller, _ = _controller(tmp_path, FakeSystemctl(available=False), backend, alive=alive)

    assert controller.stop()
    assert backend.shutdowns == 1
    assert running == {4242: 0, 4241: 0}, "waited for both"

    running.update({4242: 10**9, 4241: 1})
    backend.running = True
    assert not controller.stop(timeout=0), "a process that does not exit: not stopped"


def test_service_pids_and_process_liveness() -> None:
    from neurowatch.service.manager import process_alive, service_pids

    assert service_pids({"pid": 10, "supervisor_pid": None}) == [10]
    assert service_pids({"pid": 10, "supervisor_pid": 9}) == [10, 9]
    assert service_pids({"pid": True}) == [] and service_pids({}) == []
    assert process_alive(os.getpid())
    child = subprocess.Popen([sys.executable, "-c", "pass"])
    child.wait()
    assert not process_alive(child.pid)
    assert not process_alive(0)


def test_tray_login_entry_runs_the_tray() -> None:
    entry = tray_desktop_entry(["/venv/bin/python", "-m", "neurowatch.tray"])
    assert "Exec=/venv/bin/python -m neurowatch.tray" in entry
    assert "Type=Application" in entry


# ---------------------------------------------------------------------------
# Starting: registered with systemd by default, or for this session only
# ---------------------------------------------------------------------------


def test_opening_the_app_installs_and_starts_the_systemd_service(tmp_path, packaged) -> None:
    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, spawned = _controller(tmp_path, systemctl, backend)

    assert controller.ensure_running(autostart=True)

    unit = tmp_path / "units" / "neurowatch-backend.service"
    assert unit.is_file() and f"ExecStart={packaged}" in unit.read_text()
    assert "enable" in systemctl.verbs(), "starts at login"
    assert "start" in systemctl.verbs() and not spawned, "systemd runs it, not a child"
    assert (tmp_path / "autostart" / "neurowatch-tray.desktop").is_file()
    assert controller.starts_at_login()


def test_no_autostart_registers_nothing(tmp_path, packaged) -> None:
    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, spawned = _controller(tmp_path, systemctl, backend)

    assert controller.ensure_running(autostart=False)

    assert not (tmp_path / "units").exists(), "no systemd unit written"
    assert "enable" not in systemctl.verbs()
    assert not (tmp_path / "autostart").exists(), "no tray login item"
    assert len(spawned) == 1, "started for this session as a background process"
    assert not controller.starts_at_login()


def test_a_running_service_is_not_started_again(tmp_path, packaged) -> None:
    systemctl, backend = FakeSystemctl(), FakeBackend(running=True)
    controller, spawned = _controller(tmp_path, systemctl, backend)
    assert controller.ensure_running(autostart=False)
    assert not spawned and "start" not in systemctl.verbs()


def test_without_systemd_the_service_is_a_detached_process(tmp_path, packaged) -> None:
    systemctl, backend = FakeSystemctl(available=False), FakeBackend()
    controller, spawned = _controller(tmp_path, systemctl, backend)

    assert controller.ensure_running(autostart=True)

    # Nothing restarts a detached backend: it supervises itself.
    assert len(spawned) == 1 and spawned[0].argv == [str(packaged), "--supervise"]
    assert not (tmp_path / "units").exists()
    # The tray starts at login and starts the service from there.
    assert (tmp_path / "autostart" / "neurowatch-tray.desktop").is_file()


def test_a_service_that_dies_at_start_fails_fast(tmp_path, packaged) -> None:
    from neurowatch.service.manager import ServiceError

    class Exited:
        def poll(self) -> int:
            return 1

    systemctl, backend = FakeSystemctl(available=False), FakeBackend()
    controller = ServiceController(
        systemd=SystemdUser(runner=systemctl, unit_dir=tmp_path / "units"),
        login_items=LoginItems(autostart_dir=tmp_path / "autostart"),
        spawn=lambda _command, _log: Exited(),
        status=backend.status,
        sleep=lambda _s: None,
    )
    with pytest.raises(ServiceError, match=r"exited \(status 1\)"):
        controller.ensure_running(autostart=False, timeout=600)


def test_a_dev_backend_that_crashes_or_fails_to_build_fails_fast(tmp_path, monkeypatch) -> None:
    from neurowatch.service import dev
    from neurowatch.service.manager import ServiceError

    monkeypatch.delenv("NW_SOURCE_ROOT", raising=False)  # this checkout: dev mode
    backend = FakeBackend()

    def spawn(_command, _log):  # the supervisor starts, its backend crashes
        dev.write_state("crashed", code=101)

    controller = ServiceController(
        systemd=SystemdUser(runner=FakeSystemctl(available=False), unit_dir=tmp_path / "u"),
        login_items=LoginItems(autostart_dir=tmp_path / "a"),
        spawn=spawn,
        status=backend.status,
        sleep=lambda _s: None,
    )
    with pytest.raises(ServiceError, match=r"backend exited \(status 101\)"):
        controller.ensure_running(autostart=False, timeout=600)

    controller._spawn = lambda _c, _l: dev.write_state("build-failed")
    with pytest.raises(ServiceError, match="did not build"):
        controller.ensure_running(autostart=False, timeout=600)


def test_the_mode_reaches_the_backend(packaged, monkeypatch) -> None:
    from neurowatch.service.manager import HEADLESS

    monkeypatch.setenv("NW_HEADLESS", "1")  # left in a shell: does not decide
    assert service_command().env["NW_HEADLESS"] == "0"
    headless = service_command(mode=HEADLESS)
    assert headless.env["NW_HEADLESS"] == "1" and headless.headless
    assert "Description=NeuroWatch backend (camera capture and recording), headless" in (
        systemd_unit(headless)
    )


def test_a_dev_service_stopped_during_start_fails_fast_and_releases_its_lock(
    tmp_path, monkeypatch
) -> None:
    from neurowatch.service import dev
    from neurowatch.service.manager import ServiceError
    from neurowatch.service.paths import lock_path

    monkeypatch.delenv("NW_SOURCE_ROOT", raising=False)

    def unexpected_wait(_seconds):
        pytest.fail("A stopped development service must not keep the startup lock for 15 minutes")

    controller = ServiceController(
        systemd=SystemdUser(runner=FakeSystemctl(available=False), unit_dir=tmp_path / "u"),
        login_items=LoginItems(autostart_dir=tmp_path / "a"),
        spawn=lambda _command, _log: dev.write_state("stopped"),
        status=lambda _endpoint: None,
        sleep=unexpected_wait,
    )
    with pytest.raises(ServiceError, match="stopped before the backend became ready"):
        controller.ensure_running(autostart=False, timeout=600)
    lock = InstanceLock(lock_path("service-start"))
    assert lock.acquire(), "the next launch must be able to start the backend"
    lock.release()
    controller._started_at += 10
    assert controller._start_failure() is None, "an earlier stopped state is harmless"


def test_waiting_for_another_start_reports_why(tmp_path, packaged) -> None:
    from neurowatch.service.paths import lock_path

    lock = InstanceLock(lock_path("service-start"))
    assert lock.acquire()
    controller, _ = _controller(tmp_path, FakeSystemctl(available=False), FakeBackend())

    def release_other_start(_seconds):
        assert "another NeuroWatch" in controller.startup_message
        lock.release()

    controller._sleep = release_other_start
    try:
        assert controller.ensure_running(autostart=False)
        assert controller.startup_message is None
    finally:
        lock.release()


def test_headless_service_starts_at_boot_without_a_tray(tmp_path, packaged) -> None:
    from neurowatch.service.manager import HEADLESS

    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)

    assert controller.ensure_running(autostart=True, mode=HEADLESS)

    assert backend.starts == [True], "started headless"
    assert ["loginctl", "enable-linger"] in systemctl.calls, "starts at boot, no login needed"
    assert "enable" in systemctl.verbs()
    assert not (tmp_path / "autostart").exists(), "no tray on a headless machine"
    info = controller.describe()
    assert info["mode"] == "headless" and info["starts_at_boot"]


def test_switching_modes_restarts_the_service(tmp_path, packaged) -> None:
    from neurowatch.service.manager import DESKTOP, HEADLESS

    systemctl, backend = FakeSystemctl(available=False), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)
    controller.ensure_running(autostart=False, mode=DESKTOP)
    assert controller.ensure_running(autostart=False, mode=HEADLESS)
    assert backend.shutdowns == 1 and backend.starts == [False, True]

    # The tray asks for no mode: whatever runs is fine.
    assert controller.ensure_running(autostart=False)
    assert backend.starts == [False, True]
    # Started with no mode, the service starts in the last one asked for.
    controller.stop()
    assert controller.start()
    assert backend.starts == [False, True, True]
    # Opening the desktop app switches it back.
    assert controller.ensure_running(autostart=False, mode=DESKTOP)
    assert backend.starts[-1] is False


def test_lingering_refused_still_starts_the_service(tmp_path, packaged, caplog) -> None:
    from neurowatch.service.manager import HEADLESS

    systemctl, backend = FakeSystemctl(), FakeBackend()
    systemctl.linger_allowed = False
    controller, _ = _controller(tmp_path, systemctl, backend)
    with caplog.at_level("WARNING"):
        assert controller.ensure_running(autostart=True, mode=HEADLESS)
    assert "sudo loginctl enable-linger" in caplog.text
    assert not controller.describe()["starts_at_boot"]


def test_a_service_on_another_endpoint_leaves_the_users_unit_alone(tmp_path, packaged) -> None:
    from neurowatch.service.manager import HEADLESS

    systemctl = FakeSystemctl()
    users = SystemdUser(runner=systemctl, unit_dir=tmp_path / "units")
    users.install(service_command("unix:/run/user/1000/neurowatch/control.sock"), enable=True)
    before = users.unit_path.read_text()
    systemctl.calls.clear()

    backend = FakeBackend()
    controller, spawned = _controller(tmp_path, systemctl, backend)  # its own endpoint
    assert controller.ensure_running(autostart=True, mode=HEADLESS)
    controller.stop()

    assert users.unit_path.read_text() == before, "the user's unit is not rewritten"
    assert not [c for c in systemctl.verbs() if c in ("start", "stop", "enable", "daemon-reload")]
    assert len(spawned) == 1, "run as a background process instead"


def test_an_unchanged_unit_is_not_rewritten(tmp_path, packaged) -> None:
    systemctl = FakeSystemctl()
    systemd = SystemdUser(runner=systemctl, unit_dir=tmp_path / "units")
    command = service_command()
    systemd.install(command, enable=True)
    systemctl.calls.clear()
    systemd.install(command, enable=True)
    assert "daemon-reload" not in systemctl.verbs() and "enable" not in systemctl.verbs()


def test_stop_uses_systemd_or_asks_the_backend(tmp_path, packaged) -> None:
    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)
    controller.ensure_running(autostart=True)
    original_stop = controller.systemd.stop

    def stop() -> None:
        original_stop()
        backend.running = False

    controller.systemd.stop = stop  # type: ignore[method-assign]
    assert controller.stop()
    assert "stop" in systemctl.verbs() and backend.shutdowns == 0

    plain, backend2 = FakeSystemctl(available=False), FakeBackend(running=True)
    controller2, _ = _controller(tmp_path, plain, backend2)
    assert controller2.stop()
    assert backend2.shutdowns == 1, "system.shutdown over IPC"


def test_uninstall_removes_the_unit_and_the_tray_login_item(tmp_path, packaged) -> None:
    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)
    controller.ensure_running(autostart=True)
    controller.uninstall()
    assert not (tmp_path / "units" / "neurowatch-backend.service").exists()
    assert not (tmp_path / "autostart" / "neurowatch-tray.desktop").exists()
    assert ["disable", "--now", "neurowatch-backend.service"] in systemctl.calls


def test_windows_login_item_is_a_run_value(tmp_path, monkeypatch) -> None:
    from neurowatch.service import manager

    class Registry:
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ = 1, 2, 3

        def __init__(self) -> None:
            self.values: dict[str, str] = {}

        class _Key:
            def __enter__(self):
                return self

            def __exit__(self, *_):
                return False

        def OpenKey(self, *_args):  # noqa: N802
            return self._Key()

        def SetValueEx(self, _key, name, _reserved, _kind, value):  # noqa: N802
            self.values[name] = value

        def DeleteValue(self, _key, name):  # noqa: N802
            self.values.pop(name)

        def QueryValueEx(self, _key, name):  # noqa: N802
            if name not in self.values:
                raise FileNotFoundError(name)
            return self.values[name], self.REG_SZ

    monkeypatch.setattr(manager, "IS_WINDOWS", True)
    registry = Registry()
    items = LoginItems(registry=registry)
    items.install([r"C:\NeuroWatch\neurowatch-ui.exe", "--tray"])
    assert registry.values["NeuroWatch"] == r"C:\NeuroWatch\neurowatch-ui.exe --tray"
    assert items.installed()
    items.uninstall()
    assert not items.installed()


def test_one_process_at_a_time_holds_a_lock(tmp_path) -> None:
    first, second = InstanceLock(tmp_path / "x.lock"), InstanceLock(tmp_path / "x.lock")
    assert first.acquire()
    # flock locks belong to the open file, so a second open is refused too.
    assert not second.acquire()
    first.release()
    assert second.acquire()
    second.release()


# ---------------------------------------------------------------------------
# Talking to the backend
# ---------------------------------------------------------------------------


def _serve_once(path: str, answer: dict[str, Any]) -> threading.Thread:
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    server.listen(1)

    def run() -> None:
        connection, _ = server.accept()
        with connection, server:
            request = json.loads(connection.recv(65536).split(b"\n")[0])
            response = {"jsonrpc": "2.0", "id": request["id"], **answer}
            connection.sendall(json.dumps(response).encode() + b"\n")

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread


def test_probe_reads_status_and_is_quiet_when_stopped(tmp_path, caplog) -> None:
    path = str(tmp_path / "probe.sock")
    thread = _serve_once(path, {"result": {"status": "running", "pid": 42}})
    assert probe.backend_status(f"unix:{path}") == {"status": "running", "pid": 42}
    thread.join(2)

    os.unlink(path)
    with caplog.at_level("DEBUG"):
        assert probe.backend_status(f"unix:{path}") is None
    assert not [r for r in caplog.records if r.levelname == "WARNING"], "no log noise"

    _serve_once(path, {"error": {"code": -32601, "message": "nope"}})
    assert probe.request("system.shutdown", endpoint=f"unix:{path}") is None


# ---------------------------------------------------------------------------
# Dev supervisor: rebuild and restart on backend source changes
# ---------------------------------------------------------------------------


class FakeProcess:
    def __init__(self) -> None:
        self.code: int | None = None
        self.stopped = False

    def poll(self) -> int | None:
        return self.code

    def wait(self, timeout: float | None = None) -> int:
        self.code = 0 if self.code is None else self.code
        return self.code

    def terminate(self) -> None:
        self.code = -15

    def kill(self) -> None:
        self.code = -9


class Script:
    """Drives the supervisor's loop: one step per poll."""

    def __init__(self, supervisor_steps: list[Any]) -> None:
        self.steps = supervisor_steps
        self.now = 0.0

    def sleep(self, seconds: float) -> None:
        self.now += seconds
        if self.steps:
            step = self.steps.pop(0)
            if step is not None:
                step()

    def clock(self) -> float:
        return self.now


def _supervisor(tmp_path, builds: list[bool], script: Script, snapshots: list[dict]):
    processes: list[FakeProcess] = []
    shutdowns: list[str] = []
    build_results = list(builds)
    state = {"snapshot": snapshots[0]}

    def launch() -> FakeProcess:
        process = FakeProcess()
        processes.append(process)
        return process

    def request_shutdown(endpoint: str) -> None:
        shutdowns.append(endpoint)

    supervisor = DevSupervisor(
        tmp_path,
        endpoint="unix:/tmp/test.sock",
        build=lambda: build_results.pop(0) if build_results else True,
        launch=launch,
        request_shutdown=request_shutdown,
        snapshot=lambda _root: state["snapshot"],
        sleep=script.sleep,
        clock=script.clock,
    )
    return supervisor, processes, shutdowns, state


def test_dev_supervisor_rebuilds_and_restarts_on_change(tmp_path) -> None:
    script = Script([])
    supervisor, processes, shutdowns, state = _supervisor(
        tmp_path, [True, True], script, [{"a.rs": (1, 1)}]
    )

    def edit() -> None:
        state["snapshot"] = {"a.rs": (2, 1)}

    def stop_on_request() -> None:
        processes[-1].code = 0  # tray "Stop": system.shutdown, exit 0

    script.steps = [None, edit, None, None, None, stop_on_request]
    assert supervisor._loop() == 0
    assert len(processes) == 2, "rebuilt and restarted once"
    assert processes[0].code == 0 and shutdowns == ["unix:/tmp/test.sock"]


def test_dev_supervisor_keeps_the_backend_when_a_build_fails(tmp_path) -> None:
    script = Script([])
    supervisor, processes, _shutdowns, state = _supervisor(
        tmp_path, [True, False], script, [{"a.rs": (1, 1)}]
    )

    def edit() -> None:
        state["snapshot"] = {"a.rs": (2, 1)}

    def stop() -> None:
        supervisor.stop()

    script.steps = [edit, None, None, stop]
    supervisor._loop()
    assert len(processes) == 1 and processes[0].code is None, "the old backend kept running"


def test_dev_supervisor_restarts_a_crashed_backend(tmp_path) -> None:
    script = Script([])
    supervisor, processes, _shutdowns, _state = _supervisor(tmp_path, [True], script, [{}])

    def crash() -> None:
        processes[-1].code = 101

    steps: list[Any] = [crash] + [None] * 4 + [lambda: supervisor.stop()]
    script.steps = steps
    supervisor._loop()
    assert len(processes) == 2, "restarted after the crash pause"


def test_source_snapshot_sees_rust_sources_but_not_target(tmp_path) -> None:
    (tmp_path / "Cargo.toml").write_text("[workspace]")
    src = tmp_path / "crates" / "nw-backend" / "src"
    src.mkdir(parents=True)
    (src / "main.rs").write_text("fn main() {}")
    target = tmp_path / "crates" / "nw-backend" / "target"
    target.mkdir()
    (target / "junk.rs").write_text("")
    (src / "notes.txt").write_text("")
    before = source_snapshot(tmp_path)
    assert {Path(p).name for p in before} == {"Cargo.toml", "main.rs"}
    (src / "main.rs").write_text("fn main() { println!(); }")
    assert changed_files(before, source_snapshot(tmp_path)) == [str(src / "main.rs")]


# ---------------------------------------------------------------------------
# neurowatch service ...
# ---------------------------------------------------------------------------


def test_cli_status_and_start(tmp_path, packaged, capsys) -> None:
    systemctl, backend = FakeSystemctl(available=False), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)
    assert cli.main(["status"], controller=controller) == 3
    assert "NeuroWatch service: stopped" in capsys.readouterr().out
    assert cli.main(["start", "--no-autostart"], controller=controller) == 0
    assert cli.main(["status", "--json"], controller=controller) == 0
    info = json.loads(capsys.readouterr().out.split("\n", 1)[1])
    assert info["running"] and not info["starts_at_login"]


def test_the_bundled_nettle_3_is_used_only_without_the_systems_own(tmp_path) -> None:
    from neurowatch.service.manager import bundled_library_path

    gst = tmp_path / "gstreamer"
    (gst / "lib" / "nettle3").mkdir(parents=True)
    # Arch: nettle 4 only, so librtmp needs the bundled nettle 3.
    arch = bundled_library_path(gst, has_library=lambda _soname: False)
    assert arch.split(os.pathsep) == [str(gst / "lib"), str(gst / "lib" / "nettle3")]
    # Debian, Fedora, Ubuntu: their own nettle 3, which their TLS library needs.
    debian = bundled_library_path(gst, has_library=lambda soname: soname == "libnettle.so.8")
    assert debian == str(gst / "lib")


# ---------------------------------------------------------------------------
# Settings → Startup: the user's choice decides what is registered
# ---------------------------------------------------------------------------


def test_startup_choices_are_saved_and_default_to_starting_at_login() -> None:
    from neurowatch.service.startup import StartupPrefs, load_prefs, save_prefs

    assert load_prefs() == StartupPrefs(at_login=True, open_window=False, at_boot=False)
    save_prefs(StartupPrefs(at_login=False, open_window=True, at_boot=False))
    assert load_prefs() == StartupPrefs(at_login=False, open_window=True, at_boot=False)


def test_opening_the_app_respects_a_switched_off_start_at_sign_in(tmp_path, packaged) -> None:
    """Every UI launch registers the service; a user who switched starting at
    sign-in off must not find it switched back on."""
    from neurowatch.service.startup import StartupPrefs, save_prefs

    save_prefs(StartupPrefs(at_login=False))
    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)

    assert controller.ensure_running(autostart=True)

    assert "enable" not in systemctl.verbs()
    assert not (tmp_path / "autostart" / "neurowatch-tray.desktop").exists()
    assert not controller.starts_at_login()


def test_applying_startup_choices_registers_the_window_and_recording_from_boot(
    tmp_path, packaged
) -> None:
    from neurowatch.service.startup import StartupPrefs, load_prefs

    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)
    entry = tmp_path / "autostart" / "neurowatch-tray.desktop"

    wanted = StartupPrefs(at_login=True, open_window=True, at_boot=True)
    assert controller.apply_startup(wanted) == []
    assert load_prefs() == wanted
    assert controller.starts_at_login() and systemctl.linger, "records from boot"
    assert "neurowatch.tray" not in entry.read_text(), "the window opens at sign-in"

    assert controller.apply_startup(StartupPrefs(at_login=True)) == []
    assert "neurowatch.tray" in entry.read_text(), "only the tray at sign-in"
    assert not systemctl.linger, "starts at sign-in again, not at boot"

    assert controller.apply_startup(StartupPrefs(at_login=False)) == []
    assert not entry.exists() and not controller.starts_at_login()


def test_recording_from_boot_refused_says_what_to_run(tmp_path, packaged) -> None:
    from neurowatch.service.startup import StartupPrefs

    systemctl, backend = FakeSystemctl(), FakeBackend()
    systemctl.linger_allowed = False
    controller, _ = _controller(tmp_path, systemctl, backend)

    [problem] = controller.apply_startup(StartupPrefs(at_login=True, at_boot=True))

    assert "sudo loginctl enable-linger" in problem
    assert controller.starts_at_login(), "it still starts at sign-in"


def test_cli_install_and_uninstall_change_the_startup_choice(tmp_path, packaged, capsys) -> None:
    from neurowatch.service.startup import load_prefs

    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)
    assert cli.main(["uninstall"], controller=controller) == 0
    assert not load_prefs().at_login
    assert cli.main(["install"], controller=controller) == 0
    assert load_prefs().at_login and controller.starts_at_login()


# -- system tools under a packaged build -------------------------------------
# On a distribution newer than the build machine, NeuroWatch never started
# again after a restart: the packaged build (PyInstaller) ran `systemctl --user`
# with its bundled libraries on LD_LIBRARY_PATH, systemctl failed on the
# bundle's older OpenSSL, and the service fell back to a background process
# that nothing registered to start again.

_BUNDLE = "/opt/neurowatch/app/neurowatch-ui/_internal"
_linux_only = pytest.mark.skipif(not sys.platform.startswith("linux"), reason="Linux service")


@_linux_only
def test_host_environment_drops_the_bundles_library_path() -> None:
    from neurowatch.service.paths import host_environment

    env = host_environment({"LD_LIBRARY_PATH": _BUNDLE, "PATH": "/usr/bin"}, frozen=True)
    assert "LD_LIBRARY_PATH" not in env
    assert env["PATH"] == "/usr/bin"

    # The user's own library path, which PyInstaller kept aside, comes back.
    env = host_environment(
        {"LD_LIBRARY_PATH": f"{_BUNDLE}:/opt/mine", "LD_LIBRARY_PATH_ORIG": "/opt/mine"},
        frozen=True,
    )
    assert env["LD_LIBRARY_PATH"] == "/opt/mine"
    assert "LD_LIBRARY_PATH_ORIG" not in env


def test_host_environment_changes_nothing_outside_a_packaged_build(monkeypatch) -> None:
    from neurowatch.service import paths

    given = {"LD_LIBRARY_PATH": "/opt/dev/lib", "PATH": "/usr/bin"}
    assert paths.host_environment(given, frozen=False) == given, "a checkout keeps its own"
    monkeypatch.setattr(paths, "IS_WINDOWS", True)
    assert paths.host_environment(given, frozen=True) == given, "Windows: nothing to undo"


@_linux_only
def test_restore_host_environment_fixes_the_process_environment(monkeypatch) -> None:
    from neurowatch.service.paths import restore_host_environment

    monkeypatch.setenv("LD_LIBRARY_PATH", _BUNDLE)
    monkeypatch.delenv("LD_LIBRARY_PATH_ORIG", raising=False)
    restore_host_environment(frozen=False)
    assert os.environ["LD_LIBRARY_PATH"] == _BUNDLE, "untouched outside a packaged build"

    restore_host_environment(frozen=True)
    assert "LD_LIBRARY_PATH" not in os.environ

    monkeypatch.setenv("LD_LIBRARY_PATH", _BUNDLE)
    monkeypatch.setenv("LD_LIBRARY_PATH_ORIG", "/opt/mine")
    restore_host_environment(frozen=True)
    assert os.environ["LD_LIBRARY_PATH"] == "/opt/mine"
    assert "LD_LIBRARY_PATH_ORIG" not in os.environ


def test_the_packaged_entry_point_restores_the_environment_before_anything_else() -> None:
    entry = Path(__file__).resolve().parents[2] / "scripts" / "entrypoints" / "neurowatch_ui.py"
    text = entry.read_text()
    assert text.index("restore_host_environment()") < text.index("from neurowatch.main import")


@_linux_only
def test_systemctl_and_loginctl_run_with_the_systems_libraries(tmp_path, monkeypatch) -> None:
    seen: list[tuple[str, dict[str, str]]] = []

    def runner(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        seen.append((argv[0], kwargs["env"]))
        return subprocess.CompletedProcess(argv, 0, "no", "")

    monkeypatch.setattr(sys, "frozen", True, raising=False)
    monkeypatch.setenv("LD_LIBRARY_PATH", _BUNDLE)
    monkeypatch.delenv("LD_LIBRARY_PATH_ORIG", raising=False)
    systemd = SystemdUser(runner=runner, unit_dir=tmp_path / "units")
    monkeypatch.setattr("shutil.which", lambda _name: "/usr/bin/systemctl")

    assert systemd.available()
    systemd.lingering()

    assert [tool for tool, _env in seen] == ["systemctl", "loginctl"]
    for tool, env in seen:
        assert "LD_LIBRARY_PATH" not in env, f"{tool} must not load the bundle's libraries"


@_linux_only
def test_systemctl_gets_the_login_runtime_folder_when_the_session_has_none(
    tmp_path, monkeypatch
) -> None:
    seen: list[dict[str, str]] = []

    def runner(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        seen.append(kwargs["env"])
        return subprocess.CompletedProcess(argv, 0, "", "")

    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.setattr("neurowatch.service.manager.login_runtime_dir", lambda: tmp_path)
    monkeypatch.setattr("shutil.which", lambda _name: "/usr/bin/systemctl")
    assert SystemdUser(runner=runner, unit_dir=tmp_path / "units").available()
    assert seen[0]["XDG_RUNTIME_DIR"] == str(tmp_path)


@_linux_only
def test_a_failing_systemctl_is_reported_not_taken_for_no_systemd(
    tmp_path, packaged, monkeypatch
) -> None:
    from neurowatch.service.manager import HEADLESS

    error = (
        "systemctl: /opt/neurowatch/app/neurowatch-ui/_internal/libcrypto.so.3: version"
        " `OPENSSL_3.4.0' not found (required by libsystemd-shared-257.so)"
    )

    def runner(argv: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(argv, 1, "", error)

    monkeypatch.setattr("shutil.which", lambda _name: "/usr/bin/systemctl")
    backend = FakeBackend()
    controller = ServiceController(
        systemd=SystemdUser(runner=runner, unit_dir=tmp_path / "units"),
        login_items=LoginItems(autostart_dir=tmp_path / "autostart"),
        spawn=lambda command, _log: backend.started(command.env["NW_HEADLESS"] == "1"),
        status=backend.status,
        shutdown=backend.shutdown,
        sleep=lambda _s: None,
    )
    assert controller.ensure_running(autostart=True, mode=HEADLESS), "still runs now"
    problem = controller.autostart_problem(HEADLESS)
    assert problem is not None and "OPENSSL_3.4.0" in problem


@_linux_only
def test_autostart_problem_names_what_is_missing(tmp_path, packaged) -> None:
    from neurowatch.service.manager import DESKTOP, HEADLESS
    from neurowatch.service.startup import StartupPrefs, save_prefs

    systemctl, backend = FakeSystemctl(), FakeBackend()
    controller, _ = _controller(tmp_path, systemctl, backend)
    assert controller.ensure_running(autostart=True, mode=HEADLESS)
    assert controller.autostart_problem(HEADLESS) is None, "registered and lingering"

    systemctl.linger = False
    assert "sudo loginctl enable-linger" in (controller.autostart_problem(HEADLESS) or "")
    assert controller.autostart_problem(DESKTOP) is None, "desktop mode starts at login"

    save_prefs(StartupPrefs(at_login=False, open_window=False, at_boot=False))
    assert controller.autostart_problem(HEADLESS) is None, "the user switched it off"

    save_prefs(StartupPrefs(at_login=True, open_window=False, at_boot=False))
    plain, backend2 = FakeSystemctl(available=False), FakeBackend()
    controller2, _ = _controller(tmp_path / "plain", plain, backend2)
    assert controller2.ensure_running(autostart=True, mode=DESKTOP)
    assert controller2.autostart_problem(DESKTOP) is None, "the tray's login item starts it"
    assert "not registered to start by itself" in (controller2.autostart_problem(HEADLESS) or "")


# -- the endpoint in sessions without XDG_RUNTIME_DIR --------------------------


@pytest.fixture
def no_runtime_env(tmp_path, monkeypatch) -> Path:
    """A session without XDG_RUNTIME_DIR (as some SSH servers open) whose user has a
    login runtime folder; returns that folder."""
    from neurowatch.service import paths

    runtime = tmp_path / "run-user-1000"
    runtime.mkdir()
    monkeypatch.delenv("NW_IPC_ENDPOINT", raising=False)
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "config"))
    monkeypatch.setattr(paths, "login_runtime_dir", lambda: runtime)
    monkeypatch.setattr(paths, "_socket_exists", lambda _endpoint: False)
    return runtime


@_linux_only
def test_endpoint_without_xdg_runtime_dir_is_the_login_runtime_folder(no_runtime_env) -> None:
    from neurowatch.service.paths import service_endpoint

    assert service_endpoint() == f"unix:{no_runtime_env}/neurowatch/control.sock"


@_linux_only
def test_endpoint_without_any_runtime_folder_stays_in_tmp(monkeypatch) -> None:
    from neurowatch.service import paths

    monkeypatch.delenv("NW_IPC_ENDPOINT", raising=False)
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.setattr(paths, "login_runtime_dir", lambda: None)
    assert paths.service_endpoint() == "unix:/tmp/neurowatch/control.sock"


@_linux_only
def test_a_service_already_on_the_tmp_endpoint_keeps_it(no_runtime_env, monkeypatch) -> None:
    from neurowatch.service import paths

    legacy = "unix:/tmp/neurowatch/control.sock"
    monkeypatch.setattr(paths, "_socket_exists", lambda endpoint: endpoint == legacy)
    assert paths.service_endpoint() == legacy, "running: no second service"

    monkeypatch.setattr(paths, "_socket_exists", lambda _endpoint: False)
    unit = paths.systemd_unit_path()
    unit.parent.mkdir(parents=True)
    unit.write_text(systemd_unit(service_command(legacy)))
    assert paths.service_endpoint() == legacy, "registered on it: the unit stays the user's"


@_linux_only
def test_the_xdg_runtime_dir_still_wins(no_runtime_env, monkeypatch) -> None:
    from neurowatch.service.paths import service_endpoint

    monkeypatch.setenv("XDG_RUNTIME_DIR", "/run/user/4242")
    assert service_endpoint() == "unix:/run/user/4242/neurowatch/control.sock"
    monkeypatch.setenv("NW_IPC_ENDPOINT", "unix:/somewhere/else.sock")
    assert service_endpoint() == "unix:/somewhere/else.sock"


@_linux_only
def test_login_runtime_dir_is_only_the_users_own(tmp_path) -> None:
    from neurowatch.service.paths import login_runtime_dir

    uid = os.getuid()
    assert login_runtime_dir(uid, root=tmp_path) is None, "missing"
    (tmp_path / str(uid)).mkdir()
    assert login_runtime_dir(uid, root=tmp_path) == tmp_path / str(uid)
    (tmp_path / str(uid + 1)).mkdir()
    assert login_runtime_dir(uid + 1, root=tmp_path) is None, "owned by someone else"
