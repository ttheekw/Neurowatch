"""Single source of truth for the UI build identity shown to users.

Resolution order:

1. Source checkout whose HEAD is exactly at a git tag → the tag name.
2. Source checkout without an exact tag → the short commit SHA.
3. No ``.git`` (installed wheel, exported tarball), git missing, git failing
   or timing out → the package version (distribution metadata, else
   ``neurowatch.__version__``).

Git is invoked without a shell, with an argument list, closed stdin, no
terminal prompts and a short timeout. The result is cached so the lookup runs
at most once per process.
"""

from __future__ import annotations

import logging
import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from enum import Enum
from functools import lru_cache
from importlib.metadata import PackageNotFoundError
from importlib.metadata import version as _metadata_version
from pathlib import Path

from neurowatch import __version__

logger = logging.getLogger(__name__)

__all__ = [
    "BuildIdentity",
    "BuildSource",
    "PackageNotFoundError",
    "build_identity",
    "package_version",
]

_DISTRIBUTION = "neurowatch"
_PACKAGE_DIR: Path = Path(__file__).resolve().parent
_GIT_TIMEOUT_S = 1.0
# Labels end up in QLabels (which auto-detect rich text), so only accept
# plain ref-like characters.
_SAFE_TAG = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+/-]{0,63}$")
_SHORT_SHA = re.compile(r"^[0-9a-f]{7,40}$")


class BuildSource(Enum):
    TAG = "tag"
    COMMIT = "commit"
    PACKAGE = "package"


@dataclass(frozen=True)
class BuildIdentity:
    label: str
    source: BuildSource
    package_version: str

    @property
    def display(self) -> str:
        return f"Version {self.label}"


def package_version() -> str:
    """Installed distribution version, else the in-tree ``__version__``."""
    try:
        installed = _metadata_version(_DISTRIBUTION)
    except PackageNotFoundError:
        installed = ""
    return installed.strip() or __version__


def _find_git_root(start: Path) -> Path | None:
    """Nearest ancestor containing ``.git`` (a directory, or a worktree file)."""
    for candidate in (start, *start.parents):
        if (candidate / ".git").exists():
            return candidate
    return None


def _run_git(git: str, root: Path, *args: str) -> str | None:
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0", GIT_OPTIONAL_LOCKS="0")
    try:
        result = subprocess.run(
            [git, *args],
            cwd=root,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=_GIT_TIMEOUT_S,
            env=env,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        logger.debug("git %s failed: %s", args[0] if args else "", exc)
        return None
    if result.returncode != 0:
        return None
    lines = result.stdout.strip().splitlines()
    return lines[0].strip() if lines else None


def _git_label() -> tuple[str, BuildSource] | None:
    root = _find_git_root(_PACKAGE_DIR)
    if root is None:
        return None
    git = shutil.which("git")
    if git is None:
        return None
    tag = _run_git(git, root, "describe", "--tags", "--exact-match", "HEAD")
    if tag and _SAFE_TAG.match(tag):
        return tag, BuildSource.TAG
    sha = _run_git(git, root, "rev-parse", "--short", "HEAD")
    if sha and _SHORT_SHA.match(sha):
        return sha, BuildSource.COMMIT
    return None


@lru_cache(maxsize=1)
def build_identity() -> BuildIdentity:
    """Resolve (once per process) the build identity shown throughout the UI."""
    pkg = package_version()
    resolved = _git_label()
    if resolved is None:
        return BuildIdentity(label=pkg, source=BuildSource.PACKAGE, package_version=pkg)
    label, source = resolved
    return BuildIdentity(label=label, source=source, package_version=pkg)
