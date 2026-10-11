"""Frame ring transport selection: /dev/shm files on POSIX, named sections on Windows."""

from __future__ import annotations

import struct

from neurowatch.ipc import frame_reader as module


class _Ring(bytearray):
    closed = False

    def close(self) -> None:
        self.closed = True


def _one_frame_ring() -> _Ring:
    ring = _Ring(64 + 8 * 24)
    struct.pack_into("<I", ring, 0, 3)
    struct.pack_into("<III", ring, 64 + 3 * 24, 2, 2, 12)
    ring[64 + 3 * 24 + 12 : 64 + 4 * 24] = bytes(range(12))
    return ring


def test_windows_mapping_name_matches_backend() -> None:
    assert module.windows_mapping_name("cam-1") == "Local\\neurowatch_cam-1"
    assert module.windows_mapping_name("pb_abc") == "Local\\neurowatch_pb_abc"


def test_windows_reader_maps_named_section_and_emits_frame(qtbot, monkeypatch) -> None:
    ring = _one_frame_ring()
    calls: list[tuple[object, ...]] = []

    def fake_mmap(fileno, length, tagname=None, **_kwargs):
        calls.append((fileno, length, tagname))
        return ring

    monkeypatch.setattr(module, "_IS_WINDOWS", True)
    monkeypatch.setattr(module, "_windows_mapping_exists", lambda name: True)
    monkeypatch.setattr(module, "SLOT_SIZE", 24)
    monkeypatch.setattr(module.mmap, "mmap", fake_mmap)
    reader = module.FrameReaderThread("cam-1")
    emitted: list[tuple[object, ...]] = []

    def on_frame(*args: object) -> None:
        emitted.append(args)
        reader._stop_event.set()

    reader.frame_ready.connect(on_frame)
    reader.run()

    assert calls == [(-1, module.TOTAL_BYTES, "Local\\neurowatch_cam-1")]
    assert emitted == [("cam-1", bytes(range(12)), 2, 2, "rgb")]
    assert ring.closed


def test_windows_reader_waits_for_backend_section(qtbot, monkeypatch) -> None:
    checked: list[str] = []

    def exists(name: str) -> bool:
        checked.append(name)
        return False

    def fail_mmap(*_args, **_kwargs):
        raise AssertionError("reader must not create the section before the backend")

    monkeypatch.setattr(module, "_IS_WINDOWS", True)
    monkeypatch.setattr(module, "_windows_mapping_exists", exists)
    monkeypatch.setattr(module.mmap, "mmap", fail_mmap)
    reader = module.FrameReaderThread("offline")
    waits: list[float] = []

    def wait(timeout: float) -> bool:
        waits.append(timeout)
        reader._stop_event.set()
        return True

    monkeypatch.setattr(reader._stop_event, "wait", wait)
    reader.run()

    assert checked == ["Local\\neurowatch_offline"]
    assert waits == [0.5]


def test_posix_reader_still_uses_shm_prefix_path(qtbot, monkeypatch, tmp_path) -> None:
    ring = _one_frame_ring()
    (tmp_path / "cam").touch()
    opened: list[int] = []

    def fake_mmap(fileno, length, *_args, **_kwargs):
        opened.append(length)
        return ring

    monkeypatch.setattr(module, "_IS_WINDOWS", False)
    monkeypatch.setattr(module, "SHM_PREFIX", str(tmp_path) + "/")
    monkeypatch.setattr(module, "SLOT_SIZE", 24)
    monkeypatch.setattr(module.mmap, "mmap", fake_mmap)
    reader = module.FrameReaderThread("cam")
    emitted: list[tuple[object, ...]] = []

    def on_frame(*args: object) -> None:
        emitted.append(args)
        reader._stop_event.set()

    reader.frame_ready.connect(on_frame)
    reader.run()

    assert opened == [module.TOTAL_BYTES]
    assert emitted == [("cam", bytes(range(12)), 2, 2, "rgb")]
