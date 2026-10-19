#!/usr/bin/env python3
"""Write the downloads-page manifest of one release.

    scripts/release_manifest.py --version v1.2.3 --repo owner/name \\
        --free dist/free --unlimited dist/unlimited --out manifest.json \\
        [--index-in index.json --index-out index.json]

The downloads page (https://neurowatch.app/downloads) reads it from the
private bucket at ``releases/<version>/manifest.json``: the free packages
link to the GitHub release, the unlimited ones are objects under
``releases/<version>/unlimited/`` that the portal hands only to buyers, as
short-lived signed links. Files that are not packages (SHA256SUMS, the
version-free copies for the README) are left out.

``--index-in``/``--index-out`` also merge it into ``releases/index.json``,
the one object the page reads: every version's manifest, newest first (a
version published again replaces its entry), at most ``MAX_INDEXED``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from datetime import UTC, datetime
from pathlib import Path

MANIFEST_VERSION = 1
INDEX_VERSION = 1
MAX_INDEXED = 50

# (pattern on the file name, platform, arch, kind, label)
_PACKAGES: tuple[tuple[str, str, str, str, str], ...] = (
    (
        r"neurowatch-.+-windows-x86_64-setup\.exe",
        "windows",
        "x86_64",
        "installer",
        "Windows installer",
    ),
    (
        r"neurowatch-.+-windows-x86_64\.zip",
        "windows",
        "x86_64",
        "portable",
        "Windows portable (.zip)",
    ),
    (r"neurowatch_.+_amd64\.deb", "linux", "x86_64", "deb", "Ubuntu / Debian (.deb)"),
    (r"neurowatch_.+_arm64\.deb", "linux", "arm64", "deb", "Ubuntu / Debian ARM (.deb)"),
    (r"neurowatch-.+\.x86_64\.rpm", "linux", "x86_64", "rpm", "Fedora (.rpm)"),
    (r"neurowatch-.+\.aarch64\.rpm", "linux", "arm64", "rpm", "Fedora ARM (.rpm)"),
    (r"neurowatch-.+-x86_64\.pkg\.tar\.zst", "linux", "x86_64", "arch", "Arch Linux"),
    (
        r"neurowatch-.+-linux-x86_64\.tar\.gz",
        "linux",
        "x86_64",
        "portable",
        "Linux portable (.tar.gz)",
    ),
    (
        r"neurowatch-.+-linux-arm64\.tar\.gz",
        "linux",
        "arm64",
        "portable",
        "Linux ARM portable (.tar.gz)",
    ),
)


def classify(name: str) -> dict[str, str] | None:
    for pattern, platform, arch, kind, label in _PACKAGES:
        if re.fullmatch(pattern, name):
            return {"platform": platform, "arch": arch, "kind": kind, "label": label}
    return None


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def packages(folder: Path) -> list[dict[str, object]]:
    found = []
    for path in sorted(folder.iterdir() if folder.is_dir() else []):
        info = classify(path.name) if path.is_file() else None
        if info is None:
            continue
        found.append(
            {"name": path.name, **info, "size": path.stat().st_size, "sha256": _sha256(path)}
        )
    return found


def manifest(version: str, repo: str, free: Path, unlimited: Path) -> dict[str, object]:
    release = f"https://github.com/{repo}/releases/download/{version}"
    return {
        "manifest_version": MANIFEST_VERSION,
        "version": version,
        "prerelease": "-" in version,
        "published_at": datetime.now(UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
        "release_notes_url": f"https://github.com/{repo}/releases/tag/{version}",
        "free": [{**p, "url": f"{release}/{p['name']}"} for p in packages(free)],
        "unlimited": [
            {**p, "key": f"releases/{version}/unlimited/{p['name']}"} for p in packages(unlimited)
        ],
    }


def merged_index(previous: object, entry: dict[str, object]) -> dict[str, object]:
    """``previous`` (an index, or anything unreadable) with ``entry`` added."""
    versions = previous.get("versions") if isinstance(previous, dict) else None
    kept = [
        v
        for v in (versions if isinstance(versions, list) else [])
        if isinstance(v, dict) and v.get("version") != entry["version"]
    ]
    ordered = sorted([entry, *kept], key=lambda v: str(v.get("published_at", "")), reverse=True)
    return {"index_version": INDEX_VERSION, "versions": ordered[:MAX_INDEXED]}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--free", type=Path, required=True)
    parser.add_argument("--unlimited", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--index-in", type=Path, help="the current index (may be missing)")
    parser.add_argument("--index-out", type=Path, help="where to write the merged index")
    args = parser.parse_args(argv)
    if not re.fullmatch(r"v\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?", args.version):
        parser.error(f"not a release version: {args.version}")
    data = manifest(args.version, args.repo, args.free, args.unlimited)
    if not data["free"]:
        parser.error(f"no free packages in {args.free}")
    args.out.write_text(json.dumps(data, indent=2) + "\n")
    if args.index_out:
        try:
            previous = json.loads(args.index_in.read_text()) if args.index_in else None
        except (OSError, ValueError):
            previous = None
        index = merged_index(previous, data)
        args.index_out.write_text(json.dumps(index, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
