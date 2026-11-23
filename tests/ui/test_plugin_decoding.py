"""Who decodes a plugin's pictures, and who scales them down, in words."""

from __future__ import annotations

import pytest

from neurowatch.models.plugin_decoding import (
    PluginDecoderText,
    SharedSource,
    decoding_lines,
    parse_plugin_decoder,
    parse_shared_source,
)
from neurowatch.models.plugins import (
    BindingStatus,
    PluginBinding,
    binding_decoding,
    binding_text,
    parse_status,
)

OLD_SHARED = "NeuroWatch's pictures (no decoding here)"
OLDER_SHARED = "Video wall pictures (no decoding here)"


@pytest.mark.parametrize(
    ("text", "decoder", "scaling", "shares"),
    [
        ("VA-API (Intel, AMD) · scaled down on the GPU", "VA-API (Intel, AMD)", "GPU", False),
        ("CPU (software) · scaled down on the CPU", "CPU (software)", "CPU", False),
        (
            "NVIDIA NVDEC: VA-API is not available here · scaled down on the CPU",
            "NVIDIA NVDEC: VA-API is not available here",
            "CPU",
            False,
        ),
        (
            "NeuroWatch's pictures (no decoding here) · scaled down on the CPU",
            "NeuroWatch's pictures (no decoding here)",
            "CPU",
            True,
        ),
        # Older plugins: no scaling words, so the scaling is unknown.
        ("VA-API (Intel, AMD)", "VA-API (Intel, AMD)", "", False),
        (OLD_SHARED, OLD_SHARED, "", True),
        (OLDER_SHARED, OLDER_SHARED, "", True),
        # Unknown words are the decoder, whole, even with a separator in them.
        ("VA-API (Intel, AMD) · yolo", "VA-API (Intel, AMD) · yolo", "", False),
        ("  avdec_h264  ", "avdec_h264", "", False),
        ("", "", "", False),
    ],
)
def test_plugin_decoder_text_splits_into_decoder_and_scaling(
    text: str, decoder: str, scaling: str, shares: bool
) -> None:
    parsed = parse_plugin_decoder(text)
    assert parsed == PluginDecoderText(decoder, scaling)
    assert parsed.shares is shares


def test_plugin_decoder_text_that_is_not_text_is_empty() -> None:
    assert parse_plugin_decoder(None) == PluginDecoderText("", "")
    assert parse_plugin_decoder(7) == PluginDecoderText("", "")


def test_shared_source_is_read_only_when_it_says_who() -> None:
    assert parse_shared_source({"by": "always_on", "decoder": "VA-API"}) == SharedSource(
        "always_on", "VA-API"
    )
    assert parse_shared_source({"by": "video_wall"}) == SharedSource("video_wall", "")
    assert parse_shared_source({"decoder": "CPU"}) is None
    assert parse_shared_source("always_on") is None
    assert parse_shared_source(None) is None


def test_always_on_decoder_shared_with_the_plugin() -> None:
    lines = decoding_lines(
        plugin_name="Motion",
        shared_pictures=True,
        shared_fps=5.0,
        shared_source=SharedSource("always_on", "VA-API"),
        plugin_decoder="NeuroWatch's pictures (no decoding here) · scaled down on the CPU",
    )
    assert lines.shared
    assert lines.decoding == "NeuroWatch's always-on decoder · VA-API · shared at 5.0 fps"
    assert lines.scaling == "In Motion · CPU"
    assert lines.decoded_by == "decoded by NeuroWatch (VA-API)"
    assert lines.scaled == "scaled on the CPU"


def test_video_wall_decoder_shared_with_the_plugin() -> None:
    lines = decoding_lines(
        plugin_name="Motion",
        shared_pictures=True,
        shared_fps=9.5,
        shared_source=SharedSource("video_wall", "CPU"),
        plugin_decoder="NeuroWatch's pictures (no decoding here)",
    )
    assert lines.decoding == "NeuroWatch's video wall decoder · CPU · shared at 9.5 fps"
    assert lines.scaling == "", "an older plugin: the scaling is not known"
    assert lines.decoded_by == "decoded by NeuroWatch (CPU)"
    assert lines.scaled == ""


def test_shared_pictures_from_an_older_recorder_name_no_decoder() -> None:
    lines = decoding_lines(
        plugin_name="Motion",
        shared_pictures=True,
        shared_fps=None,
        shared_source=None,
        plugin_decoder="",
    )
    assert lines.decoding == "NeuroWatch's decoded pictures"
    assert lines.decoded_by == "decoded by NeuroWatch"


def test_a_recorder_without_the_shared_flag_trusts_the_plugin_words() -> None:
    lines = decoding_lines(
        plugin_name="Motion",
        shared_pictures=False,
        shared_fps=None,
        shared_source=None,
        plugin_decoder="Video wall pictures (no decoding here)",
    )
    assert lines.shared
    assert lines.decoding == "NeuroWatch's decoded pictures"


