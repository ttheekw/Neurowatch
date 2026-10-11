"""Headless NeuroWatch: command-line status, no GUI.

``./neurowatch.sh --headless`` makes sure the background service runs in
headless mode (starting it, and by default registering it to start at boot;
see ``neurowatch.service``), then runs this module's ``monitor``: it prints
timestamped lines when the backend comes up, cameras change state, or
recording starts or stops, plus a periodic summary. Ctrl+C ends the monitor;
the service keeps running. (``--foreground`` instead runs the backend in the
launcher's terminal, as before.) ``status`` prints the same once, from any
terminal. Extensions (``neurowatch.extensions``) can add status lines,
monitor options and commands.

This module must not import Qt, so it also runs on servers without a display.
"""

from __future__ import annotations

import argparse
import logging
import os
import shutil
import sys
import time
from collections import Counter
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any, TextIO

from neurowatch import extensions
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.camera import connection_status

Rpc = Callable[[str, dict[str, Any], float], Any]

_STATUS_TIMEOUT_S = 5.0
# Past this many camera changes in one poll, print a summary instead of a
# line per camera (a 128-camera wall coming up would otherwise flood).
_MAX_CAMERA_LINES = 5
_LOST_AFTER_FAILURES = 3
_STATE_ORDER = ("streaming", "connecting", "reconnecting", "error", "offline")
_DECODERS = {
    "vaapi": "VA-API hardware",
    "nvdec": "NVIDIA NVDEC hardware",
    "v4l2_m2m": "V4L2 hardware",
    "software": "CPU (software)",
    "blocked": "blocked (GStreamer update required)",
}


# ---------------------------------------------------------------------------
# Snapshot and presentation (pure; unit-tested without a backend)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Snapshot:
    """One poll of the backend."""

    statuses: dict[str, str]
    names: dict[str, str] = field(default_factory=dict)
    recording: dict[str, str] = field(default_factory=dict)
    # One "Label: text" line per extension (see ``neurowatch.extensions``).
    extras: tuple[str, ...] = ()

    def name(self, cam_id: str) -> str:
        return self.names.get(cam_id) or cam_id

    def counts(self) -> Counter[str]:
        return Counter(connection_status(state) for state in self.statuses.values())

    def recording_count(self) -> int:
        return sum(1 for state in self.recording.values() if state == "recording")


def camera_summary(snapshot: Snapshot) -> str:
    counts = snapshot.counts()
    total = len(snapshot.statuses)
    if not total:
        return "Cameras: none configured"
    parts = [f"{counts[state]} {state}" for state in _STATE_ORDER if counts[state]]
    parts += [f"{n} {state}" for state, n in sorted(counts.items()) if state not in _STATE_ORDER]
    return f"Cameras: {' · '.join(parts)} (of {total})"


def status_line(snapshot: Snapshot, free_gb: float | None = None) -> str:
    counts = snapshot.counts()
    disk = f" · {free_gb:.1f} GB free" if free_gb is not None else ""
    extras = ""
    for line in snapshot.extras:
        label, _, text = line.partition(": ")
        extras += f" · {label.lower()} {text}" if text else f" · {line}"
    return (
        f"Status: {counts['streaming']}/{len(snapshot.statuses)} cameras streaming · "
        f"recording {snapshot.recording_count()}{disk}{extras}"
    )


def storage_path(config: Any) -> str | None:
    recording = config.get("recording") if isinstance(config, dict) else None
    path = recording.get("storage_path") if isinstance(recording, dict) else None
    return os.path.expanduser(path) if isinstance(path, str) and path else None


def free_gb(path: str | None) -> float | None:
    """Free space on the recordings disk (the nearest existing parent of ``path``)."""
    while path and not os.path.exists(path):
        parent = os.path.dirname(path)
        path = parent if parent != path else None
    if not path:
        return None
    try:
        return shutil.disk_usage(path).free / 1e9
    except OSError:
        return None


def recording_line(config: Any, snapshot: Snapshot) -> str:
    path = storage_path(config)
    free = free_gb(path)
    where = f" to {path}" if path else ""
    disk = f" ({free:.1f} GB free)" if free is not None else ""
    everything = (config.get("recording") or {}).get("local_all_cameras") is True
    scope = "all cameras" if everything else f"{snapshot.recording_count()} cameras"
    return f"Recording: {scope}{where}{disk}"


