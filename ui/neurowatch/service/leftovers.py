"""Remove what an older version left in a portable copy's folder.

A portable download (.tar.gz, .zip) unpacked over an older one keeps the old
version's files beside the new ones: a stale ``neurowatch-<old>.dist-info``
made the window show the old version, and old libraries or GStreamer plugins
could be loaded. Every build lists its own program files in
``.neurowatch-files`` (one path per line, relative to the bundle, folders
ending in a separator). A portable copy (``.portable``) removes everything
else under its program folders when it starts. Installed copies leave this to
their installer or package manager (``scripts/installer``). Settings, cameras
and recordings live elsewhere and are never touched.
"""

from __future__ import annotations

import logging
import os
import sys
from collections.abc import Callable
from pathlib import Path

logger = logging.getLogger(__name__)

MANIFEST = ".neurowatch-files"
PORTABLE_MARKER = ".portable"
FOLDERS = ("app", "bin", "gstreamer")


def bundle_root(executable: str | None = None) -> Path | None:
    """The folder a frozen build runs from (``<root>/app/neurowatch-ui/<exe>``),
    or None for a source checkout."""
    if executable is None:
        if not getattr(sys, "frozen", False):
            return None
        executable = sys.executable
    path = Path(executable).resolve()
    if path.parent.parent.name != "app":
        return None
    return path.parent.parent.parent


def _key_for(windows: bool) -> Callable[[str], str]:
    # Windows file names ignore case.
    return str.casefold if windows else str


def remove_leftovers(root: Path, *, windows: bool = os.name == "nt") -> int:
    """Remove the files (and the folders that leaves empty) under the program
    folders of ``root`` that its file list does not name. Returns the number
    of files removed."""
    key = _key_for(windows)
    text = (root / MANIFEST).read_text(encoding="utf-8")
    listed = {key(line.strip().replace("\\", "/")) for line in text.splitlines() if line.strip()}
    removed = 0
    for folder in FOLDERS:
        top = root / folder
        if not top.is_dir():
            continue
        # Deepest first, so a folder is empty once what it held is gone.
        for current, dirs, files in os.walk(top, topdown=False):
            base = Path(current)
            entries = [(name, False) for name in files] + [(name, True) for name in dirs]
            for name, is_dir in entries:
                path = base / name
                relative = path.relative_to(root).as_posix()
                if is_dir and not path.is_symlink():
                    if key(relative + "/") not in listed:
                        try:
                            path.rmdir()  # only an empty folder
                        except OSError:
                            pass
                elif key(relative) not in listed:
                    try:
                        path.unlink()
                        removed += 1
                    except OSError as e:  # in use (Windows): the next start retries
                        logger.warning("Could not remove %s: %s", path, e)
    return removed


def clean_portable_copy(root: Path | None = None) -> int:
    """At start: a portable copy removes what an earlier version left in its
    folder. Returns the number of files removed (0 when not portable)."""
    root = root if root is not None else bundle_root()
    if root is None or not (root / PORTABLE_MARKER).is_file() or not (root / MANIFEST).is_file():
        return 0
    try:
        removed = remove_leftovers(root)
    except OSError as e:
        logger.warning("Could not check %s for files an earlier version left: %s", root, e)
        return 0
    if removed:
        logger.info("Removed %d file(s) an earlier version left in %s", removed, root)
    return removed


def leftovers_message(removed: int) -> str:
    return (
        f"NeuroWatch was updated in this folder: {removed} file(s) left over from the "
        "earlier version were removed. Settings, cameras and recordings are kept."
    )
