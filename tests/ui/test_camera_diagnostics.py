"""Synthetic snapshots verify the combined camera flow without opening video."""

from PySide6.QtWidgets import QLabel

from neurowatch.widgets.camera_diagnostics import CameraDiagnostics


def _texts(widget) -> str:
    return "\n".join(getattr(label, "full", "") or label.text()
                     for label in widget.findChildren(QLabel))


def test_groups_health_and_streams_with_semantic_connection_colors(qtbot):
    view = CameraDiagnostics()
    qtbot.addWidget(view)
    health = [{"camera_id": "a", "state": "streaming", "decoder_fps": 25,
               "renderer_fps": None, "decoder_drops": 2, "renderer_drops": 0,
               "end_to_end_latency_ms": 41}]
    streams = [
        {"camera_id": "a", "stream": "main", "state": "streaming", "consumers": 3,
         "roles": {"recording": 1, "desktop": 1, "browser": 1}, "video_bps": 2_000_000,
         "audio_bps": 64_000, "fps": 25, "buffer_bytes": 1024, "buffer_units": 5,
         "queue_drops": 0},
        {"camera_id": "a", "stream": "sub", "state": "idle", "consumers": 0, "roles": {}},
        {"camera_id": "a", "stream": "other_3", "state": "disconnected",
         "consumers": 0, "roles": {}},
    ]
    view.update_snapshot(health, streams, {"a": "<Gate>", "b": "Yard"}, decoder="VA-API")
    assert set(view.cards) == {"a", "b"}
    card = view.cards["a"]
    # An unused stream is grey "Not in use": a lost camera shows Retrying.
    assert [row.property("tone") for row in card.stream_rows] == ["green", "yellow", "unknown"]
    text = _texts(card)
    for value in ("<Gate>", "Recording", "Video wall", "Web", "25.0", "41", "2 Mbps",
                  "Waiting to disconnect", "Not in use", "Frame buffer",
                  "1.0 KiB · 5 frames · 0 drops", "Decoder · video wall", "VA-API · 25.0 fps"):
        assert value in text, value
    assert view.cards["b"].property("tone") == "unknown"
    # Refreshes keep the camera widget; missing telemetry never means disconnected.
    view.update_snapshot(health, None, {"a": "Gate"})
    assert view.cards["a"] is card
    assert card.property("tone") == "unknown"
    assert not card.stream_rows
    assert "not reported" in _texts(card).lower()
    assert "b" not in view.cards


def test_health_only_and_connection_only_cameras_keep_missing_values_unknown(qtbot):
    view = CameraDiagnostics()
    qtbot.addWidget(view)
    view.update_snapshot([{"camera_id": "health", "state": "error"}], [
        {"camera_id": "stream", "stream": "main+sub", "state": "unavailable"},
    ], {})
    assert set(view.cards) == {"health", "stream"}
    assert view.cards["stream"].property("tone") == "unknown"
    text = _texts(view.cards["stream"])
    assert "Health not reported" in text
    assert "Main / Sub (shared)" in text
    assert "Consumers not reported" in text
    assert "Frame buffer" not in text, "no buffer is claimed without telemetry"
    assert "No consumers" not in text