def describe_changes(previous: Snapshot | None, current: Snapshot) -> list[str]:
    """Lines worth printing for the step from ``previous`` to ``current``."""
    if previous is None:
        return [camera_summary(current), *current.extras]
    lines: list[str] = []
    changed = [
        (cam_id, previous.statuses.get(cam_id), state)
        for cam_id, state in current.statuses.items()
        if connection_status(previous.statuses.get(cam_id, "")) != connection_status(state)
    ]
    removed = [cam_id for cam_id in previous.statuses if cam_id not in current.statuses]
    if len(changed) + len(removed) <= _MAX_CAMERA_LINES:
        for cam_id, before, after in changed:
            old = connection_status(before) if before else "added"
            lines.append(f"Camera {current.name(cam_id)}: {old} → {connection_status(after)}")
        lines += [f"Camera {previous.name(cam_id)}: removed" for cam_id in removed]
    else:
        lines.append(f"{len(changed) + len(removed)} cameras changed state")
    if changed or removed:
        lines.append(camera_summary(current))
    if previous.recording_count() != current.recording_count():
        lines.append(f"Recording: {current.recording_count()} cameras")
    earlier = {line.partition(": ")[0]: line for line in previous.extras}
    lines += [line for line in current.extras if earlier.get(line.partition(": ")[0]) != line]
    return lines


def decoder_label(system: dict[str, Any]) -> str:
    info = system.get("video_acceleration")
    kind = str(info.get("kind", "unknown")).lower() if isinstance(info, dict) else "unknown"
    return _DECODERS.get(kind, "unknown")


# ---------------------------------------------------------------------------
# Backend access
# ---------------------------------------------------------------------------


def client_rpc(endpoint: str | None) -> Rpc:
    def call(method: str, params: dict[str, Any], timeout: float) -> Any:
        return SyncIpcClient(endpoint=endpoint).call(method, params, timeout=timeout)

    return call


def _optional(rpc: Rpc, method: str) -> Any:
    try:
        return rpc(method, {}, _STATUS_TIMEOUT_S)
    except Exception:
        return None


def poll(rpc: Rpc, names: dict[str, str]) -> Snapshot:
    """Read one snapshot. Only ``camera.status_all`` failing counts as the backend being down."""
    statuses = rpc("camera.status_all", {}, _STATUS_TIMEOUT_S)
    statuses = {k: v for k, v in (statuses or {}).items() if isinstance(v, str)}
    if any(cam_id not in names for cam_id in statuses):
        cameras = _optional(rpc, "camera.list")
        if isinstance(cameras, list):
            names.update(
                {
                    c["id"]: c.get("name") or c["id"]
                    for c in cameras
                    if isinstance(c, dict) and "id" in c
                }
            )
    recording = _optional(rpc, "recording.status_all")
    extras = extensions.call_each("status_line", rpc)
    return Snapshot(
        statuses=statuses,
        names=dict(names),
        recording={k: v for k, v in recording.items() if isinstance(v, str)}
        if isinstance(recording, dict)
        else {},
        extras=tuple(line for line in extras if isinstance(line, str) and line),
    )


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


class _Printer:
    def __init__(self, out: TextIO, clock: Callable[[], float] = time.time) -> None:
        self._out = out
        self._clock = clock

    def __call__(self, line: str) -> None:
        stamp = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(self._clock()))
        print(f"{stamp}  {line}", file=self._out, flush=True)


