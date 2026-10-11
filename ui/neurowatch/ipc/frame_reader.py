"""Shared memory frame reader — runs in a QThread, emits frame_ready signal."""

from __future__ import annotations

import contextlib
import logging
import mmap
import os
import struct
import sys
import threading
import time
from collections.abc import Iterator

from PySide6.QtCore import QThread, Signal

# Must match nw-core/src/frame.rs shm constants
SLOT_COUNT = 8
MAX_FRAME_BYTES = 3840 * 2160 * 3  # max raw RGB24 payload (3 bytes/pixel)
SLOT_HEADER_BYTES = 12  # width(4) + height(4) + data_len(4)
SLOT_SIZE = SLOT_HEADER_BYTES + MAX_FRAME_BYTES
HEADER_BYTES = 64
PREVIEW_RATE_OFFSET = 4  # optional u32 milli-FPS; 0 = unlimited
PREVIEW_FORMAT_OFFSET = 8
NV12_CAPABILITY = 0x31565559  # YUV1: explicitly negotiated, older backends ignore
NV12_SLOT_FLAG = 1 << 31
TOTAL_BYTES = HEADER_BYTES + SLOT_COUNT * SLOT_SIZE
SHM_PREFIX = "/dev/shm/neurowatch_"
# Windows has no /dev/shm: the backend creates a named, pagefile-backed section.
WINDOWS_MAPPING_PREFIX = "Local\\neurowatch_"
_IS_WINDOWS = sys.platform == "win32"
logger = logging.getLogger(__name__)


def windows_mapping_name(key: str) -> str:
    """Kernel object name of a backend frame ring on Windows."""
    return f"{WINDOWS_MAPPING_PREFIX}{key}"


def _windows_mapping_exists(name: str) -> bool:
    """True when the backend has created the named section.

    ``mmap.mmap(-1, ..., tagname=...)`` creates a missing section, so probe with
    ``OpenFileMappingW`` first to keep "backend not streaming yet" distinguishable.
    """
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)  # type: ignore[attr-defined,unused-ignore]
    kernel32.OpenFileMappingW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
    kernel32.OpenFileMappingW.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    file_map_read = 0x0004
    handle = kernel32.OpenFileMappingW(file_map_read, False, name)
    if not handle:
        return False
    kernel32.CloseHandle(handle)
    return True


