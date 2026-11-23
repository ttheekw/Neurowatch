"""Analytics plugins (``plugins.get|set|status``): the daemons the recorder
connects to and the cameras bound to each.

No Qt: Settings → Plugins and its dialogs use it, and so do tests.
"""

from __future__ import annotations

import os
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

from neurowatch.models.plugin_decoding import (
    DecodingLines,
    SharedSource,
    decoding_lines,
    parse_shared_source,
)

#: Plugin ids the recorder accepts (``plugin_host::config``).
_ID = re.compile(r"^[A-Za-z0-9._-]{1,64}$")


def default_motion_address() -> str:
    """Where ``nw-motion`` listens unless told otherwise."""
    if os.name == "nt":
        return r"pipe:\\.\pipe\neurowatch-motion"
    runtime = os.environ.get("XDG_RUNTIME_DIR") or "/tmp"
    return f"unix:{runtime}/neurowatch/plugins/nw-motion.sock"


def server_id_problem(ident: str, taken: list[str]) -> str | None:
    if not _ID.match(ident):
        return "Use 1–64 letters, digits, dots, dashes or underscores, without spaces."
    if ident in taken:
        return "Another plugin already has this name."
    return None


def address_problem(address: str) -> str | None:
    """Why the recorder would refuse ``address`` (it checks again)."""
    address = address.strip()
    address = address.replace("quic://", "quic:", 1).replace("tls://", "tls:", 1)
    if address.startswith(("quic:", "tls:")):
        endpoint = re.fullmatch(
            r"(?:quic|tls):(\[[0-9a-fA-F:]+\]|[A-Za-z0-9.-]+):([0-9]{1,5})", address
        )
        if endpoint and 0 < int(endpoint[2]) <= 65535:
            return None
        return "Use quic:hostname:port or tls:hostname:port, with a port from 1 to 65535."
    if address.startswith("unix:"):
        return None if len(address) > len("unix:") else "Give the socket's path after unix:."
    if address.startswith("tcp:"):
        host = address[len("tcp:") :].rsplit(":", 1)[0].strip("[]")
        if host in ("127.0.0.1", "::1", "localhost"):
            return None
        return "Plain TCP is for plugins on this computer only (127.0.0.1)."
    if address.startswith("pipe:"):
        path = address[5:]
        return (
            None
            if path.startswith("\\\\.\\pipe\\")
            and len(path) > 9
            and not any(c in path[9:] for c in "\\/\0")
            else "Use a local Windows pipe: pipe:\\\\.\\pipe\\name."
        )
    return "Use unix:/path, pipe:, loopback tcp:, or encrypted quic:host:port / tls:host:port."


@dataclass
class PluginServer:
    id: str
    address: str
    enabled: bool = True
    approval: dict[str, Any] | None = None
    credential_revision: int = 0
    plugin_id: str | None = None
    #: Provided by NeuroWatch: the portal decides, nothing is editable here.
    managed: dict[str, Any] | None = None

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {"id": self.id, "address": self.address, "enabled": self.enabled}
        if self.approval is not None:
            out["approval"] = self.approval
        if self.credential_revision:
            out["credential_revision"] = self.credential_revision
        if self.plugin_id:
            out["plugin_id"] = self.plugin_id
        if self.managed is not None:
            # The recorder keeps hosted plugins as the portal set them; sent
            # back without this, it read as a second plugin of that name.
            out["managed"] = self.managed
        return out

    def shown_name(self, manifest_name: str | None = None) -> str:
        """A hosted plugin's catalog name, else the plugin's own, else the id."""
        label = self.managed.get("label") if self.managed else None
        for name in (label, manifest_name):
            if isinstance(name, str) and name.strip():
                return name.strip()
        return self.id


#: Which of the camera's streams a plugin gets: None is the plugin's choice.
STREAM_CHOICES = (None, "sub", "main")


