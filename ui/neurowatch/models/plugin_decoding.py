"""Who decodes a plugin's pictures and who scales them down, in words (no Qt).

Two sources say it. The recorder reports whether it shares decoded pictures
with a binding (``shared_pictures``, ``shared_fps``) and, since 2026-11-22,
which of its decoders made them and on what (``shared_source {by, decoder}``).
The plugin reports its own ``decoder`` text, which may end
``" · scaled down on the GPU"`` (or ``CPU``): who scales the pictures down to
the analysis size. Older recorders and plugins send less; what is not said is
not shown.
"""

from __future__ import annotations

from dataclasses import dataclass

#: The plugin's decoder text ends ``<decoder> · scaled down on the GPU``.
SCALING_MARK = " · scaled down on the "
#: Decoder words a plugin reports while it decodes nothing itself.
SHARED_WORDS = ("neurowatch's pictures", "video wall pictures", "desktop (shared)")
#: The recorder's decoders that share pictures (``shared_source.by``).
SOURCE_NAMES = {
    "always_on": "NeuroWatch's always-on decoder",
    "video_wall": "NeuroWatch's video wall decoder",
}


@dataclass(frozen=True)
class PluginDecoderText:
    """The plugin's ``decoder`` text, split. ``scaling``: ``GPU``/``CPU``, or
    empty when the plugin does not say."""

    decoder: str
    scaling: str

    @property
    def shares(self) -> bool:
        """The plugin says it uses NeuroWatch's pictures (decodes nothing)."""
        return self.decoder.lower().startswith(SHARED_WORDS)


@dataclass(frozen=True)
class SharedSource:
    """Which of the recorder's decoders made the shared pictures, and on what
    (``VA-API``, ``NVDEC``, ``CPU``…; empty when not said)."""

    by: str
    decoder: str


@dataclass(frozen=True)
class DecodingLines:
    """What a binding's status shows about decoding.

    ``decoding`` and ``scaling`` are the full lines (``scaling`` empty when
    unknown); ``decoded_by`` and ``scaled`` the compact parts of a one-line
    status."""

    shared: bool
    decoding: str
    scaling: str
    decoded_by: str
    scaled: str


def parse_plugin_decoder(text: object) -> PluginDecoderText:
    """``"VA-API (Intel, AMD) · scaled down on the GPU"`` → decoder and
    ``GPU``. Text without the scaling words is the decoder, whole."""
    words = text.strip() if isinstance(text, str) else ""
    head, mark, tail = words.rpartition(SCALING_MARK)
    if mark and tail.strip():
        return PluginDecoderText(head.strip(), tail.strip())
    lead = SCALING_MARK.removeprefix(" · ")
    if words.startswith(lead) and words[len(lead) :].strip():
        return PluginDecoderText("", words[len(lead) :].strip())
    return PluginDecoderText(words, "")


def parse_shared_source(raw: object) -> SharedSource | None:
    """``{"by": "always_on", "decoder": "VA-API"}``; None unless it says who."""
    if not isinstance(raw, dict):
        return None
    by = raw.get("by")
    if not isinstance(by, str) or not by.strip():
        return None
    decoder = raw.get("decoder")
    return SharedSource(by.strip(), decoder.strip() if isinstance(decoder, str) else "")


def decoding_lines(
    *,
    plugin_name: str,
    shared_pictures: bool,
    shared_fps: float | None,
    shared_source: SharedSource | None,
    plugin_decoder: str,
) -> DecodingLines:
    """Who decodes the binding's pictures, and who scales them down."""
    where = f"In {plugin_name.strip()}" if plugin_name.strip() else "In the plugin"
    text = parse_plugin_decoder(plugin_decoder)
    scaling = f"{where} · {text.scaling}" if text.scaling else ""
    scaled = f"scaled on the {text.scaling}" if text.scaling else ""
    if shared_pictures or text.shares:
        source = shared_source if shared_pictures else None
        hardware = source.decoder if source is not None else ""
        if source is None:
            who = "NeuroWatch's decoded pictures"
        else:
            who = SOURCE_NAMES.get(source.by, "NeuroWatch's decoder")
        rate = (
            f"shared at {shared_fps:.1f} fps" if shared_pictures and shared_fps is not None else ""
        )
        return DecodingLines(
            shared=True,
            decoding=" · ".join(part for part in (who, hardware, rate) if part),
            scaling=scaling,
            decoded_by="decoded by NeuroWatch" + (f" ({hardware})" if hardware else ""),
            scaled=scaled,
        )
    return DecodingLines(
        shared=False,
        decoding=f"{where} · {text.decoder}" if text.decoder else where,
        scaling=scaling,
        decoded_by="decoded in the plugin" + (f" with {text.decoder}" if text.decoder else ""),
        scaled=scaled,
    )
