"""The README stays user-facing."""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def test_the_readme_stays_user_facing() -> None:
    readme = (ROOT / "README.md").read_text()
    for internal in ("JSON-RPC", "/dev/shm", "NW_IPC_ENDPOINT", "SyncIpcClient", "appsink"):
        assert internal not in readme, f"{internal!r} is a technical detail, not for the README"