@dataclass
class PluginBinding:
    server: str
    camera_id: str
    enabled: bool = True
    config: dict[str, Any] = field(default_factory=dict)
    #: ``"sub"``, ``"main"``, or None: the plugin's choice.
    stream: str | None = None
    #: Drawing layers shown on the live wall; None: the plugin's defaults.
    wall_layers: list[str] | None = None
    #: The plugin always decodes the camera itself; off: it uses NeuroWatch's
    #: decoded pictures whenever there are any (automatic).
    own_decoder: bool = False
    #: A hosted plugin no longer covers this camera (read only: set by the backend).
    withdrawn: bool = False

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "server": self.server,
            "camera_id": self.camera_id,
            "enabled": self.enabled,
            "config": self.config,
        }
        if self.stream is not None:
            out["stream"] = self.stream
        if self.wall_layers is not None:
            out["wall_layers"] = list(self.wall_layers)
        if self.own_decoder:
            out["own_decoder"] = True
        return out


@dataclass
class PluginsConfig:
    servers: list[PluginServer] = field(default_factory=list)
    bindings: list[PluginBinding] = field(default_factory=list)

    @classmethod
    def from_dict(cls, raw: object) -> PluginsConfig:
        raw = raw if isinstance(raw, dict) else {}

        def items(key: str) -> list[Any]:
            value = raw.get(key)
            return value if isinstance(value, list) else []

        servers = [
            PluginServer(
                str(s["id"]),
                str(s["address"]),
                bool(s.get("enabled", True)),
                s.get("approval") if isinstance(s.get("approval"), dict) else None,
                int(s.get("credential_revision") or 0),
                str(s["plugin_id"]) if s.get("plugin_id") else None,
                s["managed"] if isinstance(s.get("managed"), dict) else None,
            )
            for s in items("servers")
            if isinstance(s, dict) and "id" in s and "address" in s
        ]
        bindings = [
            PluginBinding(
                str(b["server"]),
                str(b["camera_id"]),
                bool(b.get("enabled", True)),
                dict(b["config"]) if isinstance(b.get("config"), dict) else {},
                b.get("stream") if b.get("stream") in ("sub", "main") else None,
                [str(layer) for layer in b["wall_layers"]]
                if isinstance(b.get("wall_layers"), list)
                else None,
                b.get("own_decoder") is True,
                b.get("withdrawn") is True,
            )
            for b in items("bindings")
            if isinstance(b, dict) and "server" in b and "camera_id" in b
        ]
        return cls(servers, bindings)

    def to_dict(self) -> dict[str, Any]:
        return {
            "servers": [s.to_dict() for s in self.servers],
            "bindings": [b.to_dict() for b in self.bindings],
        }

    def bindings_of(self, server: str) -> list[PluginBinding]:
        return [b for b in self.bindings if b.server == server]

    def shown_servers(self) -> list[PluginServer]:
        """Plugins to list: not a hosted one NeuroWatch no longer provides
        (kept switched off only so its camera settings return with it)."""
        return [s for s in self.servers if s.managed is None or s.enabled]

    def shown_bindings_of(self, server: str) -> list[PluginBinding]:
        """A plugin's cameras to list: not those a hosted plugin no longer covers."""
        return [b for b in self.bindings_of(server) if not b.withdrawn]

    def binding(self, server: str, camera_id: str) -> PluginBinding | None:
        return next(
            (b for b in self.bindings if b.server == server and b.camera_id == camera_id), None
        )

    def without_server(self, server: str) -> PluginsConfig:
        return PluginsConfig(
            [s for s in self.servers if s.id != server],
            [b for b in self.bindings if b.server != server],
        )

    def with_binding(self, binding: PluginBinding) -> PluginsConfig:
        """This config with ``binding`` added, or replacing the camera's one."""
        others = [
            b
            for b in self.bindings
            if not (b.server == binding.server and b.camera_id == binding.camera_id)
        ]
        return PluginsConfig(list(self.servers), [*others, binding])

    def without_binding(self, server: str, camera_id: str) -> PluginsConfig:
        return PluginsConfig(
            list(self.servers),
            [b for b in self.bindings if not (b.server == server and b.camera_id == camera_id)],
        )

    def with_server_enabled(self, server: str, enabled: bool) -> PluginsConfig:
        return PluginsConfig(
            [
                PluginServer(
                    s.id,
                    s.address,
                    enabled if s.id == server else s.enabled,
                    s.approval,
                    s.credential_revision,
                    s.plugin_id,
                )
                for s in self.servers
            ],
            list(self.bindings),
        )


