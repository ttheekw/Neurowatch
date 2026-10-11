"""Development service: build, run, and rebuild the backend when it changes.

In a source checkout the background service is this supervisor instead of a
release binary. It builds ``nw-backend`` (debug), runs it, and polls the
backend's sources (``crates/``, the workspace ``Cargo.toml`` and
``Cargo.lock``) once a second. ``NW_BACKEND_WORKSPACE`` and
``NW_BACKEND_PACKAGE`` build another workspace's backend instead (one that
adds extensions); its sources are watched too. After a change settles it rebuilds; a
successful build replaces the running backend (stopped gracefully first, so
two never share the IPC socket), a failed one leaves it running.

The supervisor exits when its backend stops on request (``system.shutdown``
from the tray or ``neurowatch service stop``, exit status 0) or on SIGTERM /
SIGINT. A backend that crashes is restarted after a growing pause.
"""

from __future__ import annotations

import json
import logging
import os
import re
import shutil
import signal
import subprocess
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

from neurowatch.service import probe
from neurowatch.service.lock import InstanceLock
from neurowatch.service.paths import (
    backend_package,
    backend_workspace,
    dev_backend,
    lock_path,
    service_endpoint,
    state_dir,
)

logger = logging.getLogger(__name__)

POLL_S = 1.0
#: A change must stay quiet this long before a rebuild (editors save in bursts).
SETTLE_S = 1.0
STOP_TIMEOUT_S = 30.0
#: A previous supervisor may still be finishing (a restart, a mode change).
LOCK_WAIT_S = 30.0
CRASH_BACKOFF_S = (2.0, 5.0, 10.0, 30.0)
MIN_CLANG_MAJOR = 21  # backends that link libwebrtc need clang >= 21
_SOURCE_SUFFIXES = {".rs", ".toml", ".proto", ".sql", ".json"}
_SKIP_DIRS = {"target", ".git", "node_modules", "__pycache__"}

Snapshot = dict[str, tuple[int, int]]
#: What the supervisor is doing, for the service manager waiting on a start.
STATE_FILE = "dev-service.json"
FAILED_PHASES = ("crashed", "build-failed")


def write_state(phase: str, **detail: object) -> None:
    """Record the supervisor's phase (building, running, crashed, ...)."""
    path = state_dir() / STATE_FILE
    try:
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps({"phase": phase, "at": time.time(), **detail}))
        temporary.replace(path)
    except OSError as error:
        logger.debug("Could not record the dev service state: %s", error)


def read_state() -> dict[str, object]:
    try:
        state = json.loads((state_dir() / STATE_FILE).read_text())
    except (OSError, ValueError):
        return {}
    return state if isinstance(state, dict) else {}


def source_snapshot(root: Path) -> Snapshot:
    """Size and modification time of every file the backend is built from."""
    snapshot: Snapshot = {}
    for path in _source_files(root):
        try:
            stat = path.stat()
        except OSError:
            continue
        snapshot[str(path)] = (stat.st_mtime_ns, stat.st_size)
    return snapshot


def _source_files(root: Path) -> Iterator[Path]:
    for name in ("Cargo.toml", "Cargo.lock"):
        path = root / name
        if path.is_file():
            yield path
    stack = [root / "crates"]
    while stack:
        directory = stack.pop()
        try:
            entries = list(os.scandir(directory))
        except OSError:
            continue
        for entry in entries:
            if entry.is_dir(follow_symlinks=False):
                if entry.name not in _SKIP_DIRS:
                    stack.append(Path(entry.path))
            elif Path(entry.name).suffix in _SOURCE_SUFFIXES:
                yield Path(entry.path)


def changed_files(before: Snapshot, after: Snapshot) -> list[str]:
    keys = before.keys() | after.keys()
    return sorted(key for key in keys if before.get(key) != after.get(key))


def select_cxx(env: dict[str, str]) -> None:
    """Prefer clang >= 21 unless ``CXX`` is set: backends with C++ dependencies
    built against libc++ (libwebrtc) need it; others build with any compiler."""
    if env.get("CXX"):
        return
    for candidate in (f"clang++-{MIN_CLANG_MAJOR}", "clang++-22", "clang++-23", "clang++"):
        path = shutil.which(candidate, path=env.get("PATH"))
        if path is None:
            continue
        try:
            version = subprocess.run(
                [path, "--version"], capture_output=True, text=True, timeout=10
            ).stdout
        except (OSError, subprocess.TimeoutExpired):
            continue
        match = re.search(r"clang version (\d+)", version)
        if match and int(match.group(1)) >= MIN_CLANG_MAJOR:
            env["CXX"] = candidate
            return


class Process(Protocol):
    def poll(self) -> int | None: ...
    def wait(self, timeout: float | None = None) -> int: ...
    def terminate(self) -> None: ...
    def kill(self) -> None: ...


