"""Analytics daemons with shared Rust transport and signed session identities.

Implement Plugin callbacks and call serve(). The transport helper owns TLS,
key protection, and peer signature verification; Python owns decoding and analysis.
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
import secrets
import shutil
import subprocess
import sys
import time
from collections import deque
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO

from .wire import GopCache, MediaUnit, encode, read_frame

__all__ = [
    "GopCache",
    "HttpRequest",
    "HttpResponse",
    "MediaUnit",
    "Plugin",
    "Refusal",
    "RefusalError",
    "Session",
    "serve",
]


class RefusalError(Exception):
    def __init__(
        self, code: str, message: str, *, url: str | None = None, retry_after_s: int | None = None
    ):
        super().__init__(message)
        self.data: dict[str, Any] = {"code": code, "message": message}
        if url is not None:
            self.data["url"] = url
        if retry_after_s is not None:
            self.data["retry_after_s"] = retry_after_s


Refusal = RefusalError


@dataclass(frozen=True)
class HttpRequest:
    path: str
    method: str
    content_type: str
    body: bytes
    user: dict[str, Any]


@dataclass(frozen=True)
class HttpResponse:
    body: bytes
    content_type: str = "text/html; charset=utf-8"
    status: int = 200


class Plugin:
    """Callbacks run in a session. Offload blocking inference with to_thread().

    Each binding gets one bounded media worker; control and liveness remain
    responsive while an async media callback waits for inference.
    """

    def manifest(self) -> dict[str, Any]:
        raise NotImplementedError

    async def authorize(self, node_key: str, credential: str | None) -> None:
        """Raise Refusal for a missing/expired licence; signature is verified."""

    async def attach(
        self, binding: int, camera: dict[str, str], config: dict[str, Any]
    ) -> dict[str, Any]:
        return {}

    async def configure(self, binding: int, config: dict[str, Any]) -> dict[str, Any]:
        return {}

    async def validate_config(self, config: dict[str, Any]) -> dict[str, Any]:
        return {"ok": True, "errors": []}

    async def notification(self, method: str, params: dict[str, Any]) -> None:
        pass

    async def unit(self, unit: MediaUnit, session: Session) -> None:
        """Decode/process compressed camera bytes in the plugin, never the node."""

    async def detach(self, binding: int) -> None:
        pass

    async def web(self, request: HttpRequest) -> HttpResponse:
        return HttpResponse(b"Not found", "text/plain", 404)


class _Binding:
    def __init__(self, session: Session, binding: int):
        self.session, self.binding = session, binding
        self.queue: deque[MediaUnit] = deque()
        self.wake = asyncio.Event()
        self.wait_keyframe = True
        self.closed = False
        self.gap = False
        self.last_seq: int | None = None
        self.task = asyncio.create_task(self.run())

    def offer(self, unit: MediaUnit) -> None:
        if unit.discontinuity:
            self.clear()
        if self.wait_keyframe and not unit.keyframe:
            return
        if len(self.queue) >= 64 or self.session.buffered + len(unit.data) > 32 * 1024 * 1024:
            self.clear()
            return
        self.wait_keyframe = False
        self.queue.append(unit)
        self.session.buffered += len(unit.data)
        self.wake.set()

    def clear(self) -> None:
        self.session.buffered -= sum(len(unit.data) for unit in self.queue)
        self.queue.clear()
        self.wait_keyframe = True
        self.gap = True

    async def run(self) -> None:
        while not self.closed:
            await self.wake.wait()
            self.wake.clear()
            while self.queue and not self.closed:
                unit = self.queue.popleft()
                try:
                    if self.gap:
                        self.gap = False
                        await self.session.plugin.notification(
                            "gap",
                            {
                                "binding": self.binding,
                                "reason": "behind",
                                "from_seq": self.last_seq,
                                "to_seq": unit.seq,
                            },
                        )
                    await self.session.plugin.unit(unit, self.session)
                    self.last_seq = unit.seq
                except Exception:
                    self.clear()
                    await self.session.notify(
                        "status",
                        {
                            "bindings": [
                                {
                                    "binding": self.binding,
                                    "state": "error",
                                    "detail": "Plugin processing failed",
                                    "fps": 0,
                                    "frames": 0,
                                }
                            ]
                        },
                    )
                finally:
                    self.session.buffered -= len(unit.data)

    async def close(self) -> None:
        self.closed = True
        self.clear()
        self.task.cancel()
        try:
            await self.task
        except asyncio.CancelledError:
            pass
        await self.session.plugin.detach(self.binding)


class Session:
    def __init__(self, plugin: Plugin, reader: BinaryIO, writer: BinaryIO):
        self.plugin, self.reader, self.writer = plugin, reader, writer
        self.bindings: dict[int, _Binding] = {}
        self.buffered = 0
        self.write_lock = asyncio.Lock()
        self.bodies: dict[str, tuple[float, str, HttpResponse]] = {}

    async def send(self, message: dict[str, Any]) -> None:
        data = encode(message)
        async with self.write_lock:
            await asyncio.to_thread(self._write, data)

    def _write(self, data: bytes) -> None:
        self.writer.write(data)
        self.writer.flush()

    async def notify(self, method: str, params: dict[str, Any]) -> None:
        await self.send({"jsonrpc": "2.0", "method": method, "params": params})

    async def event_start(
        self,
        binding: int,
        event: int,
        kind: str,
        *,
        score: float | None = None,
        seq: int | None = None,
        alert: bool | None = None,
    ) -> None:
        params: dict[str, Any] = {
            "binding": binding,
            "event": event,
            "kind": kind,
            "score": score,
            "seq": seq,
        }
        if alert is not None:
            # False: a detection (recorded and listed, never fires alert rules).
            params["alert"] = alert
        await self.notify("event.start", params)

    async def overlay(
        self,
        binding: int,
        layer: str,
        items: list[dict[str, Any]],
        *,
        seq: int | None = None,
        ttl_ms: int = 2000,
    ) -> None:
        await self.notify(
            "overlay",
            {"binding": binding, "layer": layer, "items": items, "seq": seq, "ttl_ms": ttl_ms},
        )

    async def request(self, method: str, params: dict[str, Any]) -> Any:
        if method == "hello":
            manifest = dict(self.plugin.manifest())
            manifest.update(protocol=1, ping=True)
            return manifest
        if method == "authenticate":
            await self.plugin.authorize(params["public_key"], params.get("credential"))
            return {"authorized": True}
        if method.startswith("web."):
            # The companion transport verified this context against the
            # authenticated node key and the plugin's own TLS/identity key.
            user = params["user"]["context"]
            scope = json.dumps(
                {key: user[key] for key in ("user", "role", "cameras", "plugin_key")},
                sort_keys=True,
            )
            self.bodies = {
                key: value for key, value in self.bodies.items() if value[0] > time.monotonic()
            }
            if method == "web.request":
                path, verb = params["path"], params["method"]
                if (
                    not isinstance(path, str)
                    or not path.startswith("/")
                    or path.startswith("//")
                    or len(path) > 2048
                ):
                    raise ValueError("Invalid page path")
                if user["role"] == "viewer" and verb not in ("GET", "HEAD"):
                    raise Refusal("NOT_ALLOWED", "This page is read-only")
                encoded = params.get("body_b64", "")
                if len(encoded) > 65540:
                    raise ValueError("Request too large")
                body = base64.b64decode(encoded, validate=True)
                response = await self.plugin.web(
                    HttpRequest(path, verb, params.get("content_type", ""), body, user)
                )
                if (
                    len(response.body) > 4 * 1024 * 1024
                    or not 200 <= response.status <= 599
                    or len(response.content_type) > 128
                    or not response.content_type.isascii()
                    or any(char in response.content_type for char in "\r\n")
                ):
                    raise ValueError("Response exceeds ingress limits")
                while self.bodies and (
                    len(self.bodies) >= 16
                    or sum(len(value[2].body) for value in self.bodies.values())
                    + len(response.body)
                    > 8 * 1024 * 1024
                ):
                    self.bodies.pop(next(iter(self.bodies)))
                token = secrets.token_hex(32)
                self.bodies[token] = (time.monotonic() + 300, scope, response)
                return {
                    "token": token,
                    "status": response.status,
                    "content_type": response.content_type,
                    "size": len(response.body),
                }
            if method == "web.read":
                body = self.bodies.get(params["token"])
                if body is None or body[1] != scope:
                    raise Refusal("NOT_ALLOWED", "Page body expired or belongs to another viewer")
                offset = params["offset"]
                if type(offset) is not int or not 0 <= offset <= len(body[2].body):
                    raise ValueError("Invalid body offset")
                data = body[2].body[offset : offset + 48 * 1024]
                return {
                    "offset": offset,
                    "data_b64": base64.b64encode(data).decode(),
                    "eof": offset + len(data) == len(body[2].body),
                }
            raise ValueError("Unknown web method")
        if method == "ping":
            return None
        if method == "attach":
            binding = params["binding"]
            if binding in self.bindings or len(self.bindings) >= 1024:
                raise Refusal("NO_CAPACITY", "No binding capacity")
            result = await self.plugin.attach(binding, params["camera"], params["config"])
            self.bindings[binding] = _Binding(self, binding)
            return result
        if method == "configure":
            return await self.plugin.configure(params["binding"], params["config"])
        if method == "validate_config":
            return await self.plugin.validate_config(params["config"])
        if method == "detach":
            worker = self.bindings.pop(params["binding"], None)
            if worker:
                await worker.close()
            return None
        raise Refusal("METHOD_NOT_FOUND", "Method not found")

    async def run(self) -> None:
        try:
            while True:
                frame = await asyncio.to_thread(read_frame, self.reader)
                if isinstance(frame, MediaUnit):
                    if worker := self.bindings.get(frame.binding):
                        worker.offer(frame)
                    continue
                method, params = frame.get("method"), frame.get("params", {})
                if "id" in frame and method:
                    response: dict[str, Any] = {"jsonrpc": "2.0", "id": frame["id"]}
                    try:
                        response["result"] = await self.request(method, params)
                    except Refusal as error:
                        response["error"] = {
                            "code": -33002,
                            "message": str(error),
                            "data": error.data,
                        }
                    except (KeyError, TypeError, ValueError):
                        response["error"] = {"code": -32602, "message": "Invalid parameters"}
                    await self.send(response)
                elif method:
                    if method == "gap" and (worker := self.bindings.get(params.get("binding"))):
                        worker.clear()
                    await self.plugin.notification(method, params)
        except EOFError:
            pass
        finally:
            for worker in list(self.bindings.values()):
                await worker.close()
            self.bindings.clear()


def serve(
    plugin: Plugin, *, address: str, identity: str | Path, bridge: str | Path | None = None
) -> None:
    """Run a daemon on Unix, Windows pipe, loopback TCP, or QUIC.

    Install the companion nw-analytics-bridge executable from the Rust SDK.
    The same persistent identity must be used across daemon restarts.
    """
    if os.environ.get("NW_ANALYTICS_SESSION") == "1":
        asyncio.run(Session(plugin, sys.stdin.buffer, sys.stdout.buffer).run())
        return
    executable = str(bridge) if bridge else shutil.which("nw-analytics-bridge")
    if not executable:
        raise RuntimeError(
            "Install nw-analytics-bridge from crates/nw-analytics-sdk, or pass bridge=PATH"
        )
    subprocess.run(
        [
            executable,
            "--listen",
            address,
            "--identity",
            str(identity),
            "--",
            sys.executable,
            *sys.argv,
        ],
        check=True,
    )
