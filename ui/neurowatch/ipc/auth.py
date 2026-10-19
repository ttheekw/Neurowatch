"""This user's token for a TCP endpoint (see ``ipc/token.rs`` in the backend).

Any local process can reach a TCP port (Windows always uses one), so the
backend writes a random token to a file only this user can read; requests
carrying it (``"auth"``) are served like those on the owner-only Unix socket,
including extension methods such as the cloud's. No Qt: the service manager
and the tray's quiet probes use it too.
"""

from __future__ import annotations

import os
import re
from pathlib import Path


def data_local_dir() -> Path:
    """The backend's ``dirs::data_local_dir()``."""
    if os.name == "nt":
        return Path(os.environ.get("LOCALAPPDATA") or Path.home() / "AppData" / "Local")
    return Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local" / "share")


def token_path(endpoint: str) -> Path | None:
    """The token file of a ``tcp:host:port`` endpoint; None for Unix sockets."""
    if not endpoint.startswith("tcp:"):
        return None
    label = re.sub(r"[^A-Za-z0-9.]", "-", endpoint.removeprefix("tcp:"))
    return data_local_dir() / "neurowatch" / f"ipc-token-{label}"


def token_for(endpoint: str) -> str | None:
    """The token to send to ``endpoint``, if it is TCP and the backend wrote one."""
    path = token_path(endpoint)
    if path is None:
        return None
    try:
        token = path.read_text(encoding="ascii").strip()
    except (OSError, UnicodeDecodeError):
        return None
    return token or None


#: This app's sign-in on a recorder that requires one (``auth.login``): the
#: session token every request carries, and who signed in.
_session: str | None = None
_user: dict | None = None


def set_session(token: str | None, user: dict | None = None) -> None:
    global _session, _user
    _session, _user = token, (user if token else None)


def current_session() -> str | None:
    return _session


def current_user() -> dict | None:
    """``{username, role, cameras}`` of the signed-in user, or None."""
    return _user


ROLE_ORDER = {"viewer": 0, "operator": 1, "admin": 2}


def may(role: str) -> bool:
    """Whether this app's user has at least ``role``; without sign-in, yes
    (the recorder then has no accounts, or refuses by itself)."""
    if _user is None:
        return True
    return ROLE_ORDER.get(str(_user.get("role")), -1) >= ROLE_ORDER.get(role, 99)


def with_auth(request: dict, endpoint: str) -> dict:
    """``request`` with this user's token added for a TCP endpoint, and the
    app's session when it signed in."""
    token = token_for(endpoint)
    if token:
        request = {**request, "auth": token}
    if _session:
        request = {**request, "session": _session}
    return request
