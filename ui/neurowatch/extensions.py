"""Optional extensions of the desktop app, the tray and the headless commands.

NeuroWatch runs without any. A build adds one as an installed package that
declares an entry point in the ``neurowatch.extensions`` group; for
development, ``NW_UI_EXTENSIONS`` names extension modules to load as well
(comma separated). An extension is any object (usually a module) with some of
these optional hooks:

Desktop app (Qt main thread):
    ``install(window)``: add to the main window through its extension
    methods (``add_menu_action``, ``add_status_indicator``, ``ipc_endpoint``).

Tray (worker thread):
    ``tray_status(request, endpoint) -> ExtensionStatus | None``

Headless commands (no Qt):
    ``status_line(rpc) -> str | None``: one ``"Label: text"`` line for the
    status and the monitor (reprinted when it changes).
    ``add_monitor_arguments(parser)`` and
    ``before_monitor(args, rpc, say) -> int | None``: options of
    ``--headless``; return an exit code to stop, or None to go on.
    ``commands``: the top-level command names it adds; ``add_commands(subparsers)``
    and ``run_command(args, rpc, say) -> int | None`` (None: not its command).

A failing extension is logged and skipped: NeuroWatch keeps working without it.
This module must not import Qt.
"""

from __future__ import annotations

import functools
import importlib
import logging
import os
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from importlib.metadata import entry_points
from typing import Any

logger = logging.getLogger(__name__)

GROUP = "neurowatch.extensions"
ENV = "NW_UI_EXTENSIONS"


@dataclass(frozen=True)
class ExtensionStatus:
    """What an extension adds to the tray: a detail for its status line and
    problems for its warning badge, as ``(label, explanation)`` pairs."""

    detail: str = ""
    problems: tuple[tuple[str, str], ...] = ()


def load() -> tuple[Any, ...]:
    """The extensions of this installation, loaded once."""
    return _discover()


@functools.cache
def _discover() -> tuple[Any, ...]:
    found: list[Any] = []
    names = [name.strip() for name in os.environ.get(ENV, "").split(",") if name.strip()]
    for name in names:
        try:
            found.append(importlib.import_module(name))
        except Exception:
            logger.exception("Extension module %s could not be loaded; skipped", name)
    for entry in entry_points(group=GROUP):
        try:
            extension = entry.load()
        except Exception:
            logger.exception("Extension %s could not be loaded; skipped", entry.name)
            continue
        if not any(extension is other for other in found):
            found.append(extension)
    return tuple(found)


def reset() -> None:
    """Load the extensions again on next use (tests, or after ``NW_UI_EXTENSIONS`` changes)."""
    _discover.cache_clear()


def hooks(name: str) -> Iterator[tuple[Any, Callable[..., Any]]]:
    """Each loaded extension that provides hook ``name``, with the hook."""
    for extension in load():
        hook = getattr(extension, name, None)
        if callable(hook):
            yield extension, hook


def call_each(name: str, *args: Any) -> list[Any]:
    """Call hook ``name`` of every extension; a failing one is logged and skipped."""
    results: list[Any] = []
    for extension, hook in hooks(name):
        try:
            results.append(hook(*args))
        except Exception:
            logger.exception("Extension %r failed in %s; skipped", extension, name)
    return results


def commands() -> set[str]:
    """Top-level command names the extensions add (e.g. ``neurowatch NAME ...``)."""
    names: set[str] = set()
    for extension in load():
        names.update(str(name) for name in getattr(extension, "commands", ()) or ())
    return names
