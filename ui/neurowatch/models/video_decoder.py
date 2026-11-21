"""The video decoder choice (``video.decoder``) for Settings.

The recorder applies the choice when it starts: ``auto`` picks the best
decoder it finds, a hardware family (``nvdec``, ``d3d11``, …) is used
whenever the computer has it, ``software`` decodes on the CPU only. It reports
the families it found and the decoder in use in ``system.status``
``video_acceleration`` (``available_decoders``, ``label``,
``decoder_preference_warning``), and why a graphics card that can decode
video does not (``reason_code``, see :func:`acceleration_notice`). No Qt here.
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


#: ``video_acceleration.reason_code`` values meaning "this PC has a graphics
#: card that can decode video, but its driver or software is missing".
DRIVER_MISSING_CODES: frozenset[str] = frozenset({"VAAPI_DRIVER_MISSING", "NVDEC_LIBRARY_MISSING"})

#: The package families of ``video_acceleration.suggested_packages``, as shown.
_PACKAGE_FAMILIES: tuple[tuple[str, str], ...] = (
    ("deb", "Debian/Ubuntu"),
    ("rpm", "Fedora"),
    ("arch", "Arch"),
)

#: The title of the startup message and the tray's problem.
DRIVER_MISSING_TITLE = "Hardware video decoding unavailable"


@dataclass(frozen=True)
class AccelerationNotice:
    """A graphics card that can decode video, decoding on the CPU because
    its driver (or software) is missing: what the user is told."""

    #: The recorder's ``reason_code`` (``VAAPI_DRIVER_MISSING``, …).
    code: str
    #: The plain-language reason: what is missing, what to install.
    text: str
    #: "Intel", "AMD", "NVIDIA", or "" when not reported.
    vendor: str = ""
    #: The card's kernel driver (``i915``, ``amdgpu``), "" when not reported.
    kernel_driver: str = ""
    #: Its render node (``/dev/dri/renderD128``), "" when not reported.
    render_node: str = ""
    #: The packages to install per family: (family label, packages).
    packages: tuple[tuple[str, tuple[str, ...]], ...] = ()

    @property
    def detail(self) -> str:
        """One line per package family: ``Debian/Ubuntu: a b``."""
        return "\n".join(f"{label}: {' '.join(names)}" for label, names in self.packages)

    @property
    def card(self) -> str:
        """``Intel (i915, /dev/dri/renderD128)``, as Diagnostics names it."""
        where = ", ".join(part for part in (self.kernel_driver, self.render_node) if part)
        name = self.vendor or "Unknown"
        return f"{name} ({where})" if where else name

    @property
    def short(self) -> str:
        """The status-bar notice's text."""
        return f"{self.vendor} GPU driver missing" if self.vendor else "GPU driver missing"


def acceleration_notice(info: object) -> AccelerationNotice | None:
    """The notice for a ``video_acceleration`` (from ``system.status`` or
    ``stats.get_performance``) that reports a graphics card without its
    driver; None otherwise, and for an older recorder."""
    if not isinstance(info, Mapping):
        return None
    code = info.get("reason_code")
    reason = info.get("reason")
    if code not in DRIVER_MISSING_CODES or not isinstance(reason, str) or not reason:
        return None
    gpu = info.get("gpu")
    gpu = gpu if isinstance(gpu, Mapping) else {}

    def text(key: str) -> str:
        value = gpu.get(key)
        return value if isinstance(value, str) else ""

    suggested = info.get("suggested_packages")
    packages: list[tuple[str, tuple[str, ...]]] = []
    if isinstance(suggested, Mapping):
        for key, label in _PACKAGE_FAMILIES:
            names = suggested.get(key)
            if isinstance(names, list):
                listed = tuple(name for name in names if isinstance(name, str) and name)
                if listed:
                    packages.append((label, listed))
    return AccelerationNotice(
        code=str(code),
        text=reason,
        vendor=text("vendor"),
        kernel_driver=text("kernel_driver"),
        render_node=text("render_node"),
        packages=tuple(packages),
    )


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
        warnings = [warning] if isinstance(warning, str) and warning else []
        notice = acceleration_notice(info)
        if notice is not None:
            warnings.append(notice.text)  # the graphics card's driver is missing
        return cls(
            available=tuple(available),
            active=active,
            warning=" ".join(warnings),
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
