"""Synchronous JSON-RPC 2.0 client for use from Qt main thread.

Uses a fresh Unix socket per call — simple and correct for low-frequency
camera CRUD operations (not for high-throughput streaming).
"""

from __future__ import annotations

import json
import logging
import os
import socket
from typing import Any

logger = logging.getLogger(__name__)

DEFAULT_ENDPOINT = "tcp:127.0.0.1:7676" if os.name == "nt" else "unix:/run/neurowatch/control.sock"


class IpcError(Exception):
    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class BackendOfflineError(Exception):
    """Raised when the backend socket is not reachable."""


class SyncIpcClient:
    """Synchronous JSON-RPC client — safe to call from any Qt thread."""

    def __init__(self, endpoint: str | None = None) -> None:
        self._endpoints = self._resolve_endpoints(endpoint)
        self._endpoint = self._endpoints[0]
        self._next_id = 1

    @property
    def endpoint(self) -> str:
        """The endpoint currently selected by this client."""
        return self._endpoint

    def _resolve_endpoints(self, endpoint: str | None) -> list[str]:
        if endpoint:
            return [endpoint]

        env_endpoint = os.getenv("NW_IPC_ENDPOINT")
        if env_endpoint == "unix:/control.sock":
            env_endpoint = None

        if os.name == "nt":
            return [env_endpoint or DEFAULT_ENDPOINT]

        candidates: list[str] = []
        if env_endpoint:
            candidates.append(env_endpoint)
        runtime_dir = os.getenv("XDG_RUNTIME_DIR")
        if runtime_dir:
            candidates.append(f"unix:{runtime_dir}/neurowatch/control.sock")
        candidates.append("unix:/tmp/neurowatch/control.sock")
        candidates.append(DEFAULT_ENDPOINT)

        unique_candidates = list(dict.fromkeys(candidates))
        existing = [
            candidate
            for candidate in unique_candidates
            if candidate.startswith("unix:") and os.path.exists(candidate.removeprefix("unix:"))
        ]
        if existing:
            return existing + [
                candidate for candidate in unique_candidates if candidate not in existing
            ]
        return unique_candidates

    def call(
        self,
        method: str,
        params: dict[str, Any] | list[Any] | None = None,
        *,
        timeout: float | None = None,
    ) -> Any:
        """Make a synchronous RPC call.  Raises IpcError or BackendOfflineError.

        ``timeout`` overrides the default 5 s reply wait for calls that are
        slow by design (e.g. linking to an online service, which waits on the network or a
        local human confirmation). Only use it from background threads.
        """
        rpc_id = self._next_id
        self._next_id += 1
        payload = (
            json.dumps({"jsonrpc": "2.0", "id": rpc_id, "method": method, "params": params or {}})
            + "\n"
        )

        logger.debug("IPC send: %s (id=%d)", method, rpc_id)
        try:
            sock = self._connect_socket()
        except (FileNotFoundError, ConnectionRefusedError, TimeoutError, OSError) as e:
            logger.warning("IPC connection failed for %s: %s", method, e)
            raise BackendOfflineError(f"Backend not reachable: {e}") from e

        try:
            if timeout is not None:
                sock.settimeout(timeout)
            try:
                sock.sendall(payload.encode())
                buf = b""
                while b"\n" not in buf:
                    chunk = sock.recv(65536)
                    if not chunk:
                        break
                    buf += chunk
            except TimeoutError as e:
                logger.warning("IPC timeout waiting for reply to %s: %s", method, e)
                raise BackendOfflineError(f"Timed out waiting for backend reply ({method})") from e
            except OSError as e:
                raise BackendOfflineError(f"Backend connection lost ({method}): {e}") from e
        finally:
            sock.close()

        if not buf:
            logger.warning("IPC: backend returned no data for %s", method)
            raise BackendOfflineError(f"Backend returned no data ({method})")

        line = buf.split(b"\n")[0]
        try:
            resp = json.loads(line)
        except json.JSONDecodeError as e:
            raise IpcError(-32700, f"Invalid JSON-RPC response: {e}") from e

        if err := resp.get("error"):
            logger.warning(
                "IPC error response for %s: [%s] %s",
                method,
                err.get("code", -1),
                err.get("message", "unknown"),
            )
            raise IpcError(err.get("code", -1), err.get("message", "unknown"))
        return resp.get("result")

    def is_online(self) -> bool:
        """Return True if the backend socket is reachable."""
        try:
            self.call("system.status")
            return True
        except (BackendOfflineError, IpcError):
            return False

    def _connect_socket(self) -> socket.socket:
        last_error: Exception | None = None
        for endpoint in self._endpoints:
            try:
                sock = self._connect_single_endpoint(endpoint)
                if endpoint != self._endpoint:
                    logger.info("IPC endpoint fallback selected: %s", endpoint)
                    self._endpoint = endpoint
                return sock
            except (FileNotFoundError, ConnectionRefusedError, OSError) as e:
                logger.warning("IPC could not connect to %s: %s", endpoint, e)
                last_error = e
                continue

        if last_error is None:
            raise RuntimeError("No IPC endpoints configured")
        raise last_error

    def _connect_single_endpoint(self, endpoint: str) -> socket.socket:
        if endpoint.startswith("tcp:"):
            host_port = endpoint.removeprefix("tcp:")
            host, port = host_port.rsplit(":", 1)
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(5.0)
            sock.connect((host, int(port)))
            logger.debug("IPC socket connected to %s", endpoint)
            return sock

        path = endpoint.removeprefix("unix:")
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(5.0)
        sock.connect(path)
        logger.debug("IPC socket connected to %s", endpoint)
        return sock
