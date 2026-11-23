"""One camera stream as a tree of components, for Diagnostics (no Qt).

Stream → frame buffer (the shared GOP cache) → consumers. Wherever a picture
is decoded there is a decoder box: the video wall's (whose pictures a plugin
may share, drawn dashed) and each plugin's own. Missing telemetry stays
unknown; nothing here measures or opens video.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from neurowatch.models.plugin_decoding import parse_plugin_decoder, parse_shared_source
from neurowatch.models.transcode import WHY, parse_transcode

CODECS = {"h264": "H.264", "h265": "H.265"}
STATES = {
    "streaming": "Connected",
    # No connection because nothing needs one (a lost camera shows Retrying).
    "disconnected": "Not in use",
    "idle": "Waiting to disconnect",
    "connecting": "Connecting",
    "retrying": "Retrying",
    "unavailable": "Not available",
}
#: Plain consumers: compressed video, nothing decoded for them on the recorder.
_PLAIN = {
    "recording": "Recording",
    "browser": "Web",
    "snapshot": "Snapshots",
    "audio": "Audio",
}


@dataclass
class FlowNode:
    """A box. ``kind``: stream, buffer, consumer, decoder, plugin or note."""

    kind: str
    title: str
    detail: str = ""
    chip: str | None = None
    #: The connector into this box: dashed for shared decoded pictures.
    dashed: bool = False
    description: str = ""
    children: list[FlowNode] = field(default_factory=list)


@dataclass(frozen=True)
class FlowRow:
    """A box in drawing order. ``guides[i]``: an ancestor at depth ``i+1``
    has siblings below, so its vertical line continues through this row."""

    node: FlowNode
    depth: int
    last: bool
    guides: tuple[bool, ...]


def codec_text(codec: object) -> str | None:
    """``h264`` → ``H.264``; an unknown codec as it is, upper-cased."""
    if not isinstance(codec, str) or not codec:
        return None
    return CODECS.get(codec, codec.upper())


def _times(count: int) -> str:
    return f" ×{count}" if count > 1 else ""


def _number(value: object, digits: int = 1) -> str:
    return f"{value:.{digits}f}" if isinstance(value, (int, float)) else "—"


def _bytes(value: object) -> str:
    if not isinstance(value, (int, float)):
        return "—"
    amount = float(value)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if amount < 1024 or unit == "GiB":
            return f"{amount:.1f} {unit}" if unit != "B" else f"{int(amount)} B"
        amount /= 1024
    return "—"


def _plugin_decoder(consumer: dict[str, Any]) -> str:
    """The decoder the plugin names, without its scaling words; nothing for
    the words it uses while it decodes nothing itself."""
    text = parse_plugin_decoder(consumer.get("decoder"))
    return "" if text.shares else text.decoder


def _plugin_scaling(consumer: dict[str, Any]) -> str:
    """``GPU``/``CPU``: where the plugin scales its pictures down; empty if not said."""
    return parse_plugin_decoder(consumer.get("decoder")).scaling


def _shared_hardware(plugins: list[dict[str, Any]], by: str) -> str:
    """What the recorder's ``by`` decoder runs on, as a sharing plugin's row says."""
    for plugin in plugins:
        source = parse_shared_source(plugin.get("shared_source"))
        if source is not None and source.by == by and source.decoder:
            return source.decoder
    return ""


def stream_flow(
    row: dict[str, Any],
    plugins: list[dict[str, Any]],
    *,
    bitrate: str,
    desktop_decoder: str | None = None,
    desktop_fps: object = None,
    shared_elsewhere: str | None = None,
) -> FlowNode:
    """The tree of one stream.

    ``plugins``: the named plugins on this stream (``plugin_stream_consumers``
    rows, at most its measured plugin count). ``desktop_decoder``: what decodes
    the video wall (``video_acceleration.label``). ``shared_elsewhere``: the
    stream whose video wall decoder a shared plugin uses when this stream has
    none on screen (the recorder may lend another stream's pictures).
    """
    state = str(row.get("state", "unavailable"))
    available = state != "unavailable"
    stream = str(row.get("stream", ""))
    title = {"main": "Main", "sub": "Sub", "main+sub": "Main / Sub (shared)"}.get(
        stream, stream.replace("other_", "Other ")
    )
    codec = codec_text(row.get("codec"))
    root = FlowNode(
        "stream",
        f"{title} · {STATES.get(state, state)}",
        f"In {bitrate} · {_number(row.get('fps'))} fps",
        chip=codec,
        description=f"{title} stream" + (f", {codec}" if codec else ""),
    )
    if not available:
        root.children.append(FlowNode("note", "Consumers not reported"))
        return root
    if state == "disconnected":
        # No connection, so no buffer: nothing reaches anyone.
        root.children.append(FlowNode("note", "Opens when something needs it"))
        return root
    units = row.get("buffer_units")
    drops = row.get("queue_drops", 0)
    buffer = FlowNode(
        "buffer",
        "Frame buffer",
        f"{_bytes(row.get('buffer_bytes'))} · "
        f"{units if isinstance(units, int) else '—'} frames · "
        f"{drops if isinstance(drops, int) else '—'} drops",
        description=(
            "Compressed video since the last keyframe, shared by every consumer "
            "(a new viewer starts from it). Drops: units a slow consumer missed."
        ),
    )
    root.children.append(buffer)
    roles = row.get("roles") or {}
    named = plugins[: max(0, int(roles.get("plugin", 0) or 0))]
    shared = [p for p in named if p.get("pictures") == "shared"]
    own = [p for p in named if p.get("pictures") != "shared"]
    branches: list[FlowNode] = []

    def plain(role: str) -> None:
        count = int(roles.get(role, 0) or 0)
        if count:
            branches.append(FlowNode("consumer", _PLAIN[role] + _times(count)))

    plain("recording")
    if row.get("cloud_backup") is True:
        branches.append(
            FlowNode(
                "consumer",
                "Cloud backup",
                description="Copied to NeuroWatch Cloud Backup as it arrives "
                "(compressed; nothing is decoded for it).",
            )
        )
    desktop = int(roles.get("desktop", 0) or 0)
    always = row.get("always_decoded") is True
    if always:
        # Kept decoding for plugins (the camera's setting); its pictures win.
        box = FlowNode(
            "decoder",
            _decoder_title(codec, "always on"),
            desktop_decoder or _shared_hardware(shared, "always_on"),
            description=f"The recorder decodes this stream's {codec or 'video'} all the "
            "time (the camera's setting); plugins below it share these pictures.",
        )
        # Walls showing this stream use these pictures too (no decoder of their own).
        walls = int(row.get("shared_views", 0) or 0)
        box.children = [
            *([FlowNode("consumer", "Video wall" + _times(walls))] if walls else []),
            *(_shared_plugin(plugin) for plugin in shared),
        ] or [FlowNode("note", "Nothing uses these pictures yet")]
        branches.append(box)
    if desktop:
        fps = f"{_number(desktop_fps)} fps" if isinstance(desktop_fps, (int, float)) else ""
        wall = FlowNode(
            "decoder",
            _decoder_title(codec, "video wall"),
            " · ".join(
                part
                for part in (desktop_decoder or _shared_hardware(shared, "video_wall"), fps)
                if part
            ),
            description=f"The recorder decodes this stream's {codec or 'video'} for the "
            "desktop video wall" + ("." if always else "; plugins below it share these pictures."),
        )
        wall.children.append(FlowNode("consumer", "Video wall" + _times(desktop)))
        if not always:
            for plugin in shared:
                wall.children.append(_shared_plugin(plugin))
        branches.append(wall)
    transcode = parse_transcode(row.get("transcode"))
    web = int(roles.get("browser", 0) or 0)
    if transcode is None:
        plain("browser")
    else:
        # Viewers the recorder converts hang under the conversion; the rest
        # (browsers that play the stream as it is) stay directly under the buffer.
        converted = min(web, transcode.viewers) if web else transcode.viewers
        if web - converted > 0:
            branches.append(FlowNode("consumer", _PLAIN["browser"] + _times(web - converted)))
        box = FlowNode(
            "transcode",
            f"Transcoding {transcode.conversion}",
            transcode.detail,
            description=WHY,
        )
        box.children.append(FlowNode("consumer", _PLAIN["browser"] + _times(converted)))
        branches.append(box)
    for plugin in own:
        branches.append(_decoding_plugin(plugin, codec))
    if shared and not desktop and not always:
        for plugin in shared:
            node = _shared_plugin(plugin)
            source = parse_shared_source(plugin.get("shared_source"))
            words = (
                "Uses the always-on decoder's pictures"
                if source is not None and source.by == "always_on"
                else "Uses the video wall's decoded pictures"
            )
            node.detail = (
                words
                + (f" ({shared_elsewhere})" if shared_elsewhere else "")
                + (f" · {source.decoder}" if source is not None and source.decoder else "")
                + _shared_rate(plugin)
                + _scales(plugin)
            )
            branches.append(node)
    plain("snapshot")
    plain("audio")
    unnamed = int(roles.get("plugin", 0) or 0) - len(named)
    if unnamed > 0:
        branches.append(
            FlowNode("plugin", "Plugins" + _times(unnamed) if unnamed > 1 else "Plugin")
        )
    buffer.children = branches or [FlowNode("note", "No consumers")]
    return root


def _shared_rate(plugin: dict[str, Any]) -> str:
    fps = plugin.get("shared_fps")
    return f" · {fps:.1f} fps" if isinstance(fps, (int, float)) else ""


def _scales(plugin: dict[str, Any]) -> str:
    scaling = _plugin_scaling(plugin)
    return f" · scales down on the {scaling}" if scaling else ""


def _shared_plugin(plugin: dict[str, Any]) -> FlowNode:
    name = str(plugin.get("name", "")).strip() or "Plugin"
    return FlowNode(
        "plugin",
        name,
        "Uses these decoded pictures" + _shared_rate(plugin) + _scales(plugin),
        dashed=True,
        description=f"{name} uses the pictures the video wall already decodes: no second decoder.",
    )


def _decoding_plugin(plugin: dict[str, Any], stream_codec: str | None) -> FlowNode:
    name = str(plugin.get("name", "")).strip() or "Plugin"
    codec = codec_text(plugin.get("codec")) or stream_codec
    decoder = _plugin_decoder(plugin)
    scaling = _plugin_scaling(plugin)
    if plugin.get("pictures") is None:
        # An older recorder: it does not say how the plugin gets its video.
        return FlowNode("plugin", name, description=f"{name} receives this stream.")
    box = FlowNode(
        "decoder",
        _decoder_title(codec, f"in {name}"),
        decoder,
        description=f"{name} receives the compressed {codec or 'video'} and decodes it "
        "itself"
        + (f" ({decoder})" if decoder else "")
        + (f", then scales the pictures down on the {scaling}." if scaling else "."),
    )
    box.children.append(
        FlowNode("plugin", name, f"Scales down on the {scaling}" if scaling else "")
    )
    return box


def _decoder_title(codec: str | None, where: str) -> str:
    """``H.265 decoder · in Motion``: the codec once, in words, not a second chip."""
    return f"{codec} decoder · {where}" if codec else f"Decoder · {where}"


def flatten(root: FlowNode) -> list[FlowRow]:
    """Boxes in drawing order, with the guides the connectors need."""
    rows: list[FlowRow] = []

    def visit(node: FlowNode, depth: int, last: bool, guides: tuple[bool, ...]) -> None:
        rows.append(FlowRow(node, depth, last, guides))
        child_guides = (*guides, not last) if depth > 0 else guides
        for index, child in enumerate(node.children):
            visit(child, depth + 1, index == len(node.children) - 1, child_guides)

    visit(root, 0, True, ())
    return rows
