"""Remove what an older version left in a portable copy's folder.

A portable download (.tar.gz, .zip) unpacked over an older one keeps the old
version's files beside the new ones: a stale ``neurowatch-<old>.dist-info``
made the window show the old version, and old libraries or GStreamer plugins
could be loaded. Every build lists its own program files in
``.neurowatch-files`` (one path per line, relative to the bundle, folders
ending in a separator). A portable copy (``.portable``) removes, when it
starts, the files an earlier version's list named and this one does not (see
``remove_leftovers``: never a file that was not listed, since the copy may sit
in a shared folder). Installed copies leave this to their installer or package
manager (``scripts/installer``). Settings, cameras
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
SEEN = ".neurowatch-files.seen"  # the file list of the previous start (what we own)
FOLDERS = ("app", "bin", "gstreamer")
#: Folders that only NeuroWatch fills: ``bin`` may be a shared prefix's own.
OWN_FOLDERS = ("app/neurowatch-ui", "gstreamer")


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


def _listed(path: Path, key: Callable[[str], str]) -> set[str]:
    text = path.read_text(encoding="utf-8")
    return {key(line.strip().replace("\\", "/")) for line in text.splitlines() if line.strip()}


def _inside_program_folders(root: Path, relative: str) -> Path | None:
    """``root/relative`` when it is under a program folder and stays inside
    ``root`` (no ``..``, no symlinked parent leading elsewhere), else None."""
    parts = relative.rstrip("/").split("/")
    if not parts or parts[0] not in FOLDERS or any(p in ("", ".", "..") for p in parts):
        return None
    path = root / relative.rstrip("/")
    try:
        parent = path.parent.resolve()
        parent.relative_to(root.resolve())
    except (OSError, ValueError):
        return None
    return path


def remove_leftovers(root: Path, *, windows: bool = os.name == "nt") -> int:
    """Remove the files an earlier version of this copy installed and the
    current file list no longer names (and the folders that leaves empty).
    Returns the number of files removed.

    Only what an earlier install's file list named is ever removed: a copy
    unpacked into a shared folder (``/usr/local``, a home folder) keeps every
    file that is not ours. The earlier list is the one saved beside the
    current one (``SEEN``) at the previous start. A copy that has none yet
    (updated from a version before this record) clears only its own folders
    (``OWN_FOLDERS``) of unlisted files, and never ``bin``, which may be shared.
    """
    key = _key_for(windows)
    listed = _listed(root / MANIFEST, key)
    seen_path = root / SEEN
    removed = 0
    if seen_path.is_file():
        stale = sorted(_listed(seen_path, key) - listed, reverse=True)  # deepest first
        for entry in stale:
            path = _inside_program_folders(root, entry)
            if path is None:
                continue
            try:
                if entry.endswith("/"):
                    if not path.is_symlink():
                        path.rmdir()  # only an empty folder
                elif path.is_symlink() or path.is_file():
                    path.unlink()
                    removed += 1
            except OSError as e:  # in use (Windows), or not empty: the next start retries
                logger.debug("Could not remove %s: %s", path, e)
    else:
        removed = _remove_unlisted(root, listed, key)
    _remember(root)
    return removed


def _remember(root: Path) -> None:
    """Keep this build's file list as what a later update may remove."""
    try:
        temporary = root / (SEEN + ".new")
        temporary.write_bytes((root / MANIFEST).read_bytes())
        temporary.replace(root / SEEN)
    except OSError as e:
        logger.warning("Could not record the file list in %s: %s", root, e)


def _remove_unlisted(root: Path, listed: set[str], key: Callable[[str], str]) -> int:
    removed = 0
    for folder in OWN_FOLDERS:
        top = root / folder
        if not top.is_dir() or top.is_symlink():
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