# ---- Status ---------------------------------------------------------------


@dataclass
class Manifest:
    id: str = ""
    name: str = ""
    vendor: str = ""
    version: str = ""
    config_schema: dict[str, Any] | None = None
    #: Drawing layers: (id, label, shown on the wall by default).
    layers: tuple[tuple[str, str, bool], ...] = ()
    public_key: str = ""
    permissions: tuple[str, ...] = ()

    @property
    def uses_recorder_pictures(self) -> bool:
        """Whether the plugin can use NeuroWatch's decoded pictures."""
        return "decoded_frames" in self.permissions

    web_ui: dict[str, Any] | None = None

    @classmethod
    def from_dict(cls, raw: object) -> Manifest | None:
        if not isinstance(raw, dict):
            return None
        schema = raw.get("config_schema")
        layers = raw.get("overlay_layers")
        identity = raw.get("identity")
        return cls(
            str(raw.get("id", "")),
            str(raw.get("name", "")),
            str(raw.get("vendor", "")),
            str(raw.get("version", "")),
            schema if isinstance(schema, dict) else None,
            tuple(
                (
                    str(layer["id"]),
                    str(layer.get("label") or layer["id"]),
                    bool(layer.get("default_on")),
                )
                for layer in (layers if isinstance(layers, list) else [])
                if isinstance(layer, dict) and "id" in layer
            ),
            str(identity.get("public_key") or "") if isinstance(identity, dict) else "",
            tuple(str(value) for value in raw.get("permissions", []) if isinstance(value, str)),
            raw.get("web_ui") if isinstance(raw.get("web_ui"), dict) else None,
        )

    @property
    def reports_events(self) -> bool:
        """Whether the plugin can send events: it asks to. A plugin that does
        not has no event history to show. (No list at all: assume it may.)"""
        return not self.permissions or "events" in self.permissions

    def wall_layers(self, binding: PluginBinding) -> list[str]:
        """The layers ``binding`` shows on the live wall."""
        if binding.wall_layers is not None:
            return list(binding.wall_layers)
        return [ident for ident, _label, on in self.layers if on]


@dataclass
class BindingStatus:
    state: str = ""
    detail: str = ""
    sent: int = 0
    dropped: int = 0
    plugin_state: str = ""
    plugin_detail: str = ""
    fps: float = 0.0
    decoder: str = ""
    #: The camera stream it gets (``sub``/``main``) and its format.
    stream: str = ""
    format: str = ""
    #: NeuroWatch shares its decoded pictures with the plugin now, how many a
    #: second, and which of its decoders made them on what (newer recorders).
    shared_pictures: bool = False
    shared_fps: float | None = None
    shared_source: SharedSource | None = None


@dataclass
class ServerStatus:
    id: str
    state: str = "connecting"
    detail: str = ""
    manifest: Manifest | None = None
    bindings: dict[str, BindingStatus] = field(default_factory=dict)


@dataclass(frozen=True)
class PluginConnectionNotice:
    server: str
    text: str
    tone: str


