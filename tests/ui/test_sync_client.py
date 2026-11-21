from __future__ import annotations

import os
import socket
import struct
from pathlib import Path

import pytest

from neurowatch.ipc.sync_client import BackendOfflineError, SyncIpcClient


class _DummySocket:
    def __init__(self) -> None:
        self.timeout = None
        self.connected = None

    def settimeout(self, value):
        self.timeout = value

    def connect(self, addr):
        self.connected = addr

    def getsockopt(self, level, option, size):
        # The backend runs as this user.
        return struct.pack("3i", os.getpid(), os.geteuid(), os.getegid())

    def close(self):
        pass


def test_connection_reset_becomes_backend_offline_error(monkeypatch):
    class ResetSocket(_DummySocket):
        closed = False

        def sendall(self, payload):
            pass

        def recv(self, size):
            raise ConnectionResetError(104, "Connection reset by peer")

        def close(self):
            self.closed = True

    sock = ResetSocket()
    monkeypatch.setattr(socket, "socket", lambda *args: sock)
    client = SyncIpcClient(endpoint="unix:/tmp/test-reset.sock")
    with pytest.raises(BackendOfflineError, match="Backend connection lost"):
        client.call("camera.set_quality")
    assert sock.closed


def test_connect_socket_unix_endpoint(monkeypatch):
    created = {}

    def _factory(family, _stype):
        created["family"] = family
        created["sock"] = _DummySocket()
        return created["sock"]

    monkeypatch.setattr(socket, "socket", _factory)

    client = SyncIpcClient(endpoint="unix:/tmp/neurowatch.sock")
    sock = client._connect_socket()

    assert created["family"] == socket.AF_UNIX
    assert sock.connected == "/tmp/neurowatch.sock"


def test_connect_socket_tcp_endpoint(monkeypatch):
    created = {}

    def _factory(family, _stype):
        created["family"] = family
        created["sock"] = _DummySocket()
        return created["sock"]

    monkeypatch.setattr(socket, "socket", _factory)

    client = SyncIpcClient(endpoint="tcp:127.0.0.1:7676")
    sock = client._connect_socket()

    assert created["family"] == socket.AF_INET
    assert sock.connected == ("127.0.0.1", 7676)


def test_resolve_endpoints_prefers_explicit_endpoint(monkeypatch):
    monkeypatch.setenv("NW_IPC_ENDPOINT", "unix:/tmp/from-env.sock")
    client = SyncIpcClient(endpoint="unix:/tmp/explicit.sock")
    assert client._endpoints == ["unix:/tmp/explicit.sock"]


def test_resolve_endpoints_uses_env_override(monkeypatch):
    monkeypatch.setenv("NW_IPC_ENDPOINT", "unix:/tmp/from-env.sock")
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    # Not this computer's own runtime folder: a socket left there by the
    # developer's recorder would be tried first.
    monkeypatch.setattr("neurowatch.ipc.sync_client.login_runtime_dir", lambda: None)
    client = SyncIpcClient()
    assert client._endpoints[0] == "unix:/tmp/from-env.sock"
    assert "unix:/tmp/neurowatch/control.sock" in client._endpoints
    assert "unix:/run/neurowatch/control.sock" in client._endpoints


def test_resolve_endpoints_ignores_root_control_socket(monkeypatch):
    monkeypatch.setenv("NW_IPC_ENDPOINT", "unix:/control.sock")
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.setattr("neurowatch.ipc.sync_client.login_runtime_dir", lambda: None)

    client = SyncIpcClient()

    assert client._endpoints[0] == "unix:/tmp/neurowatch/control.sock"
    assert "unix:/control.sock" not in client._endpoints


def test_resolve_endpoints_prefers_existing_runtime_socket(monkeypatch):
    monkeypatch.delenv("NW_IPC_ENDPOINT", raising=False)
    monkeypatch.setenv("XDG_RUNTIME_DIR", "/run/user/1000")
    monkeypatch.setattr(
        os.path,
        "exists",
        lambda path: path == "/run/user/1000/neurowatch/control.sock",
    )

    client = SyncIpcClient()
    assert client._endpoints[0] == "unix:/run/user/1000/neurowatch/control.sock"
    assert "unix:/run/neurowatch/control.sock" in client._endpoints