def test_compact_cards_keep_wires_visible_under_the_main_window_theme(qtbot):
    from PySide6.QtWidgets import QVBoxLayout, QWidget

    shell = QWidget()
    shell.setStyleSheet("QWidget { background-color:#12121c; color:#c8c8dc; }")
    layout = QVBoxLayout(shell)
    view = CameraDiagnostics(shell)
    layout.addWidget(view)
    qtbot.addWidget(shell)
    health = [{"camera_id": "a", "state": "streaming", "decoder_fps": 25}]
    row = {"camera_id": "a", "state": "streaming", "roles": {"plugin": 1},
           "consumers": 1, "fps": 25, "video_bps": 2000000, "buffer_bytes": 1024}
    streams = [dict(row, stream="main"), dict(row, stream="sub")]
    view.update_snapshot(health, streams, {"a": "Gate"}, plugin_consumers=[
        {"camera_id": "a", "stream": "main", "name": "Motion fixture",
         "pictures": "compressed", "codec": "h264"},
    ])
    shell.resize(920, 700)
    shell.show()
    qtbot.waitUntil(lambda: view.cards["a"].width() > 400)
    card = view.cards["a"]
    # Two streams, each stream → buffer → (decoder → plugin | plugin): 9 boxes.
    assert card.height() <= 260, card.height()
    assert not card.details_button.isChecked()
    assert ("plugin", "Motion fixture", "") in card.stream_rows[0].boxes()
    assert len(card.rails) == 2
    for rail in card.rails:
        pixel = rail.grab().toImage().pixelColor(rail.width() // 2, 8)
        assert pixel.green() > pixel.red() * 1.3, "green wire must be painted above the theme"
    connector = card.stream_rows[0].rows[1].guides
    image = connector.grab().toImage()
    assert any(image.pixelColor(x, 11).green() > 150 for x in range(image.width())), (
        "tree connectors are painted in their own widgets"
    )
    card.details_button.click()
    assert all(row.cache_label.isVisible() for row in card.stream_rows)
    view.update_snapshot(health, streams, {"a": "Renamed"})
    assert card.details_button.isChecked(), "expanded details survive telemetry refreshes"


def test_wide_desktop_keeps_unattributed_plugins_and_reflows(qtbot):
    view = CameraDiagnostics()
    qtbot.addWidget(view)
    row = {"camera_id": "a", "stream": "main", "state": "streaming", "consumers": 3,
           "roles": {"plugin": 3}}
    view.update_snapshot([], [row], {"a": "A", "b": "B"}, plugin_consumers=[
        {"camera_id": "a", "stream": "sub", "name": "Wrong stream"},
        {"camera_id": "a", "stream": "main", "name": "Motion fixture"},
        {"camera_id": "a", "stream": "main", "name": "Motion fixture"},
    ])
    view.resize(1200, 700)
    view.show()
    qtbot.waitUntil(lambda: view.cards["b"].x() > view.cards["a"].x())
    boxes = view.cards["a"].stream_rows[0].boxes()
    assert [title for kind, title, _ in boxes if kind == "plugin"] == [
        "Motion fixture", "Motion fixture", "Plugin"], "one box per binding, then the rest"
    assert "Wrong stream" not in _texts(view.cards["a"])
    view.resize(650, 700)
    qtbot.waitUntil(lambda: view.cards["b"].y() > view.cards["a"].y())


def test_health_readout_distinguishes_idle_unreported_and_measured_values():
    from neurowatch.widgets.camera_diagnostics import camera_health_readout

    health = {"decoder_fps": 0, "renderer_fps": None, "decoder_drops": 0,
              "renderer_drops": 0, "end_to_end_latency_ms": 0}
    idle = camera_health_readout(health, [{"roles": {"recording": 1}}], True)
    assert idle.decode == "Idle (no desktop view)"
    assert idle.render == "Not reported"
    assert idle.render_drops == "Not reported"
    assert idle.latency == "Not measured"
    assert idle.decode_drops == "0"
    assert camera_health_readout(health, [{"roles": {"desktop": 1}}], True).decode == "0.0 fps"
    measured = camera_health_readout(dict(health, decoder_fps=25, renderer_fps=20,
                                         end_to_end_latency_ms=0), [], True)
    assert measured.decode == "25.0 fps"
    assert measured.render == "20.0 fps"
    assert measured.render_drops == "0"
    assert measured.latency == "0 ms"
    assert camera_health_readout(None, [], False).decode == "Not reported"


def test_desktop_fits_three_columns_and_reflows_without_empty_column_gaps(qtbot):
    view = CameraDiagnostics()
    qtbot.addWidget(view)
    view.update_snapshot([], [], {"a": "A", "b": "B", "c": "C", "d": "D"})
    view.resize(1200, 600)
    view.show()
    qtbot.waitUntil(lambda: view.cards["c"].x() > view.cards["b"].x())
    assert view.cards["a"].y() == view.cards["c"].y()
    assert view.cards["d"].y() > view.cards["a"].y()
    view.resize(760, 600)
    qtbot.waitUntil(lambda: view.cards["c"].y() > view.cards["a"].y())
    view.resize(500, 600)
    qtbot.waitUntil(lambda: view.cards["b"].y() > view.cards["a"].y())
    assert view.cards["a"].width() > 470


def test_decoding_shows_as_boxes_direct_or_shared_with_the_video_wall(qtbot):
    view = CameraDiagnostics()
    qtbot.addWidget(view)
    row = {"camera_id": "a", "state": "streaming", "consumers": 4, "codec": "h265",
           "roles": {"plugin": 2, "recording": 1, "desktop": 1}, "buffer_bytes": 2048,
           "buffer_units": 12, "queue_drops": 1}
    consumers = [
        {"camera_id": "a", "stream": "main", "name": "Vehicle counting", "codec": "h265",
         "pictures": "compressed", "decoder": "VA-API (Intel, AMD) · yolo"},
        {"camera_id": "a", "stream": "main", "name": "Motion", "codec": "h265",
         "pictures": "shared", "decoder": "Video wall pictures (no decoding here)"},
    ]
    view.update_snapshot([{"camera_id": "a", "state": "streaming", "decoder_fps": 24.5}],
                         [dict(row, stream="main")], {"a": "Gate"}, decoder="VA-API",
                         plugin_consumers=consumers)
    stream = view.cards["a"].stream_rows[0]
    assert [(r.row.depth, r.row.node.kind, r.row.node.title) for r in stream.rows] == [
        (0, "stream", "Main · Connected"),
        (1, "buffer", "Frame buffer"),
        (2, "consumer", "Recording"),
        (2, "decoder", "H.265 decoder · video wall"),
        (3, "consumer", "Video wall"),
        (3, "plugin", "Motion"),
        (2, "decoder", "H.265 decoder · in Vehicle counting"),
        (3, "plugin", "Vehicle counting"),
    ]
    rows = {r.row.node.title: r for r in stream.rows}
    assert rows["Main · Connected"].box.chip.text() == "H.265"
    assert rows["Frame buffer"].box.detail.full == "2.0 KiB · 12 frames · 1 drops"
    assert rows["H.265 decoder · video wall"].box.detail.full == "VA-API · 24.5 fps"
    assert rows["H.265 decoder · video wall"].box.chip is None, "the codec once, on the stream"
    assert rows["Motion"].row.node.dashed, "shared pictures: a dashed connector"
    assert "no second decoder" in rows["Motion"].accessibleName()
    decoder = rows["H.265 decoder · in Vehicle counting"]
    assert decoder.box.chip is None
    assert decoder.box.detail.full == "VA-API (Intel, AMD) · yolo"
    assert not rows["Vehicle counting"].row.node.dashed

    # Motion goes back to its own decoder: the same widgets follow, no rebuild.
    first = stream.rows[0]
    consumers[1] = dict(consumers[1], pictures="compressed", decoder="CPU (software)")
    view.update_snapshot([], [dict(row, stream="main")], {"a": "Gate"}, decoder="VA-API",
                         plugin_consumers=consumers)
    assert stream.rows[0] is first
    titles = [r.row.node.title for r in stream.rows]
    assert "H.265 decoder · in Motion" in titles
    assert all(not r.row.node.dashed for r in stream.rows)


def test_a_plugin_sharing_another_streams_wall_and_older_recorders(qtbot):
    view = CameraDiagnostics()
    qtbot.addWidget(view)
    base = {"camera_id": "a", "state": "streaming"}
    view.update_snapshot([], [
        dict(base, stream="main", consumers=1, roles={"desktop": 1}),
        dict(base, stream="sub", consumers=1, roles={"plugin": 1}),
    ], {"a": "Gate"}, plugin_consumers=[
        {"camera_id": "a", "stream": "sub", "name": "Motion", "pictures": "shared"},
    ])
    motion = next(r for r in view.cards["a"].stream_rows[1].rows if r.row.node.title == "Motion")
    assert motion.row.node.dashed
    assert motion.box.detail.full == "Uses the video wall's decoded pictures (Main)"
    # An older recorder says neither the codec nor how the plugin gets video.
    view.update_snapshot([], [dict(base, stream="main", consumers=1, roles={"plugin": 1})],
                         {"a": "Gate"}, plugin_consumers=[
                             {"camera_id": "a", "stream": "main", "name": "Motion"}])
    stream = view.cards["a"].stream_rows[0]
    assert stream.rows[0].box.chip is None
    assert [r.row.node.kind for r in stream.rows] == ["stream", "buffer", "plugin"]
