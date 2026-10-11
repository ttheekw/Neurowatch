"""Deterministic SHM source/clock: polling jitter must not halve preview FPS."""
from __future__ import annotations

import struct

import pytest

from neurowatch.ipc import frame_reader as module


@pytest.mark.parametrize("source_fps", [15, 30])
def test_reader_retains_throttled_frames_and_keeps_cadence(
    qtbot, monkeypatch, tmp_path, source_fps,
):
    class Ring(bytearray):
        def close(self):
            pass

    ring = Ring(64 + 8 * 24)
    for slot in range(8):
        struct.pack_into("<III", ring, 64 + slot * 24, 2, 2, 12)
    path = tmp_path / "cam"
    path.touch()
    monkeypatch.setattr(module, "SHM_PREFIX", str(tmp_path) + "/")
    monkeypatch.setattr(module, "SLOT_SIZE", 24)
    monkeypatch.setattr(module.mmap, "mmap", lambda *_: ring)
    now = 10.0
    monkeypatch.setattr(module.time, "monotonic", lambda: now)
    reader = module.FrameReaderThread("cam", max_emit_fps=15)
    emitted = []
    reader.frame_ready.connect(lambda *_: emitted.append(now))

    def wait(timeout):
        nonlocal now
        now += timeout
        struct.pack_into("<I", ring, 0, int((now - 10.0) * source_fps) % 8)
        if now >= 12.0:
            reader._stop_event.set()

    monkeypatch.setattr(reader._stop_event, "wait", wait)
    reader.run()
    assert 28 <= len(emitted) <= 31, emitted


def test_reader_updates_backend_rate_on_visibility_changes_and_releases_on_stop(
    qtbot, monkeypatch, tmp_path,
):
    class Ring(bytearray):
        def close(self):
            pass

    ring = Ring(64 + 8 * 24)
    path = tmp_path / "cam"
    path.touch()
    monkeypatch.setattr(module, "SHM_PREFIX", str(tmp_path) + "/")
    monkeypatch.setattr(module.mmap, "mmap", lambda *_: ring)
    reader = module.FrameReaderThread("cam", max_emit_fps=2)
    observed = []
    rates = [15, 25, None]

    def wait(_timeout):
        observed.append(struct.unpack_from("<I", ring, module.PREVIEW_RATE_OFFSET)[0])
        if rates:
            reader.set_max_emit_fps(rates.pop(0))
        else:
            reader._stop_event.set()

    monkeypatch.setattr(reader._stop_event, "wait", wait)
    reader.run()
    assert observed == [2_000, 15_000, 25_000, 0]
    assert struct.unpack_from("<I", ring, module.PREVIEW_RATE_OFFSET)[0] == 0
    assert struct.unpack_from("<I", ring, 0)[0] == 0  # writer's frame index untouched