def test_the_plugin_decodes_itself() -> None:
    lines = decoding_lines(
        plugin_name="Motion",
        shared_pictures=False,
        shared_fps=None,
        shared_source=None,
        plugin_decoder="VA-API (Intel, AMD) · scaled down on the GPU",
    )
    assert not lines.shared
    assert lines.decoding == "In Motion · VA-API (Intel, AMD)"
    assert lines.scaling == "In Motion · GPU"
    assert lines.decoded_by == "decoded in the plugin with VA-API (Intel, AMD)"
    assert lines.scaled == "scaled on the GPU"


def test_the_plugin_falls_back_to_the_cpu() -> None:
    lines = decoding_lines(
        plugin_name="Motion",
        shared_pictures=False,
        shared_fps=None,
        shared_source=None,
        plugin_decoder="CPU (software): NVDEC is not available here · scaled down on the CPU",
    )
    assert lines.decoding == "In Motion · CPU (software): NVDEC is not available here"
    assert lines.scaling == "In Motion · CPU"


def test_unknown_decoder_and_unnamed_plugin() -> None:
    lines = decoding_lines(
        plugin_name="",
        shared_pictures=False,
        shared_fps=None,
        shared_source=None,
        plugin_decoder="",
    )
    assert lines.decoding == "In the plugin"
    assert lines.decoded_by == "decoded in the plugin"
    assert lines.scaling == ""


def test_an_unknown_source_is_still_neurowatch() -> None:
    lines = decoding_lines(
        plugin_name="Motion",
        shared_pictures=True,
        shared_fps=5.0,
        shared_source=SharedSource("something_new", ""),
        plugin_decoder="",
    )
    assert lines.decoding == "NeuroWatch's decoder · shared at 5.0 fps"


def _status(view: dict[str, object]) -> BindingStatus:
    parsed = parse_status(
        {"plugins": [{"id": "motion", "state": "connected", "bindings": {"c1": view}}]}
    )
    return parsed["motion"].bindings["c1"]


def test_status_reads_the_shared_source_beside_the_shared_flag() -> None:
    status = _status(
        {
            "state": "active",
            "shared_pictures": True,
            "shared_fps": 5,
            "shared_source": {"by": "always_on", "decoder": "VA-API"},
            "plugin": {
                "state": "running",
                "fps": 5.0,
                "decoder": "NeuroWatch's pictures (no decoding here) · scaled down on the CPU",
            },
        }
    )
    assert status.shared_pictures and status.shared_fps == 5.0
    assert status.shared_source == SharedSource("always_on", "VA-API")
    binding = PluginBinding("motion", "c1")
    assert binding_text(binding, status, True) == (
        "Running · 5 pictures a second · decoded by NeuroWatch (VA-API) · scaled on the CPU",
        "success",
    )
    assert binding_text(binding, status, True, decoding=False)[0] == (
        "Running · 5 pictures a second"
    )
    lines = binding_decoding(status, "Motion")
    assert lines is not None
    assert lines.decoding == "NeuroWatch's always-on decoder · VA-API · shared at 5.0 fps"
    assert lines.scaling == "In Motion · CPU"


def test_shared_source_is_ignored_while_nothing_is_shared() -> None:
    status = _status(
        {
            "state": "active",
            "shared_pictures": False,
            "shared_source": {"by": "always_on", "decoder": "VA-API"},
            "plugin": {"state": "running", "fps": 4.0, "decoder": "CPU (software)"},
        }
    )
    assert status.shared_source is None
    assert binding_text(PluginBinding("motion", "c1"), status, True)[0] == (
        "Running · 4 pictures a second · decoded in the plugin with CPU (software)"
    )


def test_no_decoding_lines_before_the_binding_runs() -> None:
    assert binding_decoding(None, "Motion") is None
    assert binding_decoding(BindingStatus(state="attaching"), "Motion") is None
    assert binding_decoding(BindingStatus(state="active"), "Motion") is not None


def _status_line(decoder: str, on_gpu: bool) -> str:
    """The plugins' ``scaling::status_line`` (in every plugin): at most
    96 characters, the decoder part cut first (with "…"), never the suffix."""
    suffix = " · scaled down on the " + ("GPU" if on_gpu else "CPU")
    room = 96 - len(suffix)
    if len(decoder) <= room:
        return decoder + suffix
    return decoder[: room - 1].rstrip() + "…" + suffix


def test_a_status_line_cut_short_keeps_its_scaling_words() -> None:
    note = "CPU (software): hardware decoder VA-API kept failing while running, then " * 2
    for on_gpu in (True, False):
        line = _status_line(note, on_gpu)
        assert len(line) == 96 and "…" in line
        parsed = parse_plugin_decoder(line)
        assert parsed.scaling == ("GPU" if on_gpu else "CPU")
        assert parsed.decoder.endswith("…") and len(parsed.decoder) == 71
        assert not parsed.shares


def test_older_shared_pictures_with_scaling_words_still_mean_no_decoder() -> None:
    parsed = parse_plugin_decoder("Video wall pictures (no decoding here) · scaled down on the CPU")
    assert parsed.scaling == "CPU" and parsed.shares
    assert parse_plugin_decoder(_status_line(OLD_SHARED, False)).shares
