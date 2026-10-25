"""The video decoder choice (``video.decoder``) for Settings.

The recorder applies the choice when it starts: ``auto`` picks the best
decoder it finds, a hardware family (``nvdec``, ``d3d11``, …) is used
whenever the computer has it, ``software`` decodes on the CPU only. It reports
the families it found and the decoder in use in ``system.status``
``video_acceleration`` (``available_decoders``, ``label``,
``decoder_preference_warning``). No Qt here.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from typing import Any

AUTO = "auto"
SOFTWARE = "software"

#: Every value the recorder accepts, with the name Settings shows.
DECODER_LABELS: dict[str, str] = {
    AUTO: "Automatic (recommended)",
    "nvdec": "NVIDIA NVDEC",
    "d3d11": "Direct3D 11",
    "d3d12": "Direct3D 12",
    "qsv": "Intel Quick Sync",
    "vaapi": "VA-API",
    "v4l2": "V4L2",
    SOFTWARE: "CPU only (software)",
}

#: The decoders in use, by ``video_acceleration.kind``, as "Now decoding with" says.
_ACTIVE_LABELS: dict[str, str] = {
    "vaapi": "VA-API",
    "nvdec": "NVIDIA NVDEC",
    "d3d11": "Direct3D 11",
    "d3d12": "Direct3D 12",
    "qsv": "Intel Quick Sync",
    "v4l2_m2m": "V4L2",
    "software": "the CPU (software)",
}


def decoder_from_config(config: object) -> str:
    """The configured choice (the config's ``video.decoder``); ``auto`` when
    missing or unknown."""
    video = config.get("video") if isinstance(config, Mapping) else None
    value = video.get("decoder") if isinstance(video, Mapping) else None
    return value if isinstance(value, str) and value in DECODER_LABELS else AUTO


@dataclass(frozen=True)
class DecoderStatus:
    """What the running recorder reports about decoding."""

    #: The hardware families found when it started: (id, label), in order.
    available: tuple[tuple[str, str], ...] = ()
    #: "Now decoding with …" text, or "" when unknown.
    active: str = ""
    #: Why the chosen decoder is not in use (not on this computer).
    warning: str = ""

    @classmethod
    def from_status(cls, status: object) -> DecoderStatus:
        """From a ``system.status`` (or ``stats.get_performance``) answer;
        an older recorder without these fields gives an empty status."""
        info = status.get("video_acceleration") if isinstance(status, Mapping) else None
        if not isinstance(info, Mapping):
            return cls()
        available: list[tuple[str, str]] = []
        for entry in info.get("available_decoders") or ():
            if not isinstance(entry, Mapping):
                continue
            ident = entry.get("id")
            if isinstance(ident, str) and ident in DECODER_LABELS and ident not in (AUTO, SOFTWARE):
                label = entry.get("label")
                available.append((ident, label if isinstance(label, str) and label else ident))
        kind = str(info.get("kind", ""))
        active = _ACTIVE_LABELS.get(kind, "")
        if kind == "blocked":
            active = str(info.get("label") or "")
        warning = info.get("decoder_preference_warning")
        return cls(
            available=tuple(available),
            active=active,
            warning=warning if isinstance(warning, str) else "",
        )


def decoder_choices(
    current: str, available: Iterable[tuple[str, str]] = ()
) -> list[tuple[str, str]]:
    """The combo's (value, text) rows: Automatic, each hardware decoder found,
    CPU only; a configured decoder not found is kept, marked."""
    rows = [(AUTO, DECODER_LABELS[AUTO])]
    seen = {AUTO, SOFTWARE}
    for ident, label in available:
        if ident not in seen:
            rows.append((ident, label))
            seen.add(ident)
    if current not in seen and current in DECODER_LABELS:
        rows.append((current, f"{DECODER_LABELS[current]} (not available)"))
    rows.append((SOFTWARE, DECODER_LABELS[SOFTWARE]))
    return rows


def restart_reply(result: Any) -> bool:
    """Whether a ``config.set`` answer says the recorder restarts to apply it."""
    return isinstance(result, Mapping) and bool(result.get("restarting"))