def test_resolve_endpoints_without_xdg_runtime_dir_tries_the_login_runtime_folder(monkeypatch):
    # Some SSH servers open sessions without XDG_RUNTIME_DIR; the service
    # still listens in /run/user/<uid>.
    monkeypatch.delenv("NW_IPC_ENDPOINT", raising=False)
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.setattr(
        "neurowatch.ipc.sync_client.login_runtime_dir", lambda: Path("/run/user/1000")
    )
    monkeypatch.setattr(os.path, "exists", lambda _path: False)

    client = SyncIpcClient()
    assert client._endpoints[:2] == [
        "unix:/run/user/1000/neurowatch/control.sock",
        "unix:/tmp/neurowatch/control.sock",
    ]

    # A service an older version started on the /tmp fallback is still found.
    monkeypatch.setattr(os.path, "exists", lambda path: path == "/tmp/neurowatch/control.sock")
    assert SyncIpcClient()._endpoints[0] == "unix:/tmp/neurowatch/control.sock"


def test_connect_socket_falls_back_to_secondary_endpoint(monkeypatch):
    attempts: list[str] = []

    class _FailThenPassSocket(_DummySocket):
        def connect(self, addr):
            attempts.append(addr)
            if addr == "/tmp/primary.sock":
                raise FileNotFoundError(addr)
            self.connected = addr

    monkeypatch.setattr(socket, "socket", lambda _family, _stype: _FailThenPassSocket())

    client = SyncIpcClient(endpoint="unix:/tmp/primary.sock")
    client._endpoints = ["unix:/tmp/primary.sock", "unix:/tmp/fallback.sock"]

    sock = client._connect_socket()

    assert attempts == ["/tmp/primary.sock", "/tmp/fallback.sock"]
    assert sock.connected == "/tmp/fallback.sock"
    assert client._endpoint == "unix:/tmp/fallback.sock"


class _ReplySocket(_DummySocket):
    def __init__(self) -> None:
        super().__init__()
        self.timeouts: list[float] = []
        self.timeout_during_recv: float | None = None

    def settimeout(self, value):
        self.timeouts.append(value)
        self.timeout = value

    def sendall(self, payload):
        pass

    def recv(self, size):
        self.timeout_during_recv = self.timeout
        return b'{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'


def test_call_waits_for_a_longer_reply_only_when_asked(monkeypatch):
    sock = _ReplySocket()
    monkeypatch.setattr(socket, "socket", lambda *args: sock)
    client = SyncIpcClient(endpoint="unix:/tmp/test-timeout.sock")

    assert client.call("system.status") == {"ok": True}
    assert sock.timeout_during_recv == 5.0

    assert client.call("example.slow", timeout=300.0) == {"ok": True}
    assert sock.timeout_during_recv == 300.0


def test_a_listener_of_another_user_is_never_sent_anything(monkeypatch, tmp_path):
    """Vulnerability checklist R9: another local user can create the shared
    /tmp/neurowatch socket; the client must not send it passwords or codes."""
    sent: list[bytes] = []

    class StrangerSocket(_DummySocket):
        def getsockopt(self, level, option, size):
            return struct.pack("3i", 1, os.geteuid() + 1, 1)

        def sendall(self, payload):
            sent.append(payload)

    monkeypatch.setattr(socket, "socket", lambda *args: StrangerSocket())
    client = SyncIpcClient(endpoint="unix:/tmp/neurowatch/control.sock")
    with pytest.raises(BackendOfflineError):
        client.call("auth.login", {"username": "admin", "password": "secret"})
    assert sent == []


def test_a_real_socket_of_this_user_is_accepted(tmp_path):
    import threading

    path = str(tmp_path / "control.sock")
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    server.listen(1)

    def answer():
        conn, _ = server.accept()
        with conn:
            conn.recv(65536)
            conn.sendall(b'{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n')

    thread = threading.Thread(target=answer)
    thread.start()
    try:
        assert SyncIpcClient(endpoint=f"unix:{path}").call("system.status") == {"ok": True}
    finally:
        thread.join(5)
        server.close()


def test_the_per_user_tmp_folder_is_a_candidate_without_a_runtime_dir(monkeypatch, tmp_path):
    """Round 3: a service that could not use a shared /tmp/neurowatch listens in
    /tmp/neurowatch-<uid>; the client looks there too."""
    monkeypatch.delenv("NW_IPC_ENDPOINT", raising=False)
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.setattr("neurowatch.ipc.sync_client.login_runtime_dir", lambda: None)
    monkeypatch.setattr(
        "neurowatch.ipc.sync_client.tmp_fallback_endpoint",
        lambda: f"unix:{tmp_path}/neurowatch-1/control.sock",
    )
    assert f"unix:{tmp_path}/neurowatch-1/control.sock" in SyncIpcClient()._endpoints
