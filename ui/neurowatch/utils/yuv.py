"""YUV frame conversion helpers."""

from __future__ import annotations

try:
    import numpy as _np

    _HAS_NUMPY = True
except Exception:  # pragma: no cover - optional dependency
    _HAS_NUMPY = False

_FrameBuffer = bytes | bytearray | memoryview


def has_numpy() -> bool:
    return _HAS_NUMPY


def i420_to_gray8(data: _FrameBuffer, width: int, height: int) -> bytes | None:
    """Extract the luma plane from I420 as packed Gray8 pixels."""
    if width <= 0 or height <= 0:
        return None
    y_size = width * height
    if len(data) < y_size:
        return None
    return bytes(memoryview(data)[:y_size])


def i420_to_rgb888(data: _FrameBuffer, width: int, height: int) -> bytes | None:
    """Convert planar I420 bytes into packed RGB888 bytes."""
    if width <= 0 or height <= 0:
        return None

    y_size = width * height
    uv_width = (width + 1) // 2
    uv_height = (height + 1) // 2
    uv_size = uv_width * uv_height
    expected = y_size + uv_size * 2
    if len(data) < expected:
        return None

    if _HAS_NUMPY:
        return _i420_to_rgb888_numpy(data, width, height, y_size, uv_width, uv_height, uv_size)
    return _i420_to_rgb888_python(data, width, height, y_size, uv_width, uv_height, uv_size)


def _i420_to_rgb888_numpy(
    data: _FrameBuffer,
    width: int,
    height: int,
    y_size: int,
    uv_width: int,
    uv_height: int,
    uv_size: int,
) -> bytes:
    y = _np.frombuffer(data, dtype=_np.uint8, count=y_size, offset=0).reshape((height, width))
    u = _np.frombuffer(data, dtype=_np.uint8, count=uv_size, offset=y_size).reshape(
        (uv_height, uv_width)
    )
    v = _np.frombuffer(data, dtype=_np.uint8, count=uv_size, offset=y_size + uv_size).reshape(
        (uv_height, uv_width)
    )

    # 4:2:0 subsampling: each U/V sample maps to a 2x2 luma block.
    u_up = u.repeat(2, axis=0).repeat(2, axis=1)[:height, :width]
    v_up = v.repeat(2, axis=0).repeat(2, axis=1)[:height, :width]

    c = _np.maximum(y.astype(_np.int32) - 16, 0)
    d = u_up.astype(_np.int32) - 128
    e = v_up.astype(_np.int32) - 128

    r = _np.clip((298 * c + 409 * e + 128) >> 8, 0, 255).astype(_np.uint8)
    g = _np.clip((298 * c - 100 * d - 208 * e + 128) >> 8, 0, 255).astype(_np.uint8)
    b = _np.clip((298 * c + 516 * d + 128) >> 8, 0, 255).astype(_np.uint8)

    return _np.dstack((r, g, b)).tobytes()


def _i420_to_rgb888_python(
    data: _FrameBuffer,
    width: int,
    height: int,
    y_size: int,
    uv_width: int,
    uv_height: int,
    uv_size: int,
) -> bytes:
    y_plane = memoryview(data)[:y_size]
    u_plane = memoryview(data)[y_size : y_size + uv_size]
    v_plane = memoryview(data)[y_size + uv_size : y_size + uv_size * 2]

    out = bytearray(width * height * 3)
    out_idx = 0
    for row in range(height):
        uv_row = min(row // 2, uv_height - 1)
        for col in range(width):
            y = int(y_plane[row * width + col])
            uv_col = min(col // 2, uv_width - 1)
            uv_idx = uv_row * uv_width + uv_col
            u = int(u_plane[uv_idx]) - 128
            v = int(v_plane[uv_idx]) - 128

            c = y - 16
            if c < 0:
                c = 0

            r = (298 * c + 409 * v + 128) >> 8
            g = (298 * c - 100 * u - 208 * v + 128) >> 8
            b = (298 * c + 516 * u + 128) >> 8

            out[out_idx] = 0 if r < 0 else 255 if r > 255 else r
            out[out_idx + 1] = 0 if g < 0 else 255 if g > 255 else g
            out[out_idx + 2] = 0 if b < 0 else 255 if b > 255 else b
            out_idx += 3

    return bytes(out)
