"""Transcoding for a browser: the model, the status-bar notice, the tree."""

from neurowatch.models.stream_flow import flatten, stream_flow
from neurowatch.models.transcode import parse_transcodes, summary, tooltip
from neurowatch.widgets.transcode_notice import TranscodeNotice

ONE = {
    "camera_id": "c1",
    "camera_name": "Front door",
    "stream": "main",
    "from": "h265",
    "to": "h264",
    "decoder": "VA-API",
    "encoder": "VA-API",
    "viewers": 1,
}


def test_summary_and_tooltip():
    assert parse_transcodes({}) == [] and parse_transcodes(None) == []
    one = parse_transcodes({"transcodes": [ONE]})
    assert summary(one) == "Transcoding 1 camera for a browser (H.265 → H.264)"
    assert tooltip(one).splitlines()[0] == (
        "Front door · Main · H.265 → H.264 · decoded on VA-API · encoded on VA-API · 1 viewer"
    )
    cpu = parse_transcodes({"transcodes": [{**ONE, "to": "vp8", "encoder": "CPU"}]})
    assert summary(cpu) == "Transcoding 1 camera for a browser on the CPU (VP8)"
    two = parse_transcodes({"transcodes": [ONE, {**ONE, "camera_id": "c2"}]})
    assert summary(two) == "Transcoding 2 cameras for browsers"
    assert summary([]) == ""


def test_notice_shows_only_while_transcoding(qtbot):
    notice = TranscodeNotice()
    qtbot.addWidget(notice)
    clicks = []
    notice.clicked.connect(lambda: clicks.append(1))
    notice.set_status({"transcodes": []})
    assert notice.isHidden()
    notice.set_status({"transcodes": [ONE]})
    assert not notice.isHidden() and notice.text().startswith("Transcoding 1 camera")
    qtbot.mouseClick(
        notice, __import__("PySide6.QtCore", fromlist=["Qt"]).Qt.MouseButton.LeftButton
    )
    assert clicks == [1]
    notice.set_status({})
    assert notice.isHidden()


def test_tree_has_a_transcode_block_before_the_browser():
    row = {
        "state": "streaming",
        "stream": "main",
        "codec": "h265",
        "roles": {"browser": 3},
        "transcode": ONE,
    }
    root = stream_flow(row, [], bitrate="2 Mbps")
    kinds = [(r.node.kind, r.node.title, r.node.detail) for r in flatten(root)]
    assert (
        "transcode",
        "Transcoding H.265 → H.264",
        "decoded on VA-API · encoded on VA-API · 1 viewer",
    ) in kinds
    (buffer,) = root.children
    assert [n.title for n in buffer.children] == ["Web ×2", "Transcoding H.265 → H.264"]
    assert buffer.children[1].children[0].title == "Web"
