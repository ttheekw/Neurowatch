"""Public release checks. No camera, account or cloud-link data is sent."""

from __future__ import annotations

import json
import re
from urllib.request import Request, urlopen

CATALOG_URL = "https://neurowatch.app/v1/downloads?version=latest"
MAX_RESPONSE_BYTES = 256 * 1024
_VERSION = re.compile(
    r"^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?(?:\+[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?$"
)


def release_version(value: object) -> tuple[tuple[int, int, int], bool] | None:
    """A release's numeric version and whether it is a prerelease; never a git SHA."""
    if not isinstance(value, str) or len(value) > 64:
        return None
    match = _VERSION.fullmatch(value)
    if match is None:
        return None
    return (int(match[1]), int(match[2]), int(match[3])), bool(match[4])


def newer_version(current: str, payload: object) -> str | None:
    """Newest stable release newer than this build (including prerelease → stable)."""
    installed = release_version(current)
    if installed is None or not isinstance(payload, dict):
        return None
    versions = payload.get("versions")
    if not isinstance(versions, list):
        return None
    candidates: list[tuple[tuple[int, int, int], str]] = []
    for item in versions:
        if not isinstance(item, dict) or item.get("prerelease") is not False:
            continue
        version = item.get("version")
        parsed = release_version(version)
        if not isinstance(version, str) or parsed is None or parsed[1]:
            continue
        if parsed[0] > installed[0] or (parsed[0] == installed[0] and installed[1]):
            candidates.append((parsed[0], version))
    return max(candidates)[1] if candidates else None


def read_latest(current: str) -> str | None:
    """Bounded, timed public HTTPS read; called only on a background worker."""
    if release_version(current) is None:
        return None  # Development commits have no comparable release version.
    request = Request(CATALOG_URL, headers={"User-Agent": "NeuroWatch-UpdateCheck"})
    with urlopen(request, timeout=8) as response:
        body = response.read(MAX_RESPONSE_BYTES + 1)
    if len(body) > MAX_RESPONSE_BYTES:
        raise ValueError("Release catalog is too large")
    return newer_version(current, json.loads(body))
