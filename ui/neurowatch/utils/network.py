"""Showing network rates. The backend measures them (``stats.get_performance``,
``crates/nw-backend/src/stats/system.rs``) on every platform."""

from __future__ import annotations

_UNITS = ("Kbps", "Mbps", "Gbps", "Tbps")


def _digits(value: float) -> int:
    return 2 if value < 10 else 1 if value < 100 else 0


def format_bitrate(bits_per_second: float | None) -> str:
    """``bits_per_second`` in the largest unit that keeps it under 1000:
    950 Kbps, 12.3 Mbps, 1.02 Gbps (1024 Mbps). Decimal units, as network
    speeds are quoted (1 Mbps = 1000 Kbps)."""
    if bits_per_second is None:
        return "—"
    value = max(0.0, float(bits_per_second)) / 1000
    unit = 0
    while unit < len(_UNITS) - 1 and round(value, _digits(value)) >= 1000:
        value /= 1000
        unit += 1
    text = f"{value:.{_digits(value)}f}"
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return f"{text} {_UNITS[unit]}"
