"""Camera search results from the backend (``camera.discovery.*``).

The backend searches the network and talks ONVIF; these types only read its
answers for the Add Camera dialog. Unknown fields and states are tolerated so
an older UI keeps working with a newer backend.
"""

from __future__ import annotations

import ipaddress
import re
from dataclasses import dataclass, field

from neurowatch.models.camera import CameraStream

MAX_SCAN_ADDRESSES = 1024

_STATUS = {
    "ready": "Ready",
    "credentials_needed": "Needs password",
    "unavailable": "Unavailable",
}
_ADDABLE = {"ready", "credentials_needed", "possible_match"}


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


def _number(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) and value > 0 else None


@dataclass(frozen=True)
class Candidate:
    """A device that answered a search."""

    candidate_id: str
    address: str
    state: str
    manufacturer: str | None = None
    model: str | None = None
    name: str | None = None
    firmware: str | None = None
    matched_name: str | None = None
    endpoint_reference: str | None = None
    #: How the recorder found it: "onvif", "sadp" (Hikvision), "rtsp".
    found_by: tuple[str, ...] = field(default=())
    serial: str | None = None
    mac: str | None = None
    #: False: a Hikvision device whose admin password was never set.
    activated: bool | None = None
    #: Answers RTSP only; it can be added with a typed stream URL.
    stream_url_needed: bool = False
    note: str | None = None

    @classmethod
    def from_dict(cls, raw: dict) -> Candidate:
        matched = raw.get("matched_camera")
        found_by = raw.get("found_by")
        activated = raw.get("activated")
        return cls(
            candidate_id=str(raw.get("candidate_id", "")),
            address=str(raw.get("address", "")),
            state=str(raw.get("state", "")),
            manufacturer=_text(raw.get("manufacturer")),
            model=_text(raw.get("model")),
            name=_text(raw.get("name")),
            firmware=_text(raw.get("firmware")),
            matched_name=_text(matched.get("name")) if isinstance(matched, dict) else None,
            endpoint_reference=_text(raw.get("endpoint_reference")),
            found_by=tuple(str(f) for f in found_by if isinstance(f, str))
            if isinstance(found_by, list)
            else (),
            serial=_text(raw.get("serial")),
            mac=_text(raw.get("mac")),
            activated=activated if isinstance(activated, bool) else None,
            stream_url_needed=raw.get("stream_url_needed") is True,
            note=_text(raw.get("note")),
        )

    @property
    def title(self) -> str:
        brand = " ".join(part for part in (self.manufacturer, self.model) if part)
        if self.name or brand:
            return self.name or brand
        if self.found_by and "onvif" not in self.found_by:
            return "RTSP device" if "rtsp" in self.found_by else "Camera"
        return "ONVIF device"

    @property
    def details(self) -> str:
        """A tooltip: maker and model, serial number, how it was found, and the note."""
        how = {
            "onvif": "ONVIF",
            "sadp": "Hikvision SADP",
            "rtsp": "RTSP port",
        }
        lines = [
            " ".join(p for p in (self.manufacturer, self.model) if p),
            f"Serial number {self.serial}" if self.serial else "",
            f"MAC {self.mac}" if self.mac else "",
            "Found by " + ", ".join(how.get(f, f) for f in self.found_by) if self.found_by else "",
            self.note or "",
        ]
        return "\n".join(line for line in lines if line)

    @property
    def status_text(self) -> str:
        if self.activated is False:
            return "Not activated"
        if self.stream_url_needed:
            return "Needs stream URL"
        if self.state == "already_added":
            return f"Added as {self.matched_name}" if self.matched_name else "Already added"
        if self.state == "possible_match":
            return f"May be {self.matched_name}" if self.matched_name else "Possible match"
        return _STATUS.get(self.state, "Unknown")

    @property
    def can_add(self) -> bool:
        return self.state in _ADDABLE

    @property
    def can_set_up(self) -> bool:
        """Can be set up by hand: an addable device, or one answering RTSP
        only (with its stream URL typed)."""
        return self.can_add or self.stream_url_needed


@dataclass(frozen=True)
class StreamChoice:
    """One stream a device offers. ``url`` has no credentials."""

    stream_id: str
    name: str
    width: int | None = None
    height: int | None = None
    encoding: str | None = None
    source: str | None = None
    url: str | None = None

    @classmethod
    def from_dict(cls, raw: dict) -> StreamChoice:
        return cls(
            stream_id=str(raw.get("stream_id", "")),
            name=str(raw.get("name") or ""),
            width=_number(raw.get("width")),
            height=_number(raw.get("height")),
            encoding=_text(raw.get("encoding")),
            source=_text(raw.get("source")),
            url=_text(raw.get("url")),
        )

    @property
    def pixels(self) -> int:
        return (self.width or 0) * (self.height or 0)

    @property
    def label(self) -> str:
        size = f"{self.width}×{self.height}" if self.width and self.height else None
        return " · ".join(part for part in (self.name, size, self.encoding) if part)


