"""What starts when the user signs in, and whether recording starts at boot.

The user's choices (Settings → Startup, ``neurowatch service install`` /
``uninstall``) live in ``<state>/startup.json``; the service manager follows
them every time it registers the service, so opening NeuroWatch never turns
a switched-off start back on. Whether the window opens fullscreen is the
backend config's ``ui.fullscreen``. No Qt here.
"""

from __future__ import annotations

import json
import logging
from dataclasses import asdict, dataclass, fields

from neurowatch.service.paths import state_dir

logger = logging.getLogger(__name__)

_FILE = "startup.json"


@dataclass(frozen=True)
class StartupPrefs:
    """How NeuroWatch starts outside the desktop UI."""

    # Start the recorder (and, in desktop mode, the tray) when the user signs in.
    at_login: bool = True
    # At sign-in, also open the window; otherwise only the tray icon shows.
    open_window: bool = False
    # Linux: start recording at boot, before anyone signs in (systemd lingering).
    at_boot: bool = False


def load_prefs() -> StartupPrefs:
    """The saved choices; missing or unreadable: the defaults."""
    try:
        raw = json.loads((state_dir() / _FILE).read_text())
    except (OSError, ValueError):
        return StartupPrefs()
    if not isinstance(raw, dict):
        return StartupPrefs()
    known = {f.name for f in fields(StartupPrefs)}
    return StartupPrefs(**{k: v for k, v in raw.items() if k in known and isinstance(v, bool)})


def save_prefs(prefs: StartupPrefs) -> None:
    path = state_dir() / _FILE
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(asdict(prefs), indent=2))
        temporary.replace(path)
    except OSError as error:
        logger.warning("Could not save the startup choices: %s", error)
