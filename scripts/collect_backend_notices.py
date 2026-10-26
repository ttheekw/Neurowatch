"""Copy dependency notices from the webrtc-sys build used by Cargo."""

from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path


def collect(messages: Path, destination: Path) -> None:
    notices: Path | None = None
    for line in messages.read_text(encoding="utf-8-sig").splitlines():
        message = json.loads(line)
        if message.get("reason") != "build-script-executed":
            continue
        # Path package IDs may omit the name when it matches the directory.
        package = message.get("package_id", "")
        if (
            "#webrtc-sys@" in package
            or package.rsplit("#", 1)[0].endswith("/webrtc-sys")
            or package.startswith("webrtc-sys ")
        ):
            notices = Path(message["out_dir"]) / "THIRD_PARTY_NOTICES.md"
    if notices is None or not notices.is_file():
        raise RuntimeError("the backend build did not provide webrtc-sys dependency notices")
    shutil.copyfile(notices, destination)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("messages", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    collect(args.messages, args.destination)
