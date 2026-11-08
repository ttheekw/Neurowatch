#!/usr/bin/env python3
"""Sync the standalone motion daemon into release sources; --check is read-only.

The daemon remains a separate executable. Only Cargo dependency paths differ
between the standalone checkout and this workspace's bundled source snapshot.
"""

from __future__ import annotations

import argparse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT.parent / "Neurowatch-Motion")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    source = args.source.resolve()
    target = ROOT / "crates/nw-motion"
    files = sorted(
        p for folder in ("src", "tests", "web") for p in (source / folder).rglob("*") if p.is_file()
    )
    files += [
        source / name
        for name in ("Cargo.toml", "LICENSE", "README.md")
        if (source / name).is_file()
    ]
    differences: list[str] = []
    for path in files:
        relative = path.relative_to(source)
        content = path.read_bytes()
        if relative == Path("Cargo.toml"):
            text = content.decode().replace("../Neurowatch/crates/", "../")
            content = text.split("[profile.dev]")[0].encode()
        destination = target / relative
        if destination.exists() and destination.read_bytes() == content:
            continue
        differences.append(str(relative))
        if not args.check:
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(content)
    expected = {p.relative_to(source) for p in files}
    for folder in ("src", "tests", "web"):
        for path in (target / folder).rglob("*"):
            if path.is_file() and path.relative_to(target) not in expected:
                differences.append(str(path.relative_to(target)))
                if not args.check:
                    path.unlink()
    if args.check and differences:
        print("Motion source differs: " + ", ".join(differences))
        return 1
    print("Motion source matches" if args.check else f"Synced {len(differences)} motion files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