def connection_notices(
    config: PluginsConfig,
    statuses: dict[str, ServerStatus],
    previous: Sequence[PluginConnectionNotice] = (),
) -> list[PluginConnectionNotice]:
    """One message per enabled plugin; retries retain failures until recovery."""
    before = {notice.server: notice for notice in previous}
    notices: list[PluginConnectionNotice] = []
    for server in config.servers:
        status = statuses.get(server.id)
        if not server.enabled or status is None or status.state in ("disabled", "pending_approval"):
            continue
        old = before.get(server.id)
        name = server.shown_name(status.manifest.name if status.manifest is not None else None)
        if status.state == "connected":
            if old is not None and old.tone == "success":
                notices.append(old)
            else:
                verb = "reconnected" if old else "connected"
                notices.append(PluginConnectionNotice(server.id, f"{name} {verb}.", "success"))
        elif status.state in ("connecting", "authorizing"):
            if old is not None:
                notices.append(
                    old
                    if old.tone == "error"
                    else PluginConnectionNotice(
                        server.id, f"{name} disconnected. The recorder keeps trying.", "error"
                    )
                )
        else:
            verb = "disconnected" if old else "unavailable"
            if old is not None and old.text.startswith(f"{name} unavailable"):
                verb = "unavailable"
            detail = f": {status.detail}" if status.detail else ""
            notices.append(
                PluginConnectionNotice(
                    server.id, f"{name} {verb}{detail}. The recorder keeps trying.", "error"
                )
            )
    return notices


def parse_status(result: object) -> dict[str, ServerStatus]:
    """``plugins.status`` by plugin id."""
    plugins = result.get("plugins") if isinstance(result, dict) else None
    statuses: dict[str, ServerStatus] = {}
    for raw in plugins if isinstance(plugins, list) else []:
        if not isinstance(raw, dict) or "id" not in raw:
            continue
        bindings: dict[str, BindingStatus] = {}
        views = raw.get("bindings")
        for camera_id, view in (views if isinstance(views, dict) else {}).items():
            if not isinstance(view, dict):
                continue
            reported = view.get("plugin")
            plugin: dict[str, Any] = reported if isinstance(reported, dict) else {}
            shared = view.get("shared_pictures") is True
            rate = view.get("shared_fps")
            bindings[str(camera_id)] = BindingStatus(
                state=str(view.get("state", "")),
                detail=str(view.get("detail") or ""),
                sent=int(view.get("sent") or 0),
                dropped=int(view.get("dropped") or 0),
                plugin_state=str(plugin.get("state", "")),
                plugin_detail=str(plugin.get("detail") or ""),
                fps=float(plugin.get("fps") or 0.0),
                decoder=str(plugin.get("decoder") or ""),
                stream=str(view.get("stream") or ""),
                format=str(view.get("format") or ""),
                shared_pictures=shared,
                shared_fps=float(rate) if shared and isinstance(rate, (int, float)) else None,
                shared_source=parse_shared_source(view.get("shared_source")) if shared else None,
            )
        statuses[str(raw["id"])] = ServerStatus(
            id=str(raw["id"]),
            state=str(raw.get("state", "connecting")),
            detail=str(raw.get("detail") or ""),
            manifest=Manifest.from_dict(raw.get("manifest")),
            bindings=bindings,
        )
    return statuses


def server_text(server: PluginServer, status: ServerStatus | None) -> tuple[str, str]:
    """(text, tone) for a plugin's connection."""
    if not server.enabled:
        return ("Off. The recorder does not connect to it.", "muted")
    if status is None or status.state == "connecting":
        return ("Connecting…", "info")
    if status.state == "pending_approval":
        return ("Approve this plugin's key and permissions before it receives cameras.", "warning")
    if status.state == "authorizing":
        return ("Checking authorization…", "info")
    if status.state == "connected":
        manifest = status.manifest
        if manifest is None:
            return ("Connected.", "success")
        by = f" by {manifest.vendor}" if manifest.vendor else ""
        return (f"Connected · {manifest.name or manifest.id} {manifest.version}{by}", "success")
    detail = f": {status.detail}" if status.detail else ""
    return (f"Not reachable{detail}. Is the plugin running? The recorder keeps trying.", "error")


def binding_decoding(status: BindingStatus | None, plugin_name: str) -> DecodingLines | None:
    """Who decodes the binding's pictures and who scales them down; None
    until the binding runs."""
    if status is None or status.state != "active":
        return None
    return decoding_lines(
        plugin_name=plugin_name,
        shared_pictures=status.shared_pictures,
        shared_fps=status.shared_fps,
        shared_source=status.shared_source,
        plugin_decoder=status.decoder,
    )


