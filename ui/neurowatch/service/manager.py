"""Install, start, stop and check the NeuroWatch background service.

The backend runs as a per-user background service that outlives the desktop
UI, so cameras keep recording with the window closed:

* **Linux**: a systemd user service (``neurowatch-backend.service``), enabled
  so it starts at login and restarted if it crashes. Without a systemd user
  manager the backend is started as a detached process instead.
* **Windows**: a detached per-user process. A Windows service would run in
  session 0, which cannot share the frame memory or the GPU with the user's
  desktop, so the service is started by the tray at login instead (a ``Run``
  entry).

The tray (``neurowatch.tray``) is registered to start at login on both
platforms. ``autostart=False`` (``--no-autostart``) starts the service for
this session only and registers nothing.

The service runs in one of two **modes**: ``desktop`` (the desktop UI shows
its cameras) or ``headless`` (``NW_HEADLESS=1``: no frame memory for a UI,
settings managed from the web portal). Opening the desktop app asks for
desktop, ``neurowatch --headless`` for headless; a running service in the
other mode is restarted in the one asked for, and the mode last asked for is
the one that starts at login. Headless autostart also enables systemd
lingering, so the service starts at boot on a machine nobody logs in to, and
registers no tray.

In a source checkout the service is the dev supervisor
(``neurowatch.service.dev``), which rebuilds and restarts the backend when its
code changes.
"""

from __future__ import annotations

import getpass
import logging
import os
import shlex
import shutil
import subprocess
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from neurowatch.service import probe
from neurowatch.service.lock import InstanceLock
from neurowatch.service.paths import (
    IS_WINDOWS,
    SERVICE_NAME,
    lock_path,
    log_path,
    packaged_backend,
    python_command,
    service_endpoint,
    source_root,
    state_dir,
    ui_command,
)
from neurowatch.service.startup import StartupPrefs, load_prefs, save_prefs

logger = logging.getLogger(__name__)

#: How long a packaged backend may take to answer after starting.
START_TIMEOUT_S = 30.0
#: The dev supervisor builds the backend first; a cold build takes minutes.
DEV_START_TIMEOUT_S = 900.0
STOP_TIMEOUT_S = 35.0  # the backend bounds its own shutdown at 15 s
#: Variables a systemd unit cannot inherit from the session that installs it:
#: the dev build's toolchain, and the backend's own settings (``NW_*``).
_DEV_ENV = (
    "PATH",
    "CXX",
    "CC",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "PKG_CONFIG_PATH",
)
_BACKEND_ENV_PREFIXES = ("NW_",)
_BACKEND_ENV = ("RUST_LOG", "GST_DEBUG")
#: Launcher and UI settings that are not the backend's.
_NOT_BACKEND_ENV = {"NW_IPC_ENDPOINT", "NW_HEADLESS", "NW_SOURCE_ROOT", "NW_SERVICE"}
_DISPLAY_ENV = ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY")
_TRAY_DESKTOP = "neurowatch-tray.desktop"
_RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"
_RUN_VALUE = "NeuroWatch"

DESKTOP, HEADLESS = "desktop", "headless"
_MODE_FILE = "service-mode"

Sleep = Callable[[float], None]
Runner = Callable[..., "subprocess.CompletedProcess[str]"]


class ServiceError(RuntimeError):
    """The service could not be installed, started or stopped."""


@dataclass
class ServiceCommand:
    """What the service runs: the backend, or the dev supervisor."""

    argv: list[str]
    cwd: Path | None = None
    env: dict[str, str] = field(default_factory=dict)
    dev: bool = False
    headless: bool = False


def saved_mode() -> str:
    """The mode last asked for (desktop until one was)."""
    try:
        mode = (state_dir() / _MODE_FILE).read_text().strip()
    except OSError:
        return DESKTOP
    return mode if mode in (DESKTOP, HEADLESS) else DESKTOP


def save_mode(mode: str) -> None:
    try:
        (state_dir() / _MODE_FILE).write_text(mode)
    except OSError as error:
        logger.warning("Could not remember the service mode: %s", error)


