"""Files and folders only their owner can read: copies of the config (it has
camera passwords), and the logs (camera names and addresses, event history).

On Windows these are plain calls: per-user folders there are already private.
"""

from __future__ import annotations

import logging.handlers
import os
from pathlib import Path
from typing import IO, Any

PRIVATE_FILE_MODE = 0o600
PRIVATE_DIR_MODE = 0o700
_CHUNK = 1 << 20


def private_dir(path: str | os.PathLike[str]) -> Path:
    """Create ``path`` (and its parents) and make it owner-only.

    Parents are left as they are; a folder made loose by an earlier version is
    tightened.
    """
    folder = Path(path)
    folder.mkdir(mode=PRIVATE_DIR_MODE, parents=True, exist_ok=True)
    if os.name != "nt":
        os.chmod(folder, PRIVATE_DIR_MODE)
    return folder


def open_private(
    path: str | os.PathLike[str], mode: str = "ab", encoding: str | None = None
) -> IO[Any]:
    """Open ``path`` for appending, owner-only: created that way (never wider,
    even for an instant) and tightened when an earlier version left it loose.

    ``mode`` is ``"ab"`` or ``"a"`` (text, with ``encoding``).
    """
    if mode not in ("ab", "a"):
        raise ValueError(f"open_private appends only, not {mode!r}")
    flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(os.fspath(path), flags, PRIVATE_FILE_MODE)
    try:
        if os.name != "nt":
            os.fchmod(fd, PRIVATE_FILE_MODE)
        return open(fd, mode, encoding=encoding)
    except BaseException:
        os.close(fd)
        raise


class PrivateRotatingFileHandler(logging.handlers.RotatingFileHandler):
    """A rotating log whose file and every rotated copy are owner-only."""

    def _open(self) -> Any:
        return open_private(self.baseFilename, "a", encoding=self.encoding)


def copy_private(source: str | os.PathLike[str], target: str | os.PathLike[str]) -> None:
    """Copy ``source`` to ``target`` readable by the owner only.

    The mode is set when the file is created (never wider, even for an instant)
    and again on an existing target, whatever the source's own mode is.
    """
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(os.fspath(target), flags, PRIVATE_FILE_MODE)
    try:
        if os.name != "nt":
            os.fchmod(fd, PRIVATE_FILE_MODE)
        with open(fd, "wb", closefd=False) as out, Path(source).open("rb") as src:
            while chunk := src.read(_CHUNK):
                out.write(chunk)
    finally:
        os.close(fd)
