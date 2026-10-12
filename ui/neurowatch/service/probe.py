"""Quiet requests to the backend for the service manager and the tray.

``SyncIpcClient`` logs a warning for every failed connection; a tray that
checks a stopped service every few seconds must not fill its log with them.
"""

from __future__ import annotations

import json
import socket
from typing import Any

from neurowatch.ipc.auth import with_auth
from neurowatch.service.paths import service_endpoint


def request(
    method: str,
    params: dict[str, Any] | None = None,
    *,
    endpoint: str | None = None,
    timeout: float = 2.0,
) -> Any | None:
    """The result of one JSON-RPC call, or None when the backend is not
    reachable or answers with an error."""
    target = endpoint or service_endpoint()
    request = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}
    payload = json.dumps(with_auth(request, target))
    try:
        if target.startswith("unix:"):
            family = getattr(socket, "AF_UNIX", None)
            if family is None:
                return None
            sock = socket.socket(family, socket.SOCK_STREAM)
            address: Any = target.removeprefix("unix:")
        else:
            host, _, port = target.removeprefix("tcp:").rpartition(":")
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            address = (host or "127.0.0.1", int(port))
        with sock:
            sock.settimeout(timeout)
            sock.connect(address)
            sock.sendall(payload.encode() + b"\n")
            buffer = b""
            while b"\n" not in buffer:
                chunk = sock.recv(65536)
                if not chunk:
                    break
                buffer += chunk
        response = json.loads(buffer.split(b"\n")[0])
    except (OSError, ValueError):
        return None
    if not isinstance(response, dict) or "error" in response:
        return None
    return response.get("result")


def backend_status(endpoint: str | None = None, timeout: float = 2.0) -> dict[str, Any] | None:
    """``system.status`` of a running backend, or None."""
    result = request("system.status", endpoint=endpoint, timeout=timeout)
    if isinstance(result, dict) and result.get("status") == "running":
        return result
    return None


def is_running(endpoint: str | None = None) -> bool:
    return backend_status(endpoint, timeout=1.0) is not None