def running_mode(status: Mapping[str, Any]) -> str:
    """The mode of a running backend (an older one reports none: desktop)."""
    return HEADLESS if status.get("headless") is True else DESKTOP


def backend_environment() -> dict[str, str]:
    """The backend's settings from this environment, for the service."""
    return {
        name: value
        for name, value in os.environ.items()
        if value
        and name not in _NOT_BACKEND_ENV
        and (name.startswith(_BACKEND_ENV_PREFIXES) or name in _BACKEND_ENV)
    }


def source_gstreamer_env(root: Path) -> dict[str, str]:
    """Prefer the verified VA bug-fix runtime installed for a source checkout."""
    if not sys.platform.startswith("linux"):
        return {}
    runtime = root / ".cache" / "gstreamer-1.24.13"
    plugins = next(runtime.glob("lib/*/gstreamer-1.0"), None)
    if plugins is None or not (plugins / "libgstva.so").is_file():
        return {}
    libdir = plugins.parent

    def prepend(path: Path, current: str | None) -> str:
        value = str(path)
        existing = current.split(os.pathsep) if current else []
        return os.pathsep.join([value, *(part for part in existing if part != value)])

    return {
        "LD_LIBRARY_PATH": prepend(libdir, os.environ.get("LD_LIBRARY_PATH")),
        "GST_PLUGIN_PATH_1_0": prepend(plugins, os.environ.get("GST_PLUGIN_PATH_1_0")),
        "GST_REGISTRY_1_0": str(root / ".cache" / "gstreamer-registry-1.24.13.bin"),
    }


def service_command(endpoint: str | None = None, mode: str = DESKTOP) -> ServiceCommand:
    """What the service runs, with the settings it gets when (re)started."""
    headless = mode == HEADLESS
    env = {
        **backend_environment(),
        "NW_IPC_ENDPOINT": endpoint or service_endpoint(),
        # Explicit either way: an NW_HEADLESS left in a shell must not decide.
        "NW_HEADLESS": "1" if headless else "0",
    }
    root = source_root()
    if root is not None:
        for name in _DEV_ENV:
            value = os.environ.get(name)
            if value:
                env[name] = value
        env.update(source_gstreamer_env(root))
        argv = python_command("neurowatch.service", "supervise", windowless=True)
        return ServiceCommand(argv, cwd=root, env=env, dev=True, headless=headless)
    backend = packaged_backend()
    if backend is None:
        raise ServiceError("The NeuroWatch backend (nw-backend) was not found next to the app")
    env.update(bundled_gstreamer_env(backend.parent.parent))
    return ServiceCommand([str(backend)], cwd=backend.parent, env=env, headless=headless)


def bundled_gstreamer_env(base: Path) -> dict[str, str]:
    """The matched GStreamer runtime shipped in ``<base>/gstreamer``.

    The service may start at login from the tray, not through the bundle's
    launcher, so it sets this up itself on both Linux and Windows.
    """
    gst = base / "gstreamer"
    if not gst.is_dir():
        return {}
    plugins = str(gst / "lib" / "gstreamer-1.0")
    scanner = "gst-plugin-scanner.exe" if IS_WINDOWS else "gst-plugin-scanner"
    path = os.pathsep.join([str(gst / "bin"), str(base / "bin"), os.environ.get("PATH", "")])
    if IS_WINDOWS:
        registry_dir = Path(os.environ.get("LOCALAPPDATA") or Path.home() / "AppData/Local")
        registry_dir /= "Neurowatch"
    else:
        registry_dir = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
        registry_dir /= "neurowatch"
    env = {
        "PATH": path,
        "GST_PLUGIN_PATH_1_0": plugins,
        "GST_PLUGIN_SYSTEM_PATH_1_0": plugins,
        "GST_PLUGIN_SCANNER_1_0": str(gst / "libexec" / "gstreamer-1.0" / scanner),
        "GST_REGISTRY_1_0": str(registry_dir / "gstreamer-registry.bin"),
    }
    if IS_WINDOWS:
        env["GIO_MODULE_DIR"] = str(gst / "lib" / "gio" / "modules")
    else:
        # Do not inherit PyInstaller's LD_LIBRARY_PATH: it belongs to the UI's
        # Python/Qt bundle and can substitute incompatible backend libraries.
        env["LD_LIBRARY_PATH"] = bundled_library_path(gst)
    return env


