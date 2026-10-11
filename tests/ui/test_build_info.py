"""Build identity: git tag, short SHA, or package-version fallback."""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest
from PySide6.QtWidgets import QLabel

from neurowatch import build_info
from neurowatch.build_info import BuildIdentity, BuildSource


class _FakeGit:
    """Stand-in for ``subprocess.run`` answering the git commands we issue."""

    def __init__(self, *, tag: str | None, sha: str | None) -> None:
        self.tag = tag
        self.sha = sha
        self.calls: list[tuple[list[str], dict[str, Any]]] = []

    def __call__(self, args: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        self.calls.append((list(args), kwargs))
        if "describe" in args:
            if self.tag is None:
                return subprocess.CompletedProcess(args, 128, "", "fatal: no tag exactly matches")
            return subprocess.CompletedProcess(args, 0, f"{self.tag}\n", "")
        if "rev-parse" in args:
            if self.sha is None:
                return subprocess.CompletedProcess(args, 128, "", "fatal: bad revision")
            return subprocess.CompletedProcess(args, 0, f"{self.sha}\n", "")
        raise AssertionError(f"unexpected git command {args}")


@pytest.fixture(autouse=True)
def _clear_cache():
    build_info.build_identity.cache_clear()
    yield
    build_info.build_identity.cache_clear()


@pytest.fixture
def git_tree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    (tmp_path / ".git").mkdir()
    pkg = tmp_path / "ui" / "neurowatch"
    pkg.mkdir(parents=True)
    monkeypatch.setattr(build_info, "_PACKAGE_DIR", pkg)
    monkeypatch.setattr(build_info.shutil, "which", lambda name: "/usr/bin/git")
    return tmp_path


def test_exact_tag_is_shown(git_tree: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake = _FakeGit(tag="v1.4.0", sha="12b0159")
    monkeypatch.setattr(build_info.subprocess, "run", fake)

    ident = build_info.build_identity()

    assert ident.label == "v1.4.0"
    assert ident.source is BuildSource.TAG
    assert ident.display == "Version v1.4.0"
    args, kwargs = fake.calls[0]
    assert args[0] == "/usr/bin/git"
    assert "--exact-match" in args
    assert kwargs.get("shell", False) is False
    assert kwargs["cwd"] == git_tree
    assert kwargs["timeout"] <= 2
    assert kwargs["stdin"] is subprocess.DEVNULL


def test_untagged_commit_shows_short_sha(git_tree: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake = _FakeGit(tag=None, sha="12b0159")
    monkeypatch.setattr(build_info.subprocess, "run", fake)

    ident = build_info.build_identity()

    assert ident.label == "12b0159"
    assert ident.source is BuildSource.COMMIT
    assert any("rev-parse" in args and "--short" in args for args, _ in fake.calls)


def test_no_git_directory_falls_back_to_package_version(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    pkg = tmp_path / "site-packages" / "neurowatch"
    pkg.mkdir(parents=True)
    monkeypatch.setattr(build_info, "_PACKAGE_DIR", pkg)
    monkeypatch.setattr(build_info, "_find_git_root", lambda start: None)

    def _no_subprocess(*_a: Any, **_k: Any) -> None:
        raise AssertionError("git must not run without a .git checkout")

    monkeypatch.setattr(build_info.subprocess, "run", _no_subprocess)

    ident = build_info.build_identity()

    assert ident.source is BuildSource.PACKAGE
    assert ident.label == build_info.package_version()
    assert ident.label


def test_git_executable_missing_falls_back(git_tree: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(build_info.shutil, "which", lambda name: None)
    ident = build_info.build_identity()
    assert ident.source is BuildSource.PACKAGE


@pytest.mark.parametrize(
    "exc", [OSError("boom"), subprocess.TimeoutExpired(["git"], 1.0)]
)
def test_git_failure_or_timeout_falls_back(
    git_tree: Path, monkeypatch: pytest.MonkeyPatch, exc: Exception
) -> None:
    def _raise(*_a: Any, **_k: Any) -> None:
        raise exc

    monkeypatch.setattr(build_info.subprocess, "run", _raise)
    ident = build_info.build_identity()
    assert ident.source is BuildSource.PACKAGE
    assert ident.label


def test_unsafe_tag_text_is_rejected_in_favour_of_sha(
    git_tree: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = _FakeGit(tag="<b>evil</b>", sha="12b0159")
    monkeypatch.setattr(build_info.subprocess, "run", fake)
    assert build_info.build_identity().label == "12b0159"


def test_non_hex_sha_is_rejected(git_tree: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake = _FakeGit(tag=None, sha="not-a-sha")
    monkeypatch.setattr(build_info.subprocess, "run", fake)
    assert build_info.build_identity().source is BuildSource.PACKAGE


def test_package_version_is_non_empty_when_metadata_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def _missing(name: str) -> str:
        raise build_info.PackageNotFoundError(name)

    monkeypatch.setattr(build_info, "_metadata_version", _missing)
    import neurowatch

    assert build_info.package_version() == neurowatch.__version__


def test_find_git_root_accepts_worktree_git_file(tmp_path: Path) -> None:
    (tmp_path / ".git").write_text("gitdir: /elsewhere\n")
    nested = tmp_path / "ui" / "neurowatch"
    nested.mkdir(parents=True)
    assert build_info._find_git_root(nested) == tmp_path


def test_identity_is_cached(git_tree: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake = _FakeGit(tag="v1.4.0", sha="12b0159")
    monkeypatch.setattr(build_info.subprocess, "run", fake)
    build_info.build_identity()
    build_info.build_identity()
    assert len(fake.calls) == 1


# ── UI surfaces share the same identity ─────────────────────────────────


@pytest.fixture
def fixed_identity(monkeypatch: pytest.MonkeyPatch) -> BuildIdentity:
    ident = BuildIdentity(label="v7.7.7-ui", source=BuildSource.TAG, package_version="0.1.0")
    monkeypatch.setattr(build_info, "build_identity", lambda: ident)
    return ident


def _texts(widget) -> list[str]:
    return [lbl.text() for lbl in widget.findChildren(QLabel)]


def test_about_dialog_shows_build_identity(qtbot, fixed_identity: BuildIdentity) -> None:
    from neurowatch.dialogs.about_dialog import AboutDialog

    dlg = AboutDialog()
    qtbot.addWidget(dlg)
    assert fixed_identity.display in _texts(dlg)
    assert not any("1.0.0" in text for text in _texts(dlg))


def test_status_bar_shows_build_identity(qtbot, fixed_identity: BuildIdentity) -> None:
    from neurowatch.widgets.status_bar_widget import StatusBarWidget

    bar = StatusBarWidget()
    qtbot.addWidget(bar)
    assert fixed_identity.display in _texts(bar)


def test_welcome_screen_defaults_to_build_identity(qtbot, fixed_identity: BuildIdentity) -> None:
    from neurowatch.widgets.welcome_screen import WelcomeScreen

    screen = WelcomeScreen()
    qtbot.addWidget(screen)
    assert screen.version_text() == fixed_identity.display
