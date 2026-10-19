"""``neurowatch service ...``: control the background service from a terminal.

    status              is it running, how it is run, starts at login?
    start               start it (and register it to start at login)
    stop                stop it (cameras stop recording)
    restart             stop, then start
    install             register it (and the tray) to start at login
    uninstall           stop registering it; a running service keeps running
    supervise           development only: the dev supervisor itself

``--no-autostart`` with ``start``/``restart``: start for this session only,
registering nothing with systemd or Windows startup. ``--headless`` /
``--desktop`` choose the service's mode (default: the last one used).
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
from dataclasses import replace

from neurowatch.service.manager import DESKTOP, HEADLESS, ServiceController, ServiceError
from neurowatch.service.paths import log_path, source_root
from neurowatch.service.startup import load_prefs, save_prefs


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="neurowatch service", description="Control the NeuroWatch background service."
    )
    parser.add_argument(
        "command",
        choices=["status", "start", "stop", "restart", "install", "uninstall", "supervise"],
    )
    parser.add_argument(
        "--no-autostart",
        action="store_true",
        help="start for this session only; do not register with systemd / Windows startup",
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--headless",
        dest="mode",
        action="store_const",
        const=HEADLESS,
        help="run without a desktop UI (start/restart/install; starts at boot)",
    )
    mode.add_argument(
        "--desktop", dest="mode", action="store_const", const=DESKTOP, help="for the desktop UI"
    )
    parser.add_argument("--json", action="store_true", help="status as JSON")
    return parser


def main(argv: list[str] | None = None, controller: ServiceController | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.command == "supervise":
        return _supervise()
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    service = controller or ServiceController()
    try:
        return _run(service, args)
    except ServiceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


def _run(service: ServiceController, args: argparse.Namespace) -> int:
    autostart = not args.no_autostart
    if args.command == "status":
        info = service.describe()
        if args.json:
            print(json.dumps(info, indent=2))
        else:
            print(_status_text(info))
        return 0 if info["running"] else 3
    if args.command == "start":
        if service.ensure_running(autostart=autostart, mode=args.mode):
            print(f"NeuroWatch service is running ({service.endpoint})")
            return 0
        print(f"NeuroWatch service did not start; see {log_path('service')}", file=sys.stderr)
        return 1
    if args.command == "stop":
        if service.stop():
            print("NeuroWatch service stopped")
            return 0
        print("NeuroWatch service did not stop in time", file=sys.stderr)
        return 1
    if args.command == "restart":
        service.stop()
        if service.ensure_running(autostart=autostart, mode=args.mode):
            print("NeuroWatch service restarted")
            return 0
        print(f"NeuroWatch service did not start; see {log_path('service')}", file=sys.stderr)
        return 1
    if args.command == "install":
        save_prefs(replace(load_prefs(), at_login=True))
        service.install(autostart=True, mode=args.mode)
        info = service.describe()
        if info["mode"] == HEADLESS:
            when = "at boot" if info["starts_at_boot"] else "at login"
            print(f"NeuroWatch service (headless) starts {when}")
        else:
            print("NeuroWatch service and tray start at login")
        return 0
    save_prefs(replace(load_prefs(), at_login=False))
    service.uninstall()
    print("NeuroWatch no longer starts at login (a running service keeps running)")
    return 0


def _status_text(info: dict) -> str:
    lines = [
        f"NeuroWatch service: {'running' if info['running'] else 'stopped'}",
        f"  endpoint:        {info['endpoint']}",
        f"  run as:          {info['manager']}{' (development build)' if info['dev'] else ''}",
        f"  mode:            {info['mode']}",
        f"  starts at login: {'yes' if info['starts_at_login'] else 'no'}"
        + (" (and at boot)" if info.get("starts_at_boot") else ""),
    ]
    status = info.get("status") or {}
    if status:
        lines.append(f"  site:            {status.get('site_name', '')}")
        lines.append(f"  cameras:         {status.get('cameras', 0)}")
        if "pid" in status:
            lines.append(f"  pid:             {status['pid']}")
        if "version" in status:
            lines.append(f"  version:         {status['version']}")
    return "\n".join(lines)


def _supervise() -> int:
    from neurowatch.service.dev import DevSupervisor

    root = source_root()
    if root is None:
        print("error: supervise runs only from a source checkout", file=sys.stderr)
        return 2
    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)-7s dev-service: %(message)s"
    )
    return DevSupervisor(root).run()
