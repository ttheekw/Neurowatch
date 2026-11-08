#!/usr/bin/env python3
"""Check/copy canonical browser SDK files into the sibling portal checkout."""

from __future__ import annotations

import argparse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--portal", type=Path, default=ROOT.parent / "Neurowatch-webapp")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    target = args.portal / "frontend/src/features/plugins"
    differences = []
    for source_name, target_name in [
        ("host.js", "web-host.js"),
        ("host.d.ts", "web-host.d.ts"),
        ("plugin-sdk.js", "plugin-sdk.js"),
        ("annotation-renderer.js", "annotation-renderer.js"),
        ("annotation-renderer.d.ts", "annotation-renderer.d.ts"),
    ]:
        content = (ROOT / "sdk/web" / source_name).read_bytes()
        destination = target / target_name
        if destination.exists() and destination.read_bytes() == content:
            continue
        differences.append(target_name)
        if not args.check:
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(content)
    if args.check and differences:
        print("Portal SDK differs: " + ", ".join(differences))
        return 1
    print("Portal SDK matches" if args.check else f"Synced {len(differences)} browser SDK files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
