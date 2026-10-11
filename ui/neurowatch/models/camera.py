"""Camera configuration model."""

from __future__ import annotations

import uuid
from dataclasses import asdict, dataclass, field


def connection_status(state: str) -> str:
    """Map pipeline readiness to connectivity without changing the IPC contract.

    recording_ready means a streaming pipeline opened its recording segment;
    recording.status_all remains responsible for the separate recording badge.
    """
    return "streaming" if state == "recording_ready" else state


@dataclass
class WallPosition:
    """Top-left cell of a camera's wall tile, and the cells it covers."""

    page: int = 0
    row: int = 0
    col: int = 0
    row_span: int = 1
    col_span: int = 1


@dataclass
class LocalRecordingDest:
    enabled: bool = False
    storage_path: str | None = None
    quota_gb: int | None = None


@dataclass
class CameraRecordingConfig:
    schedule: str | None = None
    local: LocalRecordingDest = field(default_factory=LocalRecordingDest)


MAIN_STREAM = "main"
SUB_STREAM = "sub"


@dataclass
class CameraStream:
    """One selectable video stream of a camera (an ONVIF profile or a URL).

    ``id`` is ``"1"``, ``"2"``, … in list order, never a role name.
    """

    id: str
    url: str
    name: str = ""
    width: int | None = None
    height: int | None = None

    @property
    def pixels(self) -> int:
        return (self.width or 0) * (self.height or 0)

    def label(self) -> str:
        """Menu text such as ``Stream 2 · 640×360 · SubStream``."""
        parts = [f"Stream {self.id}"]
        if self.width and self.height:
            parts.append(f"{self.width}×{self.height}")
        if self.name:
            parts.append(self.name)
        return " · ".join(parts)


@dataclass
class DiscoveryIdentity:
    """How a camera was found by a network search (backend `DiscoveryIdentity`).

    Kept so later searches show the camera as already added; ``source`` is the
    ONVIF profile token, which tells an NVR's channels apart.
    """

    endpoint_reference: str
    source: str | None = None


@dataclass
class CameraConfig:
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    name: str = ""
    url: str = ""
    sub_url: str = ""
    username: str = ""
    password_b64: str = ""
    transport: str = "tcp"
    protocol: str = "auto"
    notes: str = ""
    wall_position: WallPosition | None = None
    recording: CameraRecordingConfig = field(default_factory=CameraRecordingConfig)
    # Every stream the camera offers. ``url`` stays the highest-resolution
    # stream (recorded, single-view default), ``sub_url`` the grid default.
    streams: list[CameraStream] = field(default_factory=list)
    # Absent for cameras entered by hand.
    discovery: DiscoveryIdentity | None = None

    def stream_options(self) -> list[CameraStream]:
        """Streams a viewer may pick; main and sub for a camera without a list."""
        if self.streams:
            return list(self.streams)
        options = [CameraStream(id="1", url=self.url, name="Main")]
        if self.sub_url:
            options.append(CameraStream(id="2", url=self.sub_url, name="Sub"))
        return options

    @property
    def tile_span(self) -> tuple[int, int]:
        """(rows, cols) of grid cells this camera's wall tile covers."""
        wp = self.wall_position
        if wp is None:
            return (1, 1)
        return (max(1, int(wp.row_span)), max(1, int(wp.col_span)))

    def set_tile_span(self, span: tuple[int, int]) -> None:
        if self.wall_position is None:
            self.wall_position = WallPosition()
        self.wall_position.row_span, self.wall_position.col_span = span

    def to_dict(self) -> dict:
        d = asdict(self)
        if d.get("discovery") is None:
            d.pop("discovery", None)
        return d

    @classmethod
    def from_dict(cls, d: dict) -> CameraConfig:
        data = dict(d)
        wp = data.pop("wall_position", None)
        rec = data.pop("recording", None)
        streams = data.pop("streams", None)
        discovery = data.pop("discovery", None)
        data.pop("motion", None)  # ignore motion for now
        obj = cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})
        if isinstance(wp, dict):
            fields = WallPosition.__dataclass_fields__
            obj.wall_position = WallPosition(**{k: v for k, v in wp.items() if k in fields})
        if isinstance(discovery, dict) and isinstance(discovery.get("endpoint_reference"), str):
            source = discovery.get("source")
            obj.discovery = DiscoveryIdentity(
                endpoint_reference=discovery["endpoint_reference"],
                source=source if isinstance(source, str) else None,
            )
        if isinstance(streams, list):
            obj.streams = [
                CameraStream(
                    id=str(item.get("id", index + 1)),
                    url=str(item.get("url", "")),
                    name=str(item.get("name") or ""),
                    width=item.get("width"),
                    height=item.get("height"),
                )
                for index, item in enumerate(streams)
                if isinstance(item, dict) and item.get("url")
            ]
        if isinstance(rec, dict):
            # Support both new nested format and old flat format (migration)
            local_d = rec.get("local") or {}
            if not local_d and ("enabled" in rec or "storage_path" in rec):
                local_d = {
                    "enabled": rec.get("enabled", False),
                    "storage_path": rec.get("storage_path"),
                    "quota_gb": rec.get("quota_gb"),
                }
            # A legacy config/RPC payload may still carry a "s3" object from
            # the removed direct-S3 feature; it is intentionally discarded
            # here rather than round-tripped, so its credentials are never
            # persisted back to disk.
            obj.recording = CameraRecordingConfig(
                schedule=rec.get("schedule"),
                local=LocalRecordingDest(
                    enabled=local_d.get("enabled", False),
                    storage_path=local_d.get("storage_path"),
                    quota_gb=local_d.get("quota_gb"),
                ),
            )
        return obj
