"""Bounded saved-image replies. No Qt, filesystem paths or live capture."""

from __future__ import annotations

import base64
import binascii

MAX_JPEG_BYTES = 128 * 1024


def decode_saved_image(reply: object) -> bytes | None:
    if not isinstance(reply, dict):
        raise ValueError("Invalid image reply")
    if reply.get("available") is False:
        return None
    encoded = reply.get("jpeg_b64")
    if (
        reply.get("available") is not True
        or reply.get("mime") != "image/jpeg"
        or not isinstance(encoded, str)
    ):
        raise ValueError("Invalid image reply")
    if len(encoded) > 4 * ((MAX_JPEG_BYTES + 2) // 3):
        raise ValueError("Image is too large")
    try:
        data = base64.b64decode(encoded, validate=True)
    except (binascii.Error, ValueError) as error:
        raise ValueError("Invalid image encoding") from error
    if not 4 <= len(data) <= MAX_JPEG_BYTES or not data.startswith(b"\xff\xd8"):
        raise ValueError("Invalid JPEG")
    return data
