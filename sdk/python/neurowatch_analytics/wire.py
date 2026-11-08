"""Bounded protocol-v1 framing, including exact 64-bit picture identities."""

from __future__ import annotations

import json
import struct
from dataclasses import dataclass
from typing import Any, BinaryIO

MAX_MESSAGE = 1024 * 1024
MAX_MEDIA = 16 * 1024 * 1024
HEADER = struct.Struct("<BBHIQqqq")
NO_TIME = -(1 << 63)


@dataclass(frozen=True, slots=True)
class MediaUnit:
    binding: int
    seq: int
    time_us: int
    data: bytes
    flags: int = 0
    pts_ns: int | None = None
    dts_ns: int | None = None

    @property
    def keyframe(self) -> bool:
        return bool(self.flags & 1)

    @property
    def discontinuity(self) -> bool:
        return bool(self.flags & 2)


def _exact(reader: BinaryIO, size: int) -> bytes:
    data = bytearray()
    while len(data) < size:
        part = reader.read(size - len(data))
        if not part:
            raise EOFError("analytics connection closed")
        data.extend(part)
    return bytes(data)


def read_frame(reader: BinaryIO) -> dict[str, Any] | MediaUnit:
    (length,) = struct.unpack("<I", _exact(reader, 4))
    if not length:
        raise ValueError("empty frame")
    kind = _exact(reader, 1)[0]
    maximum = MAX_MESSAGE + 1 if kind == 1 else MAX_MEDIA + 41 if kind == 2 else 0
    if length > maximum:
        raise ValueError("unknown or oversized frame")
    body = _exact(reader, length - 1)
    if kind == 1:
        value = json.loads(body)
        if not isinstance(value, dict) or value.get("jsonrpc") != "2.0":
            raise ValueError("invalid JSON-RPC envelope")
        return value
    if len(body) < HEADER.size:
        raise ValueError("short media header")
    version, media, flags, binding, seq, time_us, pts, dts = HEADER.unpack_from(body)
    if version != 1 or media != 0:
        raise ValueError("unsupported media header")
    return MediaUnit(
        binding,
        seq,
        time_us,
        body[HEADER.size :],
        flags,
        None if pts == NO_TIME else pts,
        None if dts == NO_TIME else dts,
    )


def encode(frame: dict[str, Any] | MediaUnit) -> bytes:
    if isinstance(frame, MediaUnit):
        if len(frame.data) > MAX_MEDIA:
            raise ValueError("media too large")
        body = (
            b"\2"
            + HEADER.pack(
                1,
                0,
                frame.flags,
                frame.binding,
                frame.seq,
                frame.time_us,
                frame.pts_ns if frame.pts_ns is not None else NO_TIME,
                frame.dts_ns if frame.dts_ns is not None else NO_TIME,
            )
            + frame.data
        )
    else:
        data = json.dumps(
            frame, separators=(",", ":"), ensure_ascii=False, allow_nan=False
        ).encode()
        if len(data) > MAX_MESSAGE:
            raise ValueError("message too large")
        body = b"\1" + data
    return struct.pack("<I", len(body)) + body


class GopCache:
    """A bounded compressed fallback; payload bytes are retained by reference."""

    def __init__(self) -> None:
        self.units: list[MediaUnit] = []
        self.bytes = 0

    def clear(self) -> None:
        self.units.clear()
        self.bytes = 0

    def push(self, unit: MediaUnit) -> None:
        if unit.keyframe or unit.discontinuity:
            self.clear()
        if not self.units and not unit.keyframe:
            return
        if len(self.units) >= 1024 or self.bytes + len(unit.data) > MAX_MEDIA:
            self.clear()
            return
        self.units.append(unit)
        self.bytes += len(unit.data)
