"""Network throughput: reading the host's counters and showing rates.

Counts the interfaces backed by a device (Ethernet, Wi-Fi), as the backend
does (``crates/nw-backend/src/stats/system.rs``), so traffic through bridges,
veth pairs or VPN tunnels is not counted twice; without any, every interface
but loopback.
"""

from __future__ import annotations

import os
import time
from collections.abc import Callable
from dataclasses import dataclass

_UNITS = ("Kbps", "Mbps", "Gbps", "Tbps")
#: Rates are measured over at least this long (seconds).
MIN_WINDOW_S = 1.0


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


def sum_network_bytes(text: str, has_device: Callable[[str], bool]) -> tuple[int, int] | None:
    """Total (received, sent) bytes from ``/proc/net/dev`` contents."""
    interfaces: list[tuple[str, int, int]] = []
    for line in text.splitlines():
        name, sep, counters = line.partition(":")
        if not sep:
            continue
        try:
            fields = [int(value) for value in counters.split()]
        except ValueError:
            continue
        name = name.strip()
        if len(fields) < 9 or name == "lo":
            continue
        interfaces.append((name, fields[0], fields[8]))
    if not interfaces:
        return None
    physical = [entry for entry in interfaces if has_device(entry[0])]
    counted = physical or interfaces
    return sum(e[1] for e in counted), sum(e[2] for e in counted)


def _has_device(name: str) -> bool:
    return os.path.exists(os.path.join("/sys/class/net", name, "device"))


def read_network_bytes() -> tuple[int, int] | None:
    try:
        with open("/proc/net/dev", encoding="utf-8") as dev:
            text = dev.read()
    except OSError:
        return None
    return sum_network_bytes(text, _has_device)


@dataclass(frozen=True)
class Throughput:
    rx_bits_per_second: float
    tx_bits_per_second: float


class NetworkMeter:
    """Turns counter readings into rates; None until two readings a second
    apart (or when the counters cannot be read)."""

    def __init__(
        self,
        read: Callable[[], tuple[int, int] | None] = read_network_bytes,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._read = read
        self._clock = clock
        self._last: tuple[tuple[int, int], float] | None = None
        self._rate: Throughput | None = None

    def sample(self) -> Throughput | None:
        reading = self._read()
        if reading is None:
            return None
        now = self._clock()
        if self._last is None:
            self._last = (reading, now)
            return None
        (rx, tx), then = self._last
        elapsed = now - then
        if elapsed >= MIN_WINDOW_S:
            # A counter that went back (interface reset) counts as no traffic.
            self._rate = Throughput(
                max(0, reading[0] - rx) * 8 / elapsed, max(0, reading[1] - tx) * 8 / elapsed
            )
            self._last = (reading, now)
        return self._rate