# ---------------------------------------------------------------------------
# Typed searches: the same rules as the backend (``net::HostSet::parse``).
# ---------------------------------------------------------------------------

#: Most networks, ranges and addresses one search may list.
MAX_RANGE_PARTS = 64
_EXAMPLES = "Try 192.168.1.x, 192.168.1.0/24, 192.168.1.10-50 or 192.168.1.64."
_WILDCARDS = {"*", "x", "X"}


@dataclass(frozen=True)
class SearchRange:
    """What a typed search covers. ``spans`` are inclusive (first, last)
    address pairs, sorted and merged."""

    label: str
    spans: tuple[tuple[int, int], ...]

    @property
    def count(self) -> int:
        return sum(last - first + 1 for first, last in self.spans)

    @property
    def first(self) -> str:
        return str(ipaddress.IPv4Address(self.spans[0][0]))

    @property
    def last(self) -> str:
        return str(ipaddress.IPv4Address(self.spans[-1][1]))

    @property
    def single(self) -> str | None:
        """The address, when the search is one device."""
        return self.first if self.count == 1 else None

    @property
    def summary(self) -> str:
        """``Searches 192.168.1.1 – 192.168.1.254 · 254 addresses``."""
        if self.single:
            return f"Searches {self.single} only"
        if len(self.spans) == 1:
            return f"Searches {self.first} – {self.last} · {self.count} addresses"
        return (
            f"Searches {self.count} addresses in {len(self.spans)} ranges, "
            f"{self.first} – {self.last}"
        )


def _octet(text: str) -> int | None:
    if not text or len(text) > 3 or not text.isascii() or not text.isdigit():
        return None
    if len(text) > 1 and text.startswith("0"):
        return None
    value = int(text)
    return value if value <= 255 else None


def _address(text: str) -> int | None:
    parts = text.split(".")
    if len(parts) != 4:
        return None
    value = 0
    for part in parts:
        octet = _octet(part)
        if octet is None:
            return None
        value = value << 8 | octet
    return value


def _pieces(text: str) -> list[str]:
    text = text.replace("–", "-").replace("—", "-")
    text = re.sub(r"\s*-\s*", "-", text)
    return [piece for piece in re.split(r"[,;\s]+", text) if piece]


def _network(base: int, prefix: int) -> tuple[str, tuple[int, int], tuple[int, int]]:
    """(label, hosts, bounds) of the network of ``base``."""
    mask = (0xFFFFFFFF << (32 - prefix)) & 0xFFFFFFFF if prefix else 0
    first = base & mask
    last = first | (~mask & 0xFFFFFFFF)
    hosts = (first + 1, last - 1) if prefix <= 30 else (first, last)
    label = str(ipaddress.IPv4Address(first)) + ("" if prefix == 32 else f"/{prefix}")
    return label, hosts, (first, last)


def _part(piece: str) -> tuple[str, tuple[int, int], tuple[int, int]]:
    """(label, hosts, bounds) of one typed piece; ValueError says why not."""
    bad = ValueError(f"“{piece}” is not a network, range or address. {_EXAMPLES}")
    if "/" in piece:
        address, _, prefix = piece.partition("/")
        base = _address(address)
        if base is None or not prefix.isascii() or not prefix.isdigit() or len(prefix) > 2:
            raise bad
        if int(prefix) > 32:
            raise bad
        return _network(base, int(prefix))
    if "-" in piece:
        start_text, _, end_text = piece.partition("-")
        start = _address(start_text)
        if start is None:
            raise bad
        end = _address(end_text)
        if end is None:
            last = _octet(end_text)
            if last is None:
                raise bad
            end = (start & 0xFFFFFF00) | last
        if end < start:
            raise ValueError(
                f"“{piece}” ends before it starts. Put the lower address first, "
                "e.g. 192.168.1.10-50."
            )
        low, high = ipaddress.IPv4Address(start), ipaddress.IPv4Address(end)
        if start == end:
            label = str(low)
        elif start >> 8 == end >> 8:
            label = f"{low}-{end & 0xFF}"
        else:
            label = f"{low}-{high}"
        return label, (start, end), (start, end)
    octets = piece.split(".")
    if len(octets) == 3 or (len(octets) == 4 and (octets[3] in _WILDCARDS or octets[3] == "0")):
        values = [_octet(o) for o in octets[:3]]
        if any(v is None for v in values):
            raise bad
        first, second, third = (v or 0 for v in values)
        return _network(first << 24 | second << 16 | third << 8, 24)
    base = _address(piece)
    if base is None:
        raise bad
    return _network(base, 32)


