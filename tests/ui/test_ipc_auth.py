"""This user's TCP token: found where the backend writes it, and sent."""

from __future__ import annotations

import json
import socket
import threading
from pathlib import Path

import pytest

from neurowatch.ipc import auth
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.service import probe


@pytest.fixture
def data_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("XDG_DATA_HOME", str(tmp_path))
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    return tmp_path


def test_the_token_file_matches_the_backends_name(data_home: Path) -> None:
    # Same label as ipc/token.rs `file_label`.
    assert auth.token_path("tcp:127.0.0.1:7676") == (
        data_home / "neurowatch" / "ipc-token-127.0.0.1-7676"
    )
    assert auth.token_path("tcp:[::1]:7676").name == "ipc-token----1--7676"
    assert auth.token_path("unix:/run/user/1000/neurowatch/control.sock") is None


def test_the_token_is_added_only_for_tcp_and_only_when_written(data_home: Path) -> None:
    request = {"jsonrpc": "2.0", "id": 1, "method": "cloud.status", "params": {}}
    assert auth.with_auth(request, "tcp:127.0.0.1:7676") == request, "no file yet"
    path = auth.token_path("tcp:127.0.0.1:7676")
    assert path is not None
    path.parent.mkdir(parents=True)
    path.write_text("abc123\n")
    assert auth.with_auth(request, "tcp:127.0.0.1:7676")["auth"] == "abc123"
    assert "auth" not in auth.with_auth(request, "unix:/tmp/x.sock")
    assert "auth" not in request, "the caller's request is not changed"


def _one_request_server() -> tuple[int, list[dict]]:
    """A TCP server that records one request per connection and answers it."""
    server = socket.create_server(("127.0.0.1", 0))
    received: list[dict] = []

    def serve() -> None:
        for _ in range(2):
            connection, _address = server.accept()
            with connection:
                request = json.loads(connection.makefile("rb").readline())
                received.append(request)
                reply = {"jsonrpc": "2.0", "id": request["id"], "result": {"ok": True}}
                connection.sendall(json.dumps(reply).encode() + b"\n")
        server.close()

    threading.Thread(target=serve, daemon=True).start()
    return server.getsockname()[1], received


def test_clients_send_the_token_to_a_tcp_backend(data_home: Path) -> None:
    port, received = _one_request_server()
    endpoint = f"tcp:127.0.0.1:{port}"
    path = auth.token_path(endpoint)
    assert path is not None
    path.parent.mkdir(parents=True)
    path.write_text("secret-token")

    assert SyncIpcClient(endpoint=endpoint).call("cloud.status") == {"ok": True}
    assert probe.request("cloud.status", endpoint=endpoint) == {"ok": True}
    assert [r.get("auth") for r in received] == ["secret-token", "secret-token"]
    assert [r["method"] for r in received] == ["cloud.status", "cloud.status"]