def binding_text(
    binding: PluginBinding,
    status: BindingStatus | None,
    connected: bool,
    *,
    decoding: bool = True,
) -> tuple[str, str]:
    """(text, tone) for one bound camera. ``decoding``: say who decodes and
    scales its pictures (off where the full lines are shown beside it)."""
    if binding.withdrawn:
        return ("Not covered any more · add it under Subscription in NeuroWatch Cloud", "muted")
    if not binding.enabled:
        return ("Off", "muted")
    if not connected:
        return ("Waiting for the plugin", "muted")
    if status is None or status.state == "attaching":
        return ("Starting…", "info")
    if status.state == "refused":
        return (f"Refused: {status.detail or 'the plugin did not accept this camera'}", "error")
    if status.state == "stream_unavailable":
        return (status.detail or "The camera's video is not reaching the plugin", "warning")
    plugin = status.plugin_state
    if plugin == "running":
        text = f"Running · {status.fps:.0f} pictures a second"
        lines = binding_decoding(status, "") if decoding else None
        if lines is not None and (status.decoder or lines.shared):
            text += f" · {lines.decoded_by}"
            if lines.scaled:
                text += f" · {lines.scaled}"
        if status.dropped:
            text += f" · {status.dropped} frames dropped"
        return (text, "success")
    if plugin == "settling":
        return ("Paused while the camera moves", "info")
    if plugin == "error":
        return (status.plugin_detail or "The plugin reports a problem", "error")
    return ("Waiting for video…", "info")


# ---- Settings form --------------------------------------------------------


#: A choice list with more options than this gets a search box and scrolls.
SCROLL_THRESHOLD = 8


def needs_scroll(option_count: int) -> bool:
    """Whether a list of this many options is shown scrolling with a search box."""
    return option_count > SCROLL_THRESHOLD


def schema_order(node: object) -> list[str]:
    """``x-nw-order`` of a schema object: the keys to show first, in order."""
    raw = node.get("x-nw-order") if isinstance(node, dict) else None
    if not isinstance(raw, list):
        return []
    return [k for k in raw if isinstance(k, str)]


def apply_order(keys: Sequence[str], order: Sequence[str]) -> list[str]:
    """``keys`` with those named in ``order`` first (in that order, once each;
    unknown names ignored), then the rest in their own order."""
    present = set(keys)
    first = [k for k in dict.fromkeys(order) if k in present]
    named = set(first)
    return first + [k for k in keys if k not in named]


def shape_summary(values: Mapping[str, float], defaults: Mapping[str, float]) -> str:
    """One line for a class's size and shape limits: "Any size" while they are
    the defaults, else what is limited ("1–20%, 0.2–1.0": share of the picture,
    then width ÷ height)."""
    parts: list[str] = []
    size = tuple(values.get(k, defaults.get(k, 0)) for k in ("min_size", "max_size"))
    if size != tuple(defaults.get(k, 0) for k in ("min_size", "max_size")):
        parts.append(f"{size[0]:g}–{size[1]:g}%")
    ratio = tuple(values.get(k, defaults.get(k, 0)) for k in ("min_ratio", "max_ratio"))
    if ratio != tuple(defaults.get(k, 0) for k in ("min_ratio", "max_ratio")):
        parts.append(f"{ratio[0]:g}–{ratio[1]:g}")
    return ", ".join(parts) or "Any size"


def chosen_first(options: Sequence[str], chosen: Sequence[str]) -> list[str]:
    """``options`` with the chosen ones first; each group keeps its order."""
    picked = set(chosen)
    return [o for o in options if o in picked] + [o for o in options if o not in picked]


