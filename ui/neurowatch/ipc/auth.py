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


def with_auth(request: dict, endpoint: str) -> dict:
    """``request`` with this user's token added for a TCP endpoint."""
    token = token_for(endpoint)
    return {**request, "auth": token} if token else request