def _special(value: int) -> bool:
    address = ipaddress.IPv4Address(value)
    return (
        address.is_loopback
        or address.is_multicast
        or address.is_unspecified
        or value == 0xFFFFFFFF
        or address.is_link_local
        or value >= 0xF0000000
    )


def parse_range(text: str) -> SearchRange:
    """A typed search: networks (``192.168.1.0/24``, ``192.168.1``,
    ``192.168.1.x``), ranges (``192.168.1.10-50``) and addresses, separated
    by commas. ValueError says what is wrong, as the backend would."""
    pieces = _pieces(text)
    if not pieces:
        raise ValueError(
            "Enter a network, range or address to search, e.g. 192.168.1.x or 192.168.1.10-50."
        )
    if len(pieces) > MAX_RANGE_PARTS:
        raise ValueError(
            f"Enter at most {MAX_RANGE_PARTS} networks, ranges or addresses at a time."
        )
    parts = [_part(piece) for piece in pieces]
    spans: list[tuple[int, int]] = []
    for first, last in sorted(hosts for _label, hosts, _bounds in parts):
        if spans and first <= spans[-1][1] + 1:
            spans[-1] = (spans[-1][0], max(spans[-1][1], last))
        else:
            spans.append((first, last))
    labels = [label for label, _hosts, _bounds in parts]
    shown = labels[:4] + ([f"and {len(labels) - 4} more"] if len(labels) > 4 else [])
    found = SearchRange(label=", ".join(shown), spans=tuple(spans))
    if found.count > MAX_SCAN_ADDRESSES:
        raise ValueError(
            f"{found.label} covers {found.count} addresses; a search covers at most "
            f"{MAX_SCAN_ADDRESSES} (for example 192.168.1.x, or a /22). Search a smaller range."
        )
    for label, _hosts, bounds in parts:
        if any(_special(v) for v in bounds):
            raise ValueError(f"{label} cannot be searched: it is not a camera network")
    return found


def check_range(text: str) -> str | None:
    """Why ``text`` cannot be searched, or None. The backend checks again."""
    try:
        parse_range(text)
    except ValueError as e:
        return str(e)
    return None


def widen_to_network(address: str) -> str:
    """``192.168.1.64`` → ``192.168.1.x``: the network a lone camera is on."""
    return address.rsplit(".", 1)[0] + ".x"


@dataclass(frozen=True)
class RangeSuggestion:
    """A one-click search: ``text`` goes in the box, ``label`` on the chip."""

    text: str
    label: str
    tooltip: str


#: Where cameras sit out of the box (before anyone gives them an address).
FACTORY_NETWORKS = (
    RangeSuggestion(
        "192.168.1.x",
        "192.168.1.x",
        "Factory network of Hikvision (192.168.1.64) and Dahua (192.168.1.108) cameras",
    ),
    RangeSuggestion("192.168.0.x", "192.168.0.x", "A common factory and home-router network"),
)


def range_suggestions(interfaces: list[dict]) -> list[RangeSuggestion]:
    """Chips for the recorder's own networks, then common camera factory
    networks, each network once."""
    found: list[RangeSuggestion] = []
    seen: set[str] = set()
    for iface in interfaces:
        cidr, address = iface.get("cidr"), iface.get("address")
        if not isinstance(cidr, str) or not isinstance(address, str):
            continue
        try:
            network = ipaddress.IPv4Network(cidr, strict=False)
        except ValueError:
            continue
        if network.prefixlen >= 24 or network.num_addresses > MAX_SCAN_ADDRESSES + 2:
            # Small enough as it is, or too big: the /24 around the recorder.
            text = widen_to_network(address) if network.prefixlen <= 24 else cidr
        else:
            text = cidr
        if text in seen:
            continue
        seen.add(text)
        name = str(iface.get("name") or "")
        tooltip = f"This recorder's network ({name})" if name else "This recorder's network"
        factory = next((f for f in FACTORY_NETWORKS if f.text == text), None)
        if factory is not None:
            tooltip = f"{tooltip}. {factory.tooltip}"
        found.append(RangeSuggestion(text, text, tooltip))
    for factory in FACTORY_NETWORKS:
        if factory.text not in seen:
            seen.add(factory.text)
            found.append(factory)
    return found


def discovered_streams(choices: list[StreamChoice]) -> list[CameraStream]:
    """The camera's stream list: highest resolution first, as most NVRs number them."""
    with_url = [c for c in choices if c.url]
    ordered = sorted(with_url, key=lambda c: -c.pixels)
    return [
        CameraStream(
            id=str(index + 1), url=c.url or "", name=c.name, width=c.width, height=c.height
        )
        for index, c in enumerate(ordered)
    ]