@dataclass
class ExtraField:
    """A field of one area (``nw:zones``) or one class (``nw:classes``) beyond
    the ones NeuroWatch itself draws, as the item schema describes it.

    ``kind``: ``int``, ``float``, ``bool``, ``many`` (a multi-choice list,
    empty meaning the field's own "all") or ``numbers`` (``length`` number
    inputs in a row)."""

    key: str
    kind: str
    title: str
    description: str = ""
    default: Any = None
    minimum: float | None = None
    maximum: float | None = None
    choices: list[str] = field(default_factory=list)
    choice_labels: dict[str, str] = field(default_factory=dict)
    length: int = 0
    #: ``x-nw-zone-kinds``: the area kinds that show it (empty: all of them).
    zone_kinds: tuple[str, ...] = ()

    def applies_to(self, zone_kind: str) -> bool:
        return not self.zone_kinds or zone_kind in self.zone_kinds

    def initial(self) -> Any:
        """What a missing value shows and saves."""
        if self.default is not None:
            return self.default
        if self.kind == "bool":
            return False
        if self.kind == "many":
            return []
        if self.kind == "numbers":
            return [self.minimum or 0] * self.length
        return self.minimum if self.minimum is not None else 0

    def value_of(self, value: object) -> Any:
        """``value`` when it fits the field, else the default."""
        number = isinstance(value, (int, float)) and not isinstance(value, bool)
        if self.kind in ("int", "float"):
            return value if number else self.initial()
        if self.kind == "bool":
            return value if isinstance(value, bool) else self.initial()
        if self.kind == "many":
            if isinstance(value, list):
                return [v for v in value if isinstance(v, str) and v in self.choices]
            return list(self.initial())
        if isinstance(value, list) and len(value) == self.length:
            if all(isinstance(v, (int, float)) and not isinstance(v, bool) for v in value):
                return list(value)
        return list(self.initial())


_FIXED_KEYS_ZONE = frozenset({"id", "name", "kind", "points"})
_FIXED_KEYS_CLASS = frozenset({"id", "confidence"})


def extra_fields(item_schema: object, fixed: frozenset[str]) -> list[ExtraField]:
    """The fields of an array's items (``items.properties``) other than
    ``fixed``, of the types the form can show; others are left out and kept
    as they are."""
    props = _dict_at(item_schema, "properties") if isinstance(item_schema, dict) else {}
    result: list[ExtraField] = []
    for key in apply_order(list(props), schema_order(item_schema)):
        prop = props[key]
        if key in fixed or not isinstance(prop, dict):
            continue
        kind = prop.get("type")
        minimum, maximum = prop.get("minimum"), prop.get("maximum")
        low = float(minimum) if isinstance(minimum, (int, float)) else None
        high = float(maximum) if isinstance(maximum, (int, float)) else None
        entry = ExtraField(
            key=str(key),
            kind="",
            title=str(prop.get("title") or str(key).replace("_", " ").capitalize()),
            description=str(prop.get("description") or ""),
            default=prop.get("default"),
            minimum=low,
            maximum=high,
        )
        sub = prop.get("items")
        if kind == "integer":
            entry.kind = "int"
        elif kind == "number":
            entry.kind = "float"
        elif kind == "boolean":
            entry.kind = "bool"
        elif kind == "array" and isinstance(sub, dict) and isinstance(sub.get("enum"), list):
            entry.kind = "many"
            entry.choices = [str(c) for c in sub["enum"]]
            labels = sub.get("x-nw-labels")
            if isinstance(labels, dict):
                entry.choice_labels = {
                    str(k): str(v) for k, v in labels.items() if isinstance(v, str)
                }
        elif (
            kind == "array"
            and isinstance(sub, dict)
            and sub.get("type") in ("number", "integer")
            and isinstance(prop.get("minItems"), int)
            and prop.get("minItems") == prop.get("maxItems")
            and 0 < prop["minItems"] <= 16
        ):
            entry.kind = "numbers"
            entry.length = prop["minItems"]
            low, high = sub.get("minimum"), sub.get("maximum")
            entry.minimum = float(low) if isinstance(low, (int, float)) else entry.minimum
            entry.maximum = float(high) if isinstance(high, (int, float)) else entry.maximum
            if not isinstance(prop.get("default"), list):
                entry.default = None
        else:
            continue  # an unknown type: kept untouched in the saved item
        if entry.kind in ("int", "float", "bool") and entry.default is not None:
            ok = isinstance(entry.default, bool) == (entry.kind == "bool") and isinstance(
                entry.default, (int, float)
            )
            if not ok:
                entry.default = None
        zone_kinds = prop.get("x-nw-zone-kinds")
        if isinstance(zone_kinds, list):
            entry.zone_kinds = tuple(str(k) for k in zone_kinds)
        result.append(entry)
    return result