class FrameReaderThread(QThread):
    """Copy the latest negotiated RGB24/NV12 frame from shared memory.

    RGB remains the default. Only a GPU-capable wall requests NV12, whose
    colour descriptor travels with each frame so queued format changes are safe.
    """

    # cam_id, pixel_data, width, height, format ("rgb" or "nv12:<descriptor>")
    frame_ready = Signal(str, bytes, int, int, str)

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)
        self._cam_id = cam_id
        self._stop_event = threading.Event()
        self._had_error = False
        self._accept_nv12 = False
        self._max_emit_fps = max_emit_fps
        self._emit_interval = 0.0
        self._poll_interval_ms = 16
        self._next_emit_ts = 0.0
        self.set_max_emit_fps(max_emit_fps)

    # How often (seconds) to check whether the SHM file has been replaced
    # by the backend restarting a pipeline.
    _INODE_CHECK_INTERVAL = 2.0

    def run(self) -> None:
        shm_path = (
            windows_mapping_name(self._cam_id) if _IS_WINDOWS else f"{SHM_PREFIX}{self._cam_id}"
        )
        last_slot = -1
        _first_frame = True
        _shm_was_open = False

        logger.debug("Frame reader thread starting for cam_id=%s", self._cam_id)

        while not self._stop_event.is_set():
            try:
                with self._open_ring(shm_path) as (mm, our_ino):
                    _next_inode_check = time.monotonic() + self._INODE_CHECK_INTERVAL
                    requested_preview_rate = None
                    requested_format = None
                    if not _shm_was_open:
                        logger.debug(
                            "SHM opened successfully for cam_id=%s at %s",
                            self._cam_id,
                            shm_path,
                        )
                        _shm_was_open = True
                    try:
                        while not self._stop_event.is_set():
                            # Periodically check if the backend has replaced the
                            # SHM file (new pipeline = new inode).  If so, exit
                            # the inner loop so the outer loop re-opens the file.
                            now = time.monotonic()
                            if our_ino is not None and now >= _next_inode_check:
                                _next_inode_check = now + self._INODE_CHECK_INTERVAL
                                try:
                                    if os.stat(shm_path).st_ino != our_ino:
                                        logger.debug(
                                            "SHM replaced for cam_id=%s, reopening",
                                            self._cam_id,
                                        )
                                        break  # exit inner loop → outer loop reopens
                                except OSError:
                                    break  # path gone, let outer loop handle it

                            # Update from the reader thread, including after a
                            # ring replacement. The backend can skip RGB conversion
                            # and SHM copies before an off-page frame is produced.
                            fps = self._max_emit_fps
                            preview_rate = (
                                0
                                if fps is None or fps <= 0
                                else min(120_000, max(1, round(fps * 1000)))
                            )
                            if preview_rate != requested_preview_rate:
                                struct.pack_into("<I", mm, PREVIEW_RATE_OFFSET, preview_rate)
                                requested_preview_rate = preview_rate
                            capability = NV12_CAPABILITY if self._accept_nv12 else 0
                            if capability != requested_format:
                                struct.pack_into("<I", mm, PREVIEW_FORMAT_OFFSET, capability)
                                requested_format = capability
                            write_idx = struct.unpack_from("<I", mm, 0)[0]
                            if write_idx != last_slot:
                                slot_offset = HEADER_BYTES + write_idx * SLOT_SIZE
                                width = struct.unpack_from("<I", mm, slot_offset)[0]
                                height = struct.unpack_from("<I", mm, slot_offset + 4)[0]
                                data_len = struct.unpack_from("<I", mm, slot_offset + 8)[0]
                                nv12 = bool(data_len & NV12_SLOT_FLAG)
                                data_len &= ~NV12_SLOT_FLAG

                                if (
                                    data_len > 0
                                    and data_len <= MAX_FRAME_BYTES
                                    and width > 0
                                    and height > 0
                                ):
                                    now_emit = time.monotonic()
                                    if self._emit_interval > 0.0 and now_emit < self._next_emit_ts:
                                        self._stop_event.wait(self._poll_interval_ms / 1000)
                                        continue
                                    data_start = slot_offset + SLOT_HEADER_BYTES
                                    # Own one packed frame independently of the ring.
                                    # NV12 stays YUV until the GPU texture shader.
                                    pixel_format = "rgb"
                                    if nv12:
                                        expected = width * height + 2 * ((width + 1) // 2) * (
                                            (height + 1) // 2
                                        )
                                        descriptor = struct.unpack_from("<I", mm, data_start)[0]
                                        if (
                                            not self._accept_nv12
                                            or data_len != expected + 4
                                            or descriptor & ~0x73
                                            or descriptor & 3 not in (1, 2, 3)
                                        ):
                                            last_slot = write_idx
                                            self._stop_event.wait(self._poll_interval_ms / 1000)
                                            continue
                                        pixel_format = f"nv12:{descriptor}"
                                        data_start += 4
                                        data_len -= 4
                                    elif data_len != width * height * 3:
                                        last_slot = write_idx
                                        self._stop_event.wait(self._poll_interval_ms / 1000)
                                        continue
                                    raw = mm[data_start : data_start + data_len]
                                    if _first_frame:
                                        logger.info(
                                            "First frame received for cam_id=%s (%dx%d)",
                                            self._cam_id,
                                            width,
                                            height,
                                        )
                                        _first_frame = False
                                    # Do not consume a slot until it is emitted.
                                    # Keep a cadence rather than adding polling
                                    # jitter to every frame's throttle deadline.
                                    last_slot = write_idx
                                    if self._next_emit_ts == 0.0:
                                        self._next_emit_ts = now_emit + self._emit_interval
                                    else:
                                        self._next_emit_ts = max(
                                            now_emit,
                                            self._next_emit_ts + self._emit_interval,
                                        )
                                    self.frame_ready.emit(
                                        self._cam_id, raw, width, height, pixel_format
                                    )
                            self._stop_event.wait(self._poll_interval_ms / 1000)
                    finally:
                        # Release the request if the reader stops or changes rings.
                        struct.pack_into("<I", mm, PREVIEW_RATE_OFFSET, 0)
                        struct.pack_into("<I", mm, PREVIEW_FORMAT_OFFSET, 0)
                        mm.close()
                        self._had_error = False
            except FileNotFoundError:
                if _shm_was_open:
                    logger.info(
                        "SHM file unavailable for cam_id=%s (pipeline stopped/restarting)",
                        self._cam_id,
                    )
                    _shm_was_open = False
                self._stop_event.wait(0.5)  # pipeline not started yet — retry
            except Exception as e:
                if not self._had_error:
                    logger.warning(
                        "Frame reader error for %s: %s; retrying",
                        self._cam_id,
                        e,
                    )
                    self._had_error = True
                self._stop_event.wait(0.25)

        logger.debug("Frame reader thread stopping for cam_id=%s", self._cam_id)

    @staticmethod
    @contextlib.contextmanager
    def _open_ring(path: str) -> Iterator[tuple[mmap.mmap, int | None]]:
        """Map a frame ring; yields the mapping and its inode (``None`` on Windows).

        A Windows named section is shared by every writer for the same key, so a
        restarted pipeline reuses it and no replacement check is needed.
        """
        if _IS_WINDOWS:
            if not _windows_mapping_exists(path):
                raise FileNotFoundError(path)
            # ``tagname`` exists only in the Windows stubs.
            yield mmap.mmap(-1, TOTAL_BYTES, tagname=path), None  # type: ignore[call-arg,unused-ignore]
            return
        with open(path, "r+b") as f:
            yield mmap.mmap(f.fileno(), TOTAL_BYTES), os.fstat(f.fileno()).st_ino

    def stop(self) -> None:
        # A stop immediately after start() must not be undone by run() starting.
        self._stop_event.set()
        self.wait()

    def set_accept_nv12(self, enabled: bool) -> None:
        """Enable only after the GUI has validated its GPU rendering support."""
        self._accept_nv12 = enabled

    def set_max_emit_fps(self, value: float | None) -> None:
        self._max_emit_fps = value
        self._next_emit_ts = 0.0
        if value is None or value <= 0:
            self._emit_interval = 0.0
            self._poll_interval_ms = 16
            return
        self._emit_interval = 1.0 / value
        # Poll around 2x target FPS for responsiveness without busy looping.
        self._poll_interval_ms = max(8, min(120, int(500.0 / value)))
