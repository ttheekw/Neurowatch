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
import stat
import sys
from collections.abc import Mapping
from pathlib import Path

from neurowatch.utils.private_files import private_dir

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
    An installed (frozen) build is never a checkout and ignores the override.
    """
    if is_frozen():
        return None
    override = os.environ.get("NW_SOURCE_ROOT")
    if override is not None:
        root = Path(override)
    else:
        root = Path(__file__).resolve().parents[3]  # ui/neurowatch/service/paths.py
    if (root / "Cargo.toml").is_file() and (root / "crates" / "nw-backend").is_dir():
        return root
    return None


def dev_mode() -> bool:
    return source_root() is not None


def service_endpoint() -> str:
    """The IPC endpoint the service listens on and every client uses.

    Without ``XDG_RUNTIME_DIR`` (some SSH servers open sessions without it)
    the endpoint is in the user's login runtime
    folder, ``/run/user/<uid>``, when there is one, so every session of the
    user names the same service. A service still running, or registered with
    systemd, on the older fallback in ``/tmp`` keeps it.
    """
    configured = os.environ.get("NW_IPC_ENDPOINT")
    if configured and configured != _BOGUS_ENDPOINT:
        return configured
    if IS_WINDOWS:
        return _WINDOWS_ENDPOINT
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if runtime:
        return _unix_endpoint(runtime)
    legacy = _unix_endpoint("/tmp")
    login_runtime = login_runtime_dir()
    if login_runtime is None:
        return tmp_fallback_endpoint()
    endpoint = _unix_endpoint(str(login_runtime))
    if _socket_exists(legacy) and not _socket_exists(endpoint):
        return legacy
    if read_unit_endpoint(systemd_unit_path()) == legacy and not _socket_exists(endpoint):
        return legacy
    return endpoint


def _unix_endpoint(runtime: str) -> str:
    return f"unix:{runtime}/neurowatch/control.sock"


def tmp_fallback_endpoint(tmp: str = "/tmp") -> str:
    """The last resort without a runtime folder (the backend's
    ``tmp_fallback_endpoint`` makes the same choice). ``/tmp`` is shared, so
    another user can create ``/tmp/neurowatch`` first: it stays in use only
    when it is a real folder of this user that nobody else can write
    (existing installs); otherwise the endpoint is
    ``/tmp/neurowatch-<uid>/control.sock``, a folder the backend creates 0700
    and refuses to use unless it is this user's own."""
    legacy = f"{tmp}/neurowatch"
    try:
        info = os.lstat(legacy)
    except OSError:
        info = None
    uid = os.geteuid()
    if (
        info is not None
        and stat.S_ISDIR(info.st_mode)
        and info.st_uid == uid
        and stat.S_IMODE(info.st_mode) & 0o022 == 0
    ):
        return f"unix:{legacy}/control.sock"
    return f"unix:{tmp}/neurowatch-{uid}/control.sock"


def _socket_exists(endpoint: str) -> bool:
    """A socket of this user's in a folder only this user can change. The
    legacy `/tmp/neurowatch` folder is shared: a socket another user put
    there made the service manager adopt their endpoint, hand them the link
    code, and register a unit the backend then refused to start with
    (vulnerability checklist R9)."""
    path = endpoint.removeprefix("unix:")
    try:
        found = os.lstat(path)
        folder = os.lstat(os.path.dirname(path) or ".")
    except OSError:
        return False
    uid = os.geteuid()
    return (
        stat.S_ISSOCK(found.st_mode)
        and found.st_uid == uid
        and stat.S_ISDIR(folder.st_mode)
        and folder.st_uid == uid
        and stat.S_IMODE(folder.st_mode) & 0o022 == 0
    )


def login_runtime_dir(uid: int | None = None, root: Path = Path("/run/user")) -> Path | None:
    """``/run/user/<uid>`` when it is this user's own folder (systemd-logind
    makes it for every login), for sessions started without
    ``XDG_RUNTIME_DIR``; None elsewhere (Windows, systems without logind)."""
    if IS_WINDOWS:
        return None
    if uid is None:
        uid = os.getuid()
    path = root / str(uid)
    try:
        info = path.stat()
    except OSError:
        return None
    if not path.is_dir() or info.st_uid != uid or not os.access(path, os.W_OK | os.X_OK):
        return None
    return path