@dataclass
class DevSupervisor:
    root: Path
    endpoint: str = field(default_factory=service_endpoint)
    build: Callable[[], bool] | None = None
    launch: Callable[[], Process] | None = None
    request_shutdown: Callable[[str], Any] | None = None
    snapshot: Callable[[Path], Snapshot] = source_snapshot
    sleep: Callable[[float], None] = time.sleep
    clock: Callable[[], float] = time.monotonic

    def __post_init__(self) -> None:
        # NW_HEADLESS comes from the service's mode (manager.service_command).
        self._env = {**os.environ, "NW_IPC_ENDPOINT": self.endpoint}
        self._backend: Process | None = None
        self._stopping = False
        self._crashes = 0
        self._workspace = backend_workspace(self.root)
        self._build = self.build or self._cargo_build
        self._launch = self.launch or self._launch_backend
        self._request_shutdown = self.request_shutdown or (
            lambda ep: probe.request("system.shutdown", endpoint=ep)
        )

    # -- the loop ------------------------------------------------------

    def stop(self, *_args: object) -> None:
        """Stop the backend and the supervisor (signal handler)."""
        self._stopping = True

    def run(self) -> int:
        lock = InstanceLock(lock_path("dev-supervisor"))
        deadline = self.clock() + LOCK_WAIT_S
        while not lock.acquire():
            if self.clock() > deadline:
                logger.error("Another NeuroWatch dev supervisor is already running")
                return 1
            self.sleep(0.2)
        signal.signal(signal.SIGTERM, self.stop)
        signal.signal(signal.SIGINT, self.stop)
        try:
            return self._loop()
        finally:
            self._stop_backend()
            write_state("stopped")
            lock.release()

    def _loop(self) -> int:
        seen = self._sources()
        if self._build() or dev_backend(self._workspace).is_file():
            self._start_backend()
        else:
            logger.error("The backend did not build; waiting for a change to retry")
        pending_since: float | None = None
        retry_at: float | None = None
        while not self._stopping:
            self.sleep(POLL_S)
            if self._stopping:
                break
            current = self._sources()
            changes = changed_files(seen, current)
            if changes:
                seen = current
                pending_since = self.clock()
                logger.info("Backend source changed: %s", ", ".join(_short(self.root, changes)))
            if pending_since is not None and self.clock() - pending_since >= SETTLE_S:
                pending_since = None
                self._rebuild()
                retry_at = None
                continue
            backend = self._backend
            if backend is None:
                if retry_at is not None and self.clock() >= retry_at:
                    retry_at = None
                    self._start_backend()
                continue
            code = backend.poll()
            if code is None:
                continue
            self._backend = None
            if code == 0:
                logger.info("Backend stopped on request; dev supervisor exiting")
                return 0
            pause = CRASH_BACKOFF_S[min(self._crashes, len(CRASH_BACKOFF_S) - 1)]
            self._crashes += 1
            write_state("crashed", code=code)
            logger.warning("Backend exited with status %s; restarting in %.0f s", code, pause)
            retry_at = self.clock() + pause
        return 0

    def _rebuild(self) -> None:
        logger.info("Rebuilding the backend")
        if not self._build():
            logger.error("Build failed; the running backend is kept")
            return
        logger.info("Build succeeded; restarting the backend")
        self._stop_backend()
        self._crashes = 0
        self._start_backend()

    # -- backend process -----------------------------------------------

    def _start_backend(self) -> None:
        try:
            self._backend = self._launch()
        except OSError as error:
            logger.error("Could not start the backend: %s", error)
            self._backend = None
            write_state("crashed", code=None)
            return
        write_state("running")

    def _stop_backend(self) -> None:
        backend, self._backend = self._backend, None
        if backend is None or backend.poll() is not None:
            return
        self._request_shutdown(self.endpoint)
        try:
            backend.wait(timeout=STOP_TIMEOUT_S)
            return
        except subprocess.TimeoutExpired:
            logger.warning("Backend did not stop in %.0f s; terminating it", STOP_TIMEOUT_S)
        backend.terminate()
        try:
            backend.wait(timeout=5)
        except subprocess.TimeoutExpired:
            backend.kill()
            backend.wait()

    def _sources(self) -> Snapshot:
        """The backend's sources: the checkout's, and the build workspace's."""
        snapshot = self.snapshot(self.root)
        if self._workspace != self.root:
            snapshot = {**snapshot, **self.snapshot(self._workspace)}
        return snapshot

    def _cargo_build(self) -> bool:
        write_state("building")
        ok = self._run_cargo()
        if not ok:
            write_state("build-failed")
        return ok

    def _run_cargo(self) -> bool:
        select_cxx(self._env)
        command = ["cargo", "build", "-p", backend_package()]
        logger.info("$ %s (CXX=%s)", " ".join(command), self._env.get("CXX", "default"))
        try:
            result = subprocess.run(command, cwd=self._workspace, env=self._env, check=False)
        except OSError as error:
            logger.error("cargo is not available: %s", error)
            return False
        return result.returncode == 0

    def _launch_backend(self) -> Process:
        binary = dev_backend(self._workspace)
        logger.info("Starting %s", binary)
        return subprocess.Popen([str(binary)], cwd=self.root, env=self._env)


def _short(root: Path, paths: list[str]) -> list[str]:
    shown = []
    for path in paths[:5]:
        try:
            shown.append(str(Path(path).relative_to(root)))
        except ValueError:
            shown.append(path)
    if len(paths) > 5:
        shown.append(f"… {len(paths) - 5} more")
    return shown