def bundled_library_path(gst: Path, has_library: Callable[[str], bool] | None = None) -> str:
    """The bundled GStreamer's libraries, plus its nettle 3 (for librtmp) only
    on a system without nettle 3 of its own (Arch: nettle 4 only). A system
    with nettle 3 keeps its own: its TLS library may need a newer one."""
    has_library = has_library or _system_has_library
    paths = [str(gst / "lib")]
    fallback = gst / "lib" / "nettle3"
    if fallback.is_dir() and not has_library("libnettle.so.8"):
        paths.append(str(fallback))
    return os.pathsep.join(paths)


def _system_has_library(soname: str) -> bool:
    import ctypes

    try:
        ctypes.CDLL(soname)
    except OSError:
        return False
    return True


def systemd_unit(command: ServiceCommand) -> str:
    """The systemd user unit that runs ``command``."""
    lines = [
        "# Written by NeuroWatch; `neurowatch service uninstall` removes it.",
        "[Unit]",
        "Description=NeuroWatch backend (camera capture and recording)"
        + (", headless" if command.headless else "")
        + (" - development build" if command.dev else ""),
        "",
        "[Service]",
        "Type=simple",
        f"ExecStart={shlex.join(command.argv)}",
    ]
    if command.cwd is not None:
        lines.append(f"WorkingDirectory={command.cwd}")
    for name, value in sorted(command.env.items()):
        lines.append(f'Environment="{name}={_unit_escape(value)}"')
    lines += [
        "Restart=on-failure",
        "RestartSec=3",
        f"TimeoutStopSec={int(STOP_TIMEOUT_S)}",
        "",
        "[Install]",
        "WantedBy=default.target",
        "",
    ]
    return "\n".join(lines)


def _unit_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")


def tray_desktop_entry(argv: Sequence[str]) -> str:
    """XDG autostart entry that starts the tray at login (Linux)."""
    return "\n".join(
        [
            "[Desktop Entry]",
            "Type=Application",
            "Name=NeuroWatch",
            "Comment=NeuroWatch background service status",
            f"Exec={shlex.join(argv)}",
            "Icon=video-display",
            "Terminal=false",
            "X-GNOME-Autostart-enabled=true",
            "NoDisplay=true",
            "",
        ]
    )


