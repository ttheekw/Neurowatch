"""Analytics plugins (``plugins.get|set|status``): the daemons the recorder
connects to and the cameras bound to each.

No Qt: Settings → Plugins and its dialogs use it, and so do tests.
"""

from __future__ import annotations

import os
import re
from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

#: The motion plugin's id in its manifest (``nw-motion``).
MOTION_PLUGIN_ID = "nw.motion"

#: Plugin ids the recorder accepts (``plugin_host::config``).
_ID = re.compile(r"^[A-Za-z0-9._-]{1,64}$")

#: Motion settings a camera's built-in detector had, carried into a binding.
_MOTION_FIELDS = ("sensitivity", "cooldown_sec", "min_duration_sec", "low_cpu", "zones")


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

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {"id": self.id, "address": self.address, "enabled": self.enabled}
        if self.approval is not None:
            out["approval"] = self.approval
        if self.credential_revision:
            out["credential_revision"] = self.credential_revision
        if self.plugin_id:
            out["plugin_id"] = self.plugin_id
        return out


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
        if status.state == "connected":
            if old is not None and old.tone == "success":
                notices.append(old)
            else:
                verb = "reconnected" if old else "connected"
                notices.append(PluginConnectionNotice(server.id, f"{server.id} {verb}.", "success"))
        elif status.state in ("connecting", "authorizing"):
            if old is not None:
                notices.append(
                    old
                    if old.tone == "error"
                    else PluginConnectionNotice(
                        server.id, f"{server.id} disconnected. The recorder keeps trying.", "error"
                    )
                )
        else:
            verb = "disconnected" if old else "unavailable"
            if old is not None and old.text.startswith(f"{server.id} unavailable"):
                verb = "unavailable"
            detail = f": {status.detail}" if status.detail else ""
            notices.append(
                PluginConnectionNotice(
                    server.id, f"{server.id} {verb}{detail}. The recorder keeps trying.", "error"
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


def binding_text(
    binding: PluginBinding, status: BindingStatus | None, connected: bool
) -> tuple[str, str]:
    """(text, tone) for one bound camera."""
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
        if status.decoder:
            text += f" · {status.decoder}"
        if status.dropped:
            text += f" · {status.dropped} frames dropped"
        return (text, "success")
    if plugin == "settling":
        return ("Paused while the camera moves", "info")
    if plugin == "error":
        return (status.plugin_detail or "The plugin reports a problem", "error")
    return ("Waiting for video…", "info")


# ---- Settings form --------------------------------------------------------


@dataclass
class FormField:
    """One setting of a plugin's configuration, as its JSON Schema describes it."""

    key: str
    kind: str  # "int", "float", "bool", "choice", "text", "zones", "lanes"
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
    for key, prop in props.items():
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
        fields.append(
            FormField(
                key=str(key),
                kind=shown,
                title=str(prop.get("title") or str(key).replace("_", " ").capitalize()),
                default=prop.get("default"),
                minimum=float(minimum) if isinstance(minimum, (int, float)) else None,
                maximum=float(maximum) if isinstance(maximum, (int, float)) else None,
                choices=[str(c) for c in options or []] if shown in ("choice", "many") else [],
                choice_labels={str(k): str(v) for k, v in labels.items() if isinstance(v, str)}
                if shown in ("choice", "many") and isinstance(labels, dict)
                else {},
                description=str(prop.get("description") or ""),
                lines_key=str(prop.get("x-nw-lines") or "lines") if shown == "lanes" else "",
                max_items=int(prop["maxItems"]) if isinstance(prop.get("maxItems"), int) else 0,
                max_points=int(points["maxItems"])
                if isinstance(points.get("maxItems"), int)
                else 0,
            )
        )
    return fields


def motion_config_from_camera(motion: object) -> dict[str, Any]:
    """A motion plugin binding's settings, from the camera's built-in
    detector settings (same names), so a move keeps them."""
    if not isinstance(motion, dict):
        return {}
    return {key: motion[key] for key in _MOTION_FIELDS if key in motion}


def uses_builtin_motion(motion: object) -> bool:
    """The camera's built-in detector is on (``source`` builtin/both, or the
    legacy ``enabled`` switch)."""
    if not isinstance(motion, dict):
        return False
    source = motion.get("source")
    if source in ("builtin", "both"):
        return True
    return source in (None, "off") and bool(motion.get("enabled"))


def without_builtin_motion(motion: dict[str, Any]) -> dict[str, Any]:
    """``motion`` with the built-in detector off; the camera's own (ONVIF)
    detection stays as it was."""
    updated = dict(motion)
    source = updated.get("source")
    updated["source"] = "onvif" if source in ("onvif", "both") else "off"
    updated["enabled"] = False
    return updated


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