@dataclass
class FormField:
    """One setting of a plugin's configuration, as its JSON Schema describes it."""

    key: str
    kind: str  # "int", "float", "bool", "choice", "text", "zones", "lanes", "classes"
    title: str
    default: Any = None
    minimum: float | None = None
    maximum: float | None = None
    choices: list[str] = field(default_factory=list)
    #: What the form shows for a choice (``x-nw-labels``), when not the value.
    choice_labels: dict[str, str] = field(default_factory=dict)
    description: str = ""
    #: ``lanes``: the field holding their crossing lines (``x-nw-lines``).
    lines_key: str = ""
    #: The most entries a drawn field may have, and points in an area.
    max_items: int = 0
    max_points: int = 0
    #: ``classes``: the per-class confidence (None when the plugin has none),
    #: its bounds, its default, and the defaults of single classes.
    has_confidence: bool = False
    #: ``classes``: what the per-class confidence is called and means.
    confidence_title: str = ""
    confidence_description: str = ""
    confidence_min: int = 0
    confidence_max: int = 100
    confidence_default: int = 50
    class_defaults: dict[str, int] = field(default_factory=dict)
    #: ``zones``: each area's extra fields; ``classes``: each class's.
    area_fields: list[ExtraField] = field(default_factory=list)
    class_fields: list[ExtraField] = field(default_factory=list)


def _dict_at(value: dict[str, Any], *keys: str) -> dict[str, Any]:
    """The object at that path, or an empty one where the schema has none."""
    for key in keys:
        inner = value.get(key)
        value = inner if isinstance(inner, dict) else {}
    return value


