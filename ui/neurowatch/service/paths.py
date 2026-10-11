"""Where the background service's pieces live, per platform and per mode.

*Packaged* (a PyInstaller build): the backend binary ships next to the UI in
the release bundle (``<base>/bin/nw-backend`` and
``<base>/app/neurowatch-ui/neurowatch-ui``).

*Development* (a source checkout, the package installed editable): the
service is the dev supervisor (``neurowatch.service.dev``), which builds and
runs ``target/debug/nw-backend`` and restarts it when the backend's source
changes.
"""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

#: systemd user unit (Linux) and startup entry (Windows) name.
SERVICE_NAME = "neurowatch-backend"
IS_WINDOWS = sys.platform == "win32"
_WINDOWS_ENDPOINT = "tcp:127.0.0.1:7676"
_BOGUS_ENDPOINT = "unix:/control.sock"  # an unexpanded launcher default


def is_frozen() -> bool:
    return bool(getattr(sys, "frozen", False))


def source_root() -> Path | None:
    """The source checkout this package runs from (development mode), or None.

    ``NW_SOURCE_ROOT`` overrides the detection (for example to test packaged
    behaviour from a checkout: point it at a directory without ``crates``).
    """
    override = os.environ.get("NW_SOURCE_ROOT")
    if override is not None:
        root = Path(override)
    elif is_frozen():
        return None
    else:
        root = Path(__file__).resolve().parents[3]  # ui/neurowatch/service/paths.py
    if (root / "Cargo.toml").is_file() and (root / "crates" / "nw-backend").is_dir():
        return root
    return None


def dev_mode() -> bool:
    return source_root() is not None


def service_endpoint() -> str:
    """The IPC endpoint the service listens on and every client uses."""
    configured = os.environ.get("NW_IPC_ENDPOINT")
    if configured and configured != _BOGUS_ENDPOINT:
        return configured
    if IS_WINDOWS:
        return _WINDOWS_ENDPOINT
    runtime = os.environ.get("XDG_RUNTIME_DIR") or "/tmp"
    return f"unix:{runtime}/neurowatch/control.sock"


def state_dir() -> Path:
    """Per-user directory for the service's logs and lock files."""
    if IS_WINDOWS:
        base = Path(os.environ.get("LOCALAPPDATA") or Path.home() / "AppData" / "Local")
        path = base / "Neurowatch"
    else:
        base = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local" / "state")
        path = base / "neurowatch"
    path.mkdir(parents=True, exist_ok=True)
    return path


def log_path(name: str) -> Path:
    logs = state_dir() / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    return logs / f"{name}.log"


def lock_path(name: str) -> Path:
    return state_dir() / f"{name}.lock"


def packaged_backend() -> Path | None:
    """The backend binary of a release bundle, or ``NW_BACKEND_BIN``."""
    override = os.environ.get("NW_BACKEND_BIN")
    if override:
        return Path(override)
    name = "nw-backend.exe" if IS_WINDOWS else "nw-backend"
    exe = Path(sys.executable).resolve()
    for base in (exe.parent.parent.parent, exe.parent.parent, exe.parent):
        candidate = base / "bin" / name
        if candidate.is_file():
            return candidate
    found = shutil.which(name)
    return Path(found) if found else None


def backend_workspace(root: Path) -> Path:
    """The Cargo workspace the development service builds the backend in:
    ``NW_BACKEND_WORKSPACE`` (a workspace that adds extensions to the backend,
    with this checkout as a dependency), else the checkout itself."""
    override = os.environ.get("NW_BACKEND_WORKSPACE")
    return Path(override) if override else root


def backend_package() -> str:
    """The Cargo package of the development backend (``NW_BACKEND_PACKAGE``)."""
    return os.environ.get("NW_BACKEND_PACKAGE") or "nw-backend"


def dev_backend(workspace: Path) -> Path:
    """The development backend binary built in ``workspace``."""
    name = "nw-backend.exe" if IS_WINDOWS else "nw-backend"
    target = os.environ.get("CARGO_TARGET_DIR")
    return (Path(target) if target else workspace / "target") / "debug" / name


def python_command(module: str, *args: str, windowless: bool = False) -> list[str]:
    """Run a ``neurowatch`` module with this interpreter, or the packaged
    executable's matching mode (``neurowatch-ui --tray`` and so on)."""
    if is_frozen():
        flag = {"neurowatch.tray": ["--tray"], "neurowatch.service": ["service"]}.get(module, [])
        return [sys.executable, *flag, *args]
    python = sys.executable
    if windowless and IS_WINDOWS:
        pythonw = Path(python).with_name("pythonw.exe")
        if pythonw.is_file():
            python = str(pythonw)
    return [python, "-m", module, *args]


def ui_command() -> list[str]:
    """Start the desktop UI."""
    if is_frozen():
        return [sys.executable]
    return python_command("neurowatch.main", windowless=True)