def systemd_unit_path() -> Path:
    """Where the service's systemd user unit is written (Linux)."""
    config = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    return config / "systemd" / "user" / f"{SERVICE_NAME}.service"


def read_unit_endpoint(unit: Path) -> str | None:
    """The IPC endpoint a written systemd unit starts the service on."""
    try:
        text = unit.read_text()
    except OSError:
        return None
    prefix = 'Environment="NW_IPC_ENDPOINT='
    for line in text.splitlines():
        if line.startswith(prefix) and line.endswith('"'):
            return line[len(prefix) : -1].replace("%%", "%")
    return None


def host_environment(
    environ: Mapping[str, str] | None = None, *, frozen: bool | None = None
) -> dict[str, str]:
    """The environment for the system's own programs (``systemctl``,
    ``loginctl``, the desktop's URL opener).

    A packaged build (PyInstaller) runs with ``LD_LIBRARY_PATH`` pointing at
    its bundled libraries, and every program it starts inherits that. Those
    libraries come from the build machine: a newer system's ``systemctl``
    then loads the bundle's older OpenSSL and fails to start (``version
    'OPENSSL_3.4.0' not found``, Debian 13), and NeuroWatch took that to
    mean the machine had no systemd. This restores the library path the
    program was started with (PyInstaller keeps it as
    ``LD_LIBRARY_PATH_ORIG``), or removes it when there was none. Unchanged
    outside a packaged build and on Windows.
    """
    env = dict(os.environ if environ is None else environ)
    if IS_WINDOWS or not (is_frozen() if frozen is None else frozen):
        return env
    original = env.pop("LD_LIBRARY_PATH_ORIG", None)
    if original:
        env["LD_LIBRARY_PATH"] = original
    else:
        env.pop("LD_LIBRARY_PATH", None)
    return env


def restore_host_environment(*, frozen: bool | None = None) -> None:
    """Give every program this process starts the system's own library path
    (see ``host_environment``); this process keeps its own, which the dynamic
    loader read when it started. Called once at start-up by the packaged
    executable, so Qt's URL and folder openers get it too."""
    wanted = host_environment(frozen=frozen)
    for name in ("LD_LIBRARY_PATH", "LD_LIBRARY_PATH_ORIG"):
        if name in wanted:
            os.environ[name] = wanted[name]
        else:
            os.environ.pop(name, None)


def state_dir() -> Path:
    """Per-user directory for the service's logs and lock files."""
    if IS_WINDOWS:
        base = Path(os.environ.get("LOCALAPPDATA") or Path.home() / "AppData" / "Local")
        path = base / "Neurowatch"
    else:
        base = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local" / "state")
        path = base / "neurowatch"
    # Logs name cameras and their addresses: owner only.
    return private_dir(path)


def log_path(name: str) -> Path:
    logs = private_dir(state_dir() / "logs")
    return logs / f"{name}.log"


def lock_path(name: str) -> Path:
    return state_dir() / f"{name}.lock"


def packaged_backend() -> Path | None:
    """The backend binary of a release bundle.

    A frozen (installed) build runs only the backend bundled with it: neither
    ``NW_BACKEND_BIN`` nor ``PATH`` can substitute another program for the
    service. Other runs (a checkout) keep ``NW_BACKEND_BIN`` and ``PATH``.
    """
    frozen = is_frozen()
    override = None if frozen else os.environ.get("NW_BACKEND_BIN")
    if override:
        return Path(override)
    name = "nw-backend.exe" if IS_WINDOWS else "nw-backend"
    exe = Path(sys.executable).resolve()
    for base in (exe.parent.parent.parent, exe.parent.parent, exe.parent):
        candidate = base / "bin" / name
        if candidate.is_file():
            return candidate
    if frozen:
        return None
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