def form_fields(schema: object) -> list[FormField]:
    """The settings NeuroWatch can show for a schema. Properties of other
    types are left out of the form and kept as they are."""
    props = schema.get("properties") if isinstance(schema, dict) else None
    fields: list[FormField] = []
    if not isinstance(props, dict):
        return fields  # a plugin's schema is not trusted to be well formed
    for key in apply_order(list(props), schema_order(schema)):
        prop = props[key]
        if not isinstance(prop, dict):
            continue
        kind = prop.get("type")
        if prop.get("x-nw-widget") == "nw:zones":
            shown = "zones"
        elif prop.get("x-nw-widget") == "nw:lanes":
            shown = "lanes"
        elif prop.get("x-nw-widget") == "nw:lines":
            shown = "lines"
        elif prop.get("x-nw-widget") == "nw:areas":
            shown = "areas"
        elif prop.get("x-nw-widget") == "nw:classes" and isinstance(
            _dict_at(prop, "items", "properties", "id").get("enum"), list
        ):
            shown = "classes"
        elif isinstance(prop.get("items"), dict) and isinstance(prop["items"].get("enum"), list):
            shown = "many"
        elif isinstance(prop.get("enum"), list):
            shown = "choice"
        elif kind == "integer":
            shown = "int"
        elif kind == "number":
            shown = "float"
        elif kind == "boolean":
            shown = "bool"
        elif kind == "string":
            shown = "text"
        else:
            continue
        minimum, maximum = prop.get("minimum"), prop.get("maximum")
        labels = prop.get("x-nw-labels")
        points = _dict_at(prop, "items", "properties", "points")
        options = _dict_at(prop, "items").get("enum") if shown == "many" else prop.get("enum")
        if shown == "many" and not isinstance(labels, dict):
            # A list's labels may sit with its options.
            labels = _dict_at(prop, "items").get("x-nw-labels")
        class_id = _dict_at(prop, "items", "properties", "id")
        confidence = _dict_at(prop, "items", "properties", "confidence")
        if shown == "classes":
            options = class_id.get("enum")
            labels = class_id.get("x-nw-labels")
        low, high, base = (_whole(confidence.get(n)) for n in ("minimum", "maximum", "default"))
        low = 0 if low is None else low
        high = 100 if high is None else max(high, low)
        base = low if base is None else min(max(base, low), high)
        per_class = confidence.get("x-nw-defaults")
        class_defaults = (
            {
                str(k): min(max(v, low), high)
                for k, raw in per_class.items()
                if (v := _whole(raw)) is not None
            }
            if isinstance(per_class, dict)
            else {}
        )
        fields.append(
            FormField(
                key=str(key),
                kind=shown,
                title=str(prop.get("title") or str(key).replace("_", " ").capitalize()),
                default=prop.get("default"),
                minimum=float(minimum) if isinstance(minimum, (int, float)) else None,
                maximum=float(maximum) if isinstance(maximum, (int, float)) else None,
                choices=[str(c) for c in options or []]
                if shown in ("choice", "many", "classes")
                else [],
                choice_labels={str(k): str(v) for k, v in labels.items() if isinstance(v, str)}
                if shown in ("choice", "many", "classes") and isinstance(labels, dict)
                else {},
                description=str(prop.get("description") or ""),
                lines_key=str(prop.get("x-nw-lines") or "lines") if shown == "lanes" else "",
                max_items=int(prop["maxItems"]) if isinstance(prop.get("maxItems"), int) else 0,
                max_points=int(points["maxItems"])
                if isinstance(points.get("maxItems"), int)
                else 0,
                has_confidence=shown == "classes" and bool(confidence),
                confidence_title=str(confidence.get("title") or "Confidence (%)")
                if shown == "classes"
                else "",
                confidence_description=str(confidence.get("description") or "")
                if shown == "classes"
                else "",
                confidence_min=low,
                confidence_max=high,
                confidence_default=base,
                class_defaults=class_defaults if shown == "classes" else {},
                area_fields=extra_fields(prop.get("items"), _FIXED_KEYS_ZONE)
                if shown == "zones"
                else [],
                class_fields=[
                    f
                    for f in extra_fields(prop.get("items"), _FIXED_KEYS_CLASS)
                    if f.kind in ("int", "float")
                ]
                if shown == "classes"
                else [],
            )
        )
    return fields


def _whole(value: object) -> int | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return int(value)
    return None


def camera_plugins_text(result: object, camera_id: str) -> str:
    """Which connected plugins analyse ``camera_id`` (``plugins.status``), for
    the camera's Motion tab."""
    names = []
    for status in parse_status(result).values():
        view = status.bindings.get(camera_id)
        if view is None:
            continue
        name = status.manifest.name if status.manifest and status.manifest.name else status.id
        binding = PluginBinding(status.id, camera_id)
        text, _tone = binding_text(binding, view, status.state == "connected")
        names.append(f"{name} ({text[0].lower()}{text[1:]})" if text else name)
    if not names:
        return "No plugin analyses this camera."
    return "Analysed by " + "; ".join(names) + "."


# ---- Plugins that stopped answering ----------------------------------------


def offline_plugin_names(system: object) -> list[str]:
    """``system.status`` ``plugins_offline``: the names of enabled plugins the
    recorder could not reach for a while (``plugin_host::OFFLINE_AFTER``)."""
    raw = system.get("plugins_offline") if isinstance(system, dict) else None
    names: list[str] = []
    for plugin in raw if isinstance(raw, list) else []:
        if isinstance(plugin, dict) and (plugin.get("name") or plugin.get("id")):
            names.append(str(plugin.get("name") or plugin.get("id")))
    return names


def offline_notice(names: Sequence[str]) -> tuple[str, str]:
    """(status bar text, its tooltip) for plugins offline; empty when none."""
    if not names:
        return ("", "")
    text = f"Plugin offline: {names[0]}" if len(names) == 1 else f"{len(names)} plugins offline"
    tooltip = (
        "Not answering: "
        + ", ".join(names)
        + ". The recorder keeps trying; everything else keeps working."
    )
    return (text, tooltip)