class SystemdUser:
    """``systemctl --user`` for the backend's unit (Linux)."""

    def __init__(self, runner: Runner = subprocess.run, unit_dir: Path | None = None) -> None:
        self._run = runner
        self.unit_dir = unit_dir or (
            Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / "systemd" / "user"
        )
        self.unit_name = f"{SERVICE_NAME}.service"

    @property
    def unit_path(self) -> Path:
        return self.unit_dir / self.unit_name

    def available(self) -> bool:
        if IS_WINDOWS or shutil.which("systemctl") is None:
            return False
        return self._systemctl("show-environment", check=False).returncode == 0

    def installed(self) -> bool:
        return self.unit_path.is_file()

    def unit_endpoint(self) -> str | None:
        """The IPC endpoint the installed unit's service listens on."""
        try:
            text = self.unit_path.read_text()
        except OSError:
            return None
        prefix = 'Environment="NW_IPC_ENDPOINT='
        for line in text.splitlines():
            if line.startswith(prefix) and line.endswith('"'):
                return line[len(prefix) : -1].replace("%%", "%")
        return None

    def active(self) -> bool:
        result = self._systemctl("is-active", self.unit_name, check=False)
        return result.stdout.strip() in ("active", "activating", "reloading")

    def enabled(self) -> bool:
        result = self._systemctl("is-enabled", self.unit_name, check=False)
        return result.stdout.strip() == "enabled"

    def install(self, command: ServiceCommand, *, enable: bool) -> None:
        text = systemd_unit(command)
        current = self.unit_path.read_text() if self.installed() else None
        if current != text:
            self.unit_dir.mkdir(parents=True, exist_ok=True)
            self.unit_path.write_text(text)
            self._systemctl("daemon-reload")
            logger.info("Wrote systemd user unit %s", self.unit_path)
        if enable and not self.enabled():
            self._systemctl("enable", self.unit_name)
            logger.info("Enabled %s at login", self.unit_name)

    def uninstall(self) -> None:
        if not self.installed():
            return
        self._systemctl("disable", "--now", self.unit_name, check=False)
        self.unit_path.unlink(missing_ok=True)
        self._systemctl("daemon-reload", check=False)
        self._systemctl("reset-failed", self.unit_name, check=False)

    def start(self) -> None:
        # The backend opens X11 video windows for native overlays; a user
        # manager started before the desktop may not have the display yet.
        names = [name for name in _DISPLAY_ENV if os.environ.get(name)]
        if names:
            self._systemctl("import-environment", *names, check=False)
        self._systemctl("reset-failed", self.unit_name, check=False)
        self._systemctl("start", "--no-block", self.unit_name)

    def stop(self) -> None:
        self._systemctl("stop", self.unit_name)

    def failed(self) -> bool:
        return self._systemctl("is-failed", self.unit_name, check=False).stdout.strip() == "failed"

    def lingering(self) -> bool:
        """Whether this user's services start at boot, without a login."""
        result = self._loginctl("show-user", getpass.getuser(), "--property=Linger", "--value")
        return result.returncode == 0 and result.stdout.strip() == "yes"

    def enable_linger(self) -> bool:
        """Start this user's services at boot (a headless machine nobody logs
        in to). Allowed for one's own account on common distributions."""
        if self.lingering():
            return True
        result = self._loginctl("enable-linger")
        if result.returncode != 0:
            logger.warning(
                "Could not enable lingering (%s); NeuroWatch starts at login, not at boot."
                " To start it at boot: sudo loginctl enable-linger %s",
                (result.stderr or result.stdout).strip(),
                getpass.getuser(),
            )
            return False
        logger.info("Enabled lingering: NeuroWatch starts at boot")
        return True

    def disable_linger(self) -> bool:
        """Start this user's services at login again, not at boot."""
        if not self.lingering():
            return True
        result = self._loginctl("disable-linger")
        if result.returncode != 0:
            logger.warning(
                "Could not disable lingering (%s): sudo loginctl disable-linger %s",
                (result.stderr or result.stdout).strip(),
                getpass.getuser(),
            )
            return False
        logger.info("Disabled lingering: NeuroWatch starts at login")
        return True

    def _loginctl(self, *args: str) -> subprocess.CompletedProcess[str]:
        try:
            return self._run(["loginctl", *args], capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired) as error:
            return subprocess.CompletedProcess(args, 1, "", str(error))

    def _systemctl(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        try:
            result = self._run(
                ["systemctl", "--user", *args], capture_output=True, text=True, timeout=60
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            if check:
                raise ServiceError(f"systemctl --user {' '.join(args)} failed: {error}") from error
            return subprocess.CompletedProcess(args, 1, "", str(error))
        if check and result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            raise ServiceError(f"systemctl --user {' '.join(args)} failed: {detail}")
        return result


class LoginItems:
    """Start the tray at login: XDG autostart (Linux), a ``Run`` value (Windows)."""

    def __init__(self, autostart_dir: Path | None = None, registry: Any = None) -> None:
        self.autostart_dir = autostart_dir or (
            Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / "autostart"
        )
        self._registry = registry

    def install(self, argv: Sequence[str]) -> None:
        if IS_WINDOWS:
            self._set_run_value(subprocess.list2cmdline(list(argv)))
            return
        path = self.autostart_dir / _TRAY_DESKTOP
        text = tray_desktop_entry(argv)
        if not path.is_file() or path.read_text() != text:
            self.autostart_dir.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
            logger.info("Tray starts at login: %s", path)

    def uninstall(self) -> None:
        if IS_WINDOWS:
            self._set_run_value(None)
            return
        (self.autostart_dir / _TRAY_DESKTOP).unlink(missing_ok=True)

    def installed(self) -> bool:
        if IS_WINDOWS:
            return self._get_run_value() is not None
        return (self.autostart_dir / _TRAY_DESKTOP).is_file()

    def _winreg(self) -> Any:
        if self._registry is not None:
            return self._registry
        import winreg

        return winreg

    def _set_run_value(self, command: str | None) -> None:
        reg = self._winreg()
        with reg.OpenKey(reg.HKEY_CURRENT_USER, _RUN_KEY, 0, reg.KEY_SET_VALUE) as key:
            if command is None:
                try:
                    reg.DeleteValue(key, _RUN_VALUE)
                except FileNotFoundError:
                    pass
            else:
                reg.SetValueEx(key, _RUN_VALUE, 0, reg.REG_SZ, command)

    def _get_run_value(self) -> str | None:
        reg = self._winreg()
        try:
            with reg.OpenKey(reg.HKEY_CURRENT_USER, _RUN_KEY) as key:
                value, _kind = reg.QueryValueEx(key, _RUN_VALUE)
        except FileNotFoundError:
            return None
        return str(value)


def spawn_detached(command: ServiceCommand, log_file: Path) -> subprocess.Popen[Any]:
    """Start ``command`` so it outlives this process and its terminal."""
    env = {**os.environ, **command.env}
    log = open(log_file, "ab")  # the child keeps its own copy
    kwargs: dict[str, Any] = {
        "cwd": str(command.cwd) if command.cwd else None,
        "env": env,
        "stdin": subprocess.DEVNULL,
        "stdout": log,
        "stderr": subprocess.STDOUT,
        "close_fds": True,
    }
    try:
        if IS_WINDOWS:
            flags = (
                subprocess.DETACHED_PROCESS  # type: ignore[attr-defined]
                | subprocess.CREATE_NEW_PROCESS_GROUP  # type: ignore[attr-defined]
                | subprocess.CREATE_NO_WINDOW  # type: ignore[attr-defined]
            )
            try:
                return subprocess.Popen(
                    command.argv,
                    creationflags=flags | subprocess.CREATE_BREAKAWAY_FROM_JOB,  # type: ignore[attr-defined]
                    **kwargs,
                )
            except OSError:
                # A job that forbids breakaway: still detached from the console.
                return subprocess.Popen(command.argv, creationflags=flags, **kwargs)
        return subprocess.Popen(command.argv, start_new_session=True, **kwargs)
    finally:
        log.close()


class ServiceController:
    """The background service as one thing, whatever runs it."""

    def __init__(
        self,
        *,
        endpoint: str | None = None,
        systemd: SystemdUser | None = None,
        login_items: LoginItems | None = None,
        spawn: Callable[[ServiceCommand, Path], Any] = spawn_detached,
        status: Callable[[str], Mapping[str, Any] | None] | None = None,
        shutdown: Callable[[str], Any] | None = None,
        sleep: Sleep = time.sleep,
    ) -> None:
        self.endpoint = endpoint or service_endpoint()
        self.systemd = systemd if systemd is not None else SystemdUser()
        self.login_items = login_items if login_items is not None else LoginItems()
        self._spawn = spawn
        self._status = status or (lambda ep: probe.backend_status(ep, timeout=1.0))
        self._shutdown = shutdown or (lambda ep: probe.request("system.shutdown", endpoint=ep))
        self._sleep = sleep
        self._systemd_ok: bool | None = None
        self._process: Any = None  # a detached service started here
        self._started_at = 0.0

    # -- queries -------------------------------------------------------

    def status(self) -> Mapping[str, Any] | None:
        """``system.status`` of the running backend, or None when stopped."""
        return self._status(self.endpoint)

    def running(self) -> bool:
        return self.status() is not None

    def uses_systemd(self) -> bool:
        """systemd runs this service: a user manager is available, and its
        unit is not already another endpoint's (one unit per user; a service
        on another endpoint, such as a test's, must never rewrite or start
        the user's own)."""
        if self._systemd_ok is None:
            available = self.systemd.available()
            unit_endpoint = self.systemd.unit_endpoint() if available else None
            if unit_endpoint is not None and unit_endpoint != self.endpoint:
                logger.info(
                    "The systemd unit serves %s, not %s: running this service as a"
                    " background process instead",
                    unit_endpoint,
                    self.endpoint,
                )
                available = False
            self._systemd_ok = available
        return self._systemd_ok

    def describe(self) -> dict[str, Any]:
        """For ``neurowatch service status``."""
        status = self.status()
        systemd = self.uses_systemd() and self.systemd.installed()
        if systemd and (status is None or self.systemd.active()):
            manager = "systemd user service"
        elif IS_WINDOWS:
            manager = "per-user background process"
        else:
            manager = "background process"
        return {
            "running": status is not None,
            "endpoint": self.endpoint,
            "manager": manager,
            "mode": running_mode(status) if status else saved_mode(),
            "dev": source_root() is not None,
            "starts_at_login": self.starts_at_login(),
            "starts_at_boot": saved_mode() == HEADLESS
            and self.uses_systemd()
            and self.starts_at_login()
            and self.systemd.lingering(),
            "status": dict(status) if status else None,
        }

    def starts_at_login(self) -> bool:
        if self.uses_systemd():
            return self.systemd.installed() and self.systemd.enabled()
        return self.login_items.installed()

    # -- install -------------------------------------------------------

    def install(self, *, autostart: bool = True, mode: str | None = None) -> None:
        """Register the service in ``mode`` (default: the last one); with
        ``autostart`` and the user's startup choice (``startup.StartupPrefs``)
        it starts at every login (headless, or "record from boot": at boot),
        with the desktop mode's tray or window. Otherwise nothing is
        registered."""
        prefs = load_prefs()
        if not autostart or not prefs.at_login:
            return
        mode = mode or saved_mode()
        command = service_command(self.endpoint, mode)
        if self.uses_systemd():
            self.systemd.install(command, enable=True)
            if mode == HEADLESS or prefs.at_boot:
                self.systemd.enable_linger()
        if mode == DESKTOP:
            self.login_items.install(
                ui_command()
                if prefs.open_window
                else python_command("neurowatch.tray", windowless=True)
            )

    def apply_startup(self, prefs: StartupPrefs) -> list[str]:
        """Save the user's startup choices and register (or unregister) the
        service to match. Returns what could not be done, in plain words."""
        save_prefs(prefs)
        problems: list[str] = []
        try:
            if prefs.at_login:
                self.install(autostart=True)
            else:
                self.uninstall()
        except (OSError, ServiceError) as error:
            problems.append(f"Could not change what starts at sign-in: {error}")
        if IS_WINDOWS or not self.uses_systemd():
            return problems
        user = getpass.getuser()
        if prefs.at_login and prefs.at_boot:
            if not self.systemd.enable_linger():
                problems.append(
                    "Recording from boot needs permission once: run "
                    f"sudo loginctl enable-linger {user}"
                )
        elif saved_mode() != HEADLESS and not self.systemd.disable_linger():
            # Headless mode starts at boot by design; leave it.
            problems.append(
                f"Could not stop starting at boot: run sudo loginctl disable-linger {user}"
            )
        return problems

    def uninstall(self) -> None:
        """Stop starting at login (the running service keeps running)."""
        if self.uses_systemd():
            self.systemd.uninstall()
        self.login_items.uninstall()

    # -- lifecycle -----------------------------------------------------

    def ensure_running(
        self, *, autostart: bool = True, mode: str | None = None, timeout: float | None = None
    ) -> bool:
        """Start the service unless it is running; True once it answers.

        ``mode`` (desktop / headless) is the one asked for: a service running
        in the other mode is restarted in it. None accepts either and starts
        the last mode asked for (the tray at login).

        One process at a time starts it (a lock), so the UI and the tray
        opening together never start two backends.
        """
        lock = InstanceLock(lock_path("service-start"))
        deadline = time.monotonic() + (timeout if timeout is not None else self._start_timeout())
        while not lock.acquire():
            status = self.status()
            if status is not None and mode in (None, running_mode(status)):
                return True
            if time.monotonic() > deadline:
                return False
            self._sleep(0.25)
        try:
            if mode is not None:
                save_mode(mode)
            wanted = mode or saved_mode()
            status = self.status()
            if status is not None:
                self._register(autostart, wanted)
                if mode is None or running_mode(status) == mode:
                    return True
                logger.info("Restarting the NeuroWatch service in %s mode", mode)
                if not self.stop():
                    raise ServiceError("The service did not stop to change its mode")
            else:
                self._register(autostart, wanted)
            self._start(wanted)
            return self._wait(lambda: self.running(), deadline, starting=True)
        finally:
            lock.release()

    def start(self, *, timeout: float | None = None, mode: str | None = None) -> bool:
        return self.ensure_running(autostart=False, mode=mode, timeout=timeout)

    def stop(self, *, timeout: float = STOP_TIMEOUT_S) -> bool:
        """Stop the service; True once it no longer answers (and, in
        development, its supervisor has exited, so a start right after
        never finds the old one still finishing)."""
        deadline = time.monotonic() + timeout
        if self.uses_systemd() and self.systemd.installed() and self.systemd.active():
            self.systemd.stop()
        elif self.running():
            self._shutdown(self.endpoint)
        if not self._wait(lambda: not self.running(), deadline):
            return False
        if source_root() is not None:
            return self._wait(_dev_supervisor_gone, deadline)
        return True

    def restart(self, *, mode: str | None = None) -> bool:
        if not self.stop():
            raise ServiceError("The service did not stop")
        return self.start(mode=mode)

    # -- internals -----------------------------------------------------

    def _register(self, autostart: bool, mode: str) -> None:
        try:
            self.install(autostart=autostart, mode=mode)
        except (OSError, ServiceError) as error:
            # Starting at login is a convenience: the service still runs now.
            logger.warning("Could not register NeuroWatch to start at login: %s", error)

    def _start(self, mode: str) -> None:
        command = service_command(self.endpoint, mode)
        if self.uses_systemd() and self.systemd.installed():
            # The unit is rewritten if this checkout or bundle moved.
            self.systemd.install(command, enable=False)
            logger.info("Starting %s", self.systemd.unit_name)
            self._started_at = time.time()
            self.systemd.start()
            return
        log = log_path("service")
        logger.info("Starting NeuroWatch service: %s (log %s)", shlex.join(command.argv), log)
        self._started_at = time.time()
        self._process = self._spawn(command, log)

    def _start_timeout(self) -> float:
        return DEV_START_TIMEOUT_S if source_root() is not None else START_TIMEOUT_S

    def _wait(self, done: Callable[[], bool], deadline: float, *, starting: bool = False) -> bool:
        while True:
            if done():
                return True
            if self.uses_systemd() and self.systemd.installed() and self.systemd.failed():
                return done()
            if starting:
                failure = self._start_failure()
                if failure:
                    raise ServiceError(f"{failure} See the log: {log_path('service')}")
            if time.monotonic() > deadline:
                return False
            self._sleep(0.25)

    def _start_failure(self) -> str | None:
        """Why a start just made cannot succeed, if that is already known."""
        process = self._process
        poll = getattr(process, "poll", None)
        if callable(poll):
            code = poll()
            if code is not None:
                return f"The service exited (status {code})."
        if source_root() is None:
            return None
        from neurowatch.service.dev import FAILED_PHASES, read_state

        state = read_state()
        at = state.get("at")
        if state.get("phase") in FAILED_PHASES and isinstance(at, float) and at >= self._started_at:
            if state["phase"] == "build-failed":
                return "The backend did not build."
            return f"The backend exited (status {state.get('code')})."
        return None


def _dev_supervisor_gone() -> bool:
    lock = InstanceLock(lock_path("dev-supervisor"))
    if not lock.acquire():
        return False
    lock.release()
    return True


def default_controller(sleep: Sleep = time.sleep) -> ServiceController:
    return ServiceController(sleep=sleep)
