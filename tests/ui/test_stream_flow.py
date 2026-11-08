"""The stream tree model: no Qt, no video."""

from neurowatch.models.stream_flow import codec_text, flatten, stream_flow


def _flow(row, plugins=(), **kwargs):
    return stream_flow({"state": "streaming", "stream": "main", **row}, list(plugins),
                       bitrate="2 Mbps", **kwargs)


def test_buffer_feeds_consumers_and_decoding_is_its_own_box():
    root = _flow({"roles": {"recording": 1, "desktop": 2, "browser": 1, "plugin": 1},
                  "codec": "h264", "buffer_bytes": 1536, "buffer_units": 7, "queue_drops": 0,
                  "fps": 25},
                 [{"name": "Counting", "pictures": "compressed", "decoder": "NVIDIA NVDEC"}],
                 desktop_decoder="VA-API", desktop_fps=25)
    assert (root.kind, root.chip, root.detail) == ("stream", "H.264", "In 2 Mbps · 25.0 fps")
    (buffer,) = root.children
    assert (buffer.title, buffer.detail) == ("Frame buffer", "1.5 KiB · 7 frames · 0 drops")
    assert [(n.kind, n.title) for n in buffer.children] == [
        ("consumer", "Recording"),
        ("decoder", "H.264 decoder · video wall"),
        ("consumer", "Web"),
        ("decoder", "H.264 decoder · in Counting"),
    ]
    wall = buffer.children[1]
    assert (wall.detail, wall.chip) == ("VA-API · 25.0 fps", None), "the codec once, on the stream"
    assert [n.title for n in wall.children] == ["Video wall ×2"]
    own = buffer.children[3]
    assert (own.chip, own.detail, own.children[0].title) == (None, "NVIDIA NVDEC", "Counting")


def test_a_shared_plugin_hangs_off_the_wall_decoder_dashed():
    root = _flow({"roles": {"desktop": 1, "plugin": 1}},
                 [{"name": "Motion", "pictures": "shared", "shared_fps": 9.5,
                   "decoder": "Video wall pictures (no decoding here)"}])
    wall = root.children[0].children[0]
    assert wall.kind == "decoder"
    video_wall, motion = wall.children
    assert (motion.title, motion.dashed) == ("Motion", True)
    assert motion.detail == "Uses these decoded pictures · 9.5 fps", "the measured shared rate"
    assert not video_wall.dashed
    # Without a wall decoder on this stream it borrows another stream's.
    root = _flow({"roles": {"plugin": 1}}, [{"name": "Motion", "pictures": "shared"}],
                 shared_elsewhere="Main")
    (motion,) = root.children[0].children
    assert motion.dashed and motion.detail.endswith("(Main)")


def test_an_always_on_decoder_feeds_the_plugins_and_the_wall_keeps_its_own():
    plugins = [{"name": "Motion", "pictures": "shared", "shared_fps": 5.0},
               {"name": "Counting", "pictures": "compressed", "decoder": "CPU (software)"}]
    root = _flow({"roles": {"desktop": 1, "plugin": 2, "recording": 1}, "codec": "h264",
                  "always_decoded": True}, plugins, desktop_decoder="VA-API", desktop_fps=9)
    buffer = root.children[0]
    assert [(n.kind, n.title, [c.title for c in n.children]) for n in buffer.children] == [
        ("consumer", "Recording", []),
        ("decoder", "H.264 decoder · always on", ["Motion"]),
        ("decoder", "H.264 decoder · video wall", ["Video wall"]),
        ("decoder", "H.264 decoder · in Counting", ["Counting"]),
    ]
    always = buffer.children[1]
    assert (always.chip, always.detail) == (None, "VA-API")
    assert always.children[0].dashed
    # Nobody on a wall, nobody sharing yet: the decoder runs, and says so.
    idle = _flow({"roles": {}, "always_decoded": True}).children[0].children
    assert [(n.title, [c.title for c in n.children]) for n in idle] == [
        ("Decoder · always on", ["Nothing uses these pictures yet"])]
    # A wall showing the stream uses the always-on pictures: no decoder of its own.
    shared = _flow({"roles": {"plugin": 1}, "always_decoded": True, "shared_views": 2},
                   [{"name": "Motion", "pictures": "shared"}]).children[0].children
    assert [(n.title, [c.title for c in n.children]) for n in shared] == [
        ("Decoder · always on", ["Video wall ×2", "Motion"])]


def test_plugin_names_never_exceed_the_measured_count_and_unknowns_stay_unknown():
    root = _flow({"roles": {"plugin": 1}}, [{"name": "A", "pictures": "compressed"},
                                            {"name": "B", "pictures": "compressed"}])
    titles = [r.node.title for r in flatten(root)[2:]]
    assert "B" not in " ".join(titles)
    assert _flow({"roles": {"plugin": 3}}).children[0].children[0].title == "Plugins ×3"
    assert _flow({"roles": {}}).children[0].children[0].title == "No consumers"
    unavailable = _flow({"state": "unavailable"})
    assert [n.title for n in unavailable.children] == ["Consumers not reported"]
    disconnected = _flow({"state": "disconnected", "roles": {}})
    assert disconnected.title == "Main · Not in use", "unused, not broken"
    assert [n.title for n in disconnected.children] == ["Opens when something needs it"]
    assert _flow({"roles": {}}).children[0].detail == "— · — frames · 0 drops"
    assert codec_text("h265") == "H.265" and codec_text("av1") == "AV1"
    assert codec_text(None) is None


def test_flatten_gives_the_guides_a_tree_connector_needs():
    root = _flow({"roles": {"desktop": 1, "recording": 1}})
    rows = flatten(root)
    shape = [(r.node.title, r.depth, r.last, r.guides) for r in rows]
    assert shape == [
        ("Main · Connected", 0, True, ()),
        ("Frame buffer", 1, True, ()),
        ("Recording", 2, False, (False,)),
        ("Decoder · video wall", 2, True, (False,)),
        ("Video wall", 3, True, (False, False)),
    ]


def test_words_for_no_decoder_of_its_own_are_never_shown_as_a_decoder():
    for words in ("NeuroWatch's pictures (no decoding here)",
                  "Video wall pictures (no decoding here)", "desktop (shared)"):
        root = _flow({"roles": {"plugin": 1}},
                     [{"name": "Motion", "pictures": "compressed", "decoder": words}])
        decoder = root.children[0].children[0]
        assert decoder.detail == "", words


def test_a_stream_cloud_backup_copies_shows_it_beside_the_recording():
    root = _flow({"roles": {"recording": 1}, "cloud_backup": True})
    assert [(n.kind, n.title) for n in root.children[0].children] == [
        ("consumer", "Recording"),
        ("consumer", "Cloud backup"),
    ]
    alone = _flow({"roles": {}, "cloud_backup": True})
    assert [n.title for n in alone.children[0].children] == ["Cloud backup"]
