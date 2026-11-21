"""Whether a local socket's other end is this user's own process.

The backend checks every client's user (`ipc/server.rs`); clients check the
backend the same way. A socket in a shared folder (the `/tmp/neurowatch`
fallback) can be created by any local user: without this check the window
and the terminal commands sent camera and account passwords and link codes
to whoever listened there (vulnerability checklist R9).

No Qt: the service manager and headless mode use it too.
"""

from __future__ import annotations

import os
import socket
import struct


def same_user(sock: socket.socket, path: str) -> bool:
    """True when the process at the other end of `sock` (connected to the
    Unix socket `path`) runs as this user (or as root)."""
    allowed = {os.geteuid(), 0}
    option = getattr(socket, "SO_PEERCRED", None)
    if option is not None:
        size = struct.calcsize("3i")
        try:
            raw = sock.getsockopt(socket.SOL_SOCKET, option, size)
        except OSError:
            return False
        _pid, uid, _gid = struct.unpack("3i", raw[:size])
        return uid in allowed
    # No peer credentials here (macOS): the socket file's owner.
    try:
        return os.stat(path).st_uid in allowed
    except OSError:
        return False