def monitor(
    rpc: Rpc,
    say: Callable[[str], None],
    *,
    interval: float = 2.0,
    heartbeat: float = 60.0,
    sleep: Callable[[float], None] = time.sleep,
    clock: Callable[[], float] = time.monotonic,
    max_polls: int | None = None,
    persistent: bool = False,
) -> int:
    """Print status changes until the backend stops answering. Returns an exit code.

    ``persistent`` (the background service): a backend that stops answering is
    waited for, not the end, since the service restarts it (after a crash, or
    a rebuild in development); Ctrl+C ends the monitor.
    """
    names: dict[str, str] = {}
    previous: Snapshot | None = None
    failures = 0
    last_status = clock()
    announced = False
    recordings: str | None = None
    polls = 0
    while max_polls is None or polls < max_polls:
        polls += 1
        try:
            current = poll(rpc, names)
        except Exception as exc:
            failures += 1
            if failures == _LOST_AFTER_FAILURES and persistent:
                say("Backend not responding; waiting for the service to bring it back")
                announced = False
                previous = None
            elif failures >= _LOST_AFTER_FAILURES and not persistent:
                say(f"Backend stopped responding: {exc}")
                return 1
            sleep(interval)
            continue
        if not announced:
            system = _optional(rpc, "system.status") or {}
            site = system.get("site_name") or "NeuroWatch"
            say(f"Backend ready: {site} · decoder {decoder_label(system)}")
            config = _optional(rpc, "config.get")
            recordings = storage_path(config)
            say(recording_line(config if isinstance(config, dict) else {}, current))
            announced = True
        failures = 0
        for line in describe_changes(previous, current):
            say(line)
        if clock() - last_status >= heartbeat:
            say(status_line(current, free_gb(recordings)))
            last_status = clock()
        previous = current
        sleep(interval)
    return 0


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="neurowatch-headless",
        description="NeuroWatch without the desktop UI: status.",
    )
    parser.add_argument(
        "--endpoint",
        default=os.environ.get("NW_IPC_ENDPOINT"),
        help="backend IPC endpoint (default: NW_IPC_ENDPOINT, else the standard socket)",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    watch = commands.add_parser("monitor", help="print status changes until the backend stops")
    watch.add_argument("--interval", type=float, default=2.0, help="seconds between polls")
    watch.add_argument("--heartbeat", type=float, default=60.0, help="seconds between summaries")
    watch.add_argument(
        "--service",
        action="store_true",
        help="start the background service in headless mode first; it outlives the monitor",
    )
    watch.add_argument(
        "--no-autostart",
        action="store_true",
        help="with --service: do not register it to start at boot / login",
    )
    commands.add_parser("status", help="print camera and recording status once")
    extensions.call_each("add_monitor_arguments", watch)
    extensions.call_each("add_commands", commands)
    return parser


def _start_headless_service(say: Callable[[str], None], *, autostart: bool) -> int:
    """Start (or switch) the background service in headless mode; 0 when it
    runs, else an exit code."""
    from neurowatch.service.manager import (
        HEADLESS,
        ServiceController,
        ServiceError,
        running_mode,
    )
    from neurowatch.service.paths import dev_mode, log_path

    service = ServiceController()
    status = service.status()
    if status is None:
        say(
            "Starting the NeuroWatch service (headless)"
            + (" — building the backend first" if dev_mode() else "")
        )
    elif running_mode(status) != HEADLESS:
        say("Restarting the NeuroWatch service in headless mode (no desktop UI)")
    try:
        running = service.ensure_running(autostart=autostart, mode=HEADLESS)
    except ServiceError as error:
        say(f"The NeuroWatch service did not start: {error}")
        return 1
    if not running:
        say(f"The NeuroWatch service did not start in time; see {log_path('service')}")
        return 1
    info = service.describe()
    if info["starts_at_boot"]:
        when = "starts at boot"
    elif info["starts_at_login"]:
        when = "starts at login"
    else:
        when = "runs until stopped or the next restart (--no-autostart)"
    say(f"Service running ({info['manager']}, headless); it {when}")
    return 0


def print_status(rpc: Rpc, say: Callable[[str], None]) -> int:
    """Cameras, recording and the extensions' lines, once. Returns an exit code."""
    try:
        snapshot = poll(rpc, {})
    except Exception as exc:
        say(_unreachable(exc))
        return 1
    say(camera_summary(snapshot))
    say(f"Recording: {snapshot.recording_count()} cameras")
    for line in snapshot.extras:
        say(line)
    return 0


def _unreachable(exc: Exception) -> str:
    detail = str(exc).removeprefix("Backend not reachable: ")
    return (
        f"Backend not reachable ({detail}). Is NeuroWatch running? "
        "Open NeuroWatch, or start it with: neurowatch service start"
    )


def main(argv: list[str] | None = None, out: TextIO = sys.stdout) -> int:
    args = _parser().parse_args(argv)
    # These commands report connection problems themselves, in plain words;
    # the IPC client's own retry warnings would only repeat them.
    logging.getLogger("neurowatch.ipc").setLevel(logging.CRITICAL)
    rpc = client_rpc(args.endpoint)
    say = _Printer(out)
    try:
        if args.command == "monitor":
            if args.service:
                started = _start_headless_service(say, autostart=not args.no_autostart)
                if started:
                    return started
            for _extension, before_monitor in extensions.hooks("before_monitor"):
                stop = before_monitor(args, rpc, say)
                if stop is not None:
                    return int(stop)
            try:
                return monitor(
                    rpc,
                    say,
                    interval=args.interval,
                    heartbeat=args.heartbeat,
                    persistent=args.service,
                )
            except KeyboardInterrupt:
                if args.service:
                    say(
                        "Monitor stopped; NeuroWatch keeps running in the background"
                        " (status: neurowatch service status, stop: neurowatch service stop)"
                    )
                    return 0
                raise
        if args.command == "status":
            return print_status(rpc, say)
        for _extension, run_command in extensions.hooks("run_command"):
            result = run_command(args, rpc, say)
            if result is not None:
                return int(result)
        say(f"Unknown command: {args.command}")
        return 2
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
