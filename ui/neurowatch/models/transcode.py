"""Transcoding for a browser, as the recorder reports it (no Qt).

A browser watching through NeuroWatch Cloud may not play H.265, so the
recorder can convert the video. ``system.status`` lists each running
conversion (``transcodes``); Diagnostics' ``stream_connections[]`` entries
carry ``transcode`` while one reads that connection. Older recorders send
neither: that reads as nothing running.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

WHY = (
    "A browser watching through NeuroWatch Cloud can't play H.265, so this recorder "
    "converts the video for it. This uses the recorder's graphics card (or processor)."
)
_SOURCES = {"h264": "H.264", "h265": "H.265", "hevc": "H.265"}
_TARGETS = {"h264": "H.264", "vp8": "VP8", "vp9": "VP9", "av1": "AV1"}


@dataclass(frozen=True)
class Transcode:
    source: str
    target: str
    decoder: str
    encoder: str
    viewers: int
    camera_id: str = ""
    camera_name: str = ""
    stream: str = ""

    @property
    def conversion(self) -> str:
        return f"{self.source} → {self.target}"

    @property
    def on_cpu(self) -> bool:
        return "CPU" in (self.decoder, self.encoder)

    @property
    def hardware(self) -> str:
        """``decoded on VA-API · encoded on VA-API``; empty when not reported."""
        parts = []
        if self.decoder:
            parts.append(f"decoded on {self.decoder}")
        if self.encoder:
            parts.append(f"encoded on {self.encoder}")
        return " · ".join(parts)

    @property
    def viewers_text(self) -> str:
        return f"{self.viewers} viewer{'' if self.viewers == 1 else 's'}"

    @property
    def detail(self) -> str:
        return " · ".join(p for p in (self.hardware, self.viewers_text) if p)

    @property
    def line(self) -> str:
        """One tooltip line: ``Front door · Main · H.265 → H.264 · …``."""
        stream = {"main": "Main", "sub": "Sub"}.get(self.stream, self.stream.capitalize())
        name = self.camera_name or self.camera_id
        return " · ".join(p for p in (name, stream, self.conversion, self.detail) if p)


def _text(value: object) -> str:
    return value.strip() if isinstance(value, str) else ""


def parse_transcode(value: object) -> Transcode | None:
    """One report (a ``transcodes`` item or a connection's ``transcode``)."""
    if not isinstance(value, dict):
        return None
    raw_source = _text(value.get("from")).lower()
    source = _SOURCES.get(raw_source, raw_source.upper())
    raw_target = _text(value.get("to")).lower()
    target = _TARGETS.get(raw_target, raw_target.upper())
    if not source or not target:
        return None
    viewers = value.get("viewers")
    return Transcode(
        source=source,
        target=target,
        decoder=_text(value.get("decoder")),
        encoder=_text(value.get("encoder")),
        viewers=viewers if isinstance(viewers, int) and not isinstance(viewers, bool) else 0,
        camera_id=_text(value.get("camera_id")),
        camera_name=_text(value.get("camera_name")),
        stream=_text(value.get("stream")),
    )


def parse_transcodes(status: object) -> list[Transcode]:
    """The running conversions of a ``system.status`` (none when it says nothing)."""
    items: Any = status.get("transcodes") if isinstance(status, dict) else None
    if not isinstance(items, list):
        return []
    return [t for t in map(parse_transcode, items) if t is not None]


def summary(transcodes: list[Transcode]) -> str:
    """The status bar's words; empty when nothing is converted."""
    if not transcodes:
        return ""
    count = len(transcodes)
    cpu = any(t.on_cpu for t in transcodes)
    targets = sorted({t.target for t in transcodes})
    if count == 1:
        (only,) = transcodes
        text = "Transcoding 1 camera for a browser"
        if cpu:
            return f"{text} on the CPU ({only.target})"
        return f"{text} ({only.conversion})"
    text = f"Transcoding {count} cameras for browsers"
    return f"{text} on the CPU ({' / '.join(targets)})" if cpu else text


def tooltip(transcodes: list[Transcode]) -> str:
    if not transcodes:
        return ""
    return "\n".join([*(t.line for t in transcodes), "", WHY, "Click for Diagnostics."])
