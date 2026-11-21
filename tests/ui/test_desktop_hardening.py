"""Hardening of the desktop app: notifications, enrollment code input, private
exports, startup-file escaping and the backend a frozen build runs."""

from __future__ import annotations

import io
import logging
import os
import stat
import sys
from pathlib import Path
from typing import Any

import pytest

from neurowatch import headless, tray
from neurowatch.cloud import headless as cloud
from neurowatch.service import paths
from neurowatch.service.manager import (
    ServiceCommand,
    ServiceError,
    systemd_unit,
    tray_desktop_entry,
)
from neurowatch.utils.private_files import (
    PrivateRotatingFileHandler,
    copy_private,
    open_private,
    private_dir,
)

CODE = "11111111-1111-4111-8111-111111111111.22222222-2222-4222-8222-222222222222.proof-token-1234"
LINKED = {"phase": "linked", "cloud_configured": True, "cloud_reachable": True}


# -- notifications -----------------------------------------------------------


def test_notification_bodies_carry_no_markup_on_linux(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(tray, "IS_WINDOWS", False)
    monkeypatch.setattr(sys, "platform", "linux")
    body = tray.notification_body('Motion on <a href="http://x">Door</a> & <img src=y>')
    assert "<" not in body and ">" not in body
    assert body == 'Motion on &lt;a href="http://x"&gt;Door&lt;/a&gt; &amp; &lt;img src=y&gt;'


def test_notification_bodies_stay_plain_on_windows(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(tray, "IS_WINDOWS", True)
    assert tray.notification_body("A & B") == "A & B"


# -- enrollment code ---------------------------------------------------------


class _Pipe(io.StringIO):
    def __init__(self, text: str, tty: bool = False) -> None:
        super().__init__(text)
        self._tty = tty

    def isatty(self) -> bool:
        return self._tty


def test_a_code_given_as_an_argument_still_works_but_warns() -> None:
    said: list[str] = []
    assert cloud.resolve_code(CODE, said.append, environ={}) == CODE
    assert said and "process list" in said[0]


def test_a_code_comes_from_the_variable_standard_input_or_a_prompt() -> None:
    said: list[str] = []
    env = {cloud.CODE_ENV: f" {CODE}\n"}
    assert cloud.resolve_code(None, said.append, environ=env) == CODE
    assert cloud.resolve_code("", said.append, environ=env) == CODE
    assert cloud.resolve_code("-", said.append, environ={}, stdin=_Pipe(CODE + "\n")) == CODE
    prompted: list[str] = []

    def prompt(text: str) -> str:
        prompted.append(text)
        return CODE + "\n"

    assert (
        cloud.resolve_code(None, said.append, environ={}, stdin=_Pipe("", tty=True), prompt=prompt)
        == CODE
    )
    assert prompted and said == []  # none of these put the code on a command line


def test_the_cloud_link_command_reads_the_code_from_standard_input(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, Any]] = []

    def backend(method: str, params: dict[str, Any], timeout: float) -> Any:
        calls.append((method, params))
        return (
            {"phase": "unlinked", "cloud_configured": True} if method == "cloud.status" else LINKED
        )

    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)
    monkeypatch.setattr(sys, "stdin", _Pipe(CODE + "\n"))
    monkeypatch.delenv(cloud.CODE_ENV, raising=False)
    out = io.StringIO()
    assert headless.main(["cloud", "link", "--name", "Lab"], out=out) == 0
    assert [method for method, _ in calls] == ["cloud.status", "cloud.begin"]
    assert "process list" not in out.getvalue()


def test_the_monitor_link_option_without_a_code_uses_the_variable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    args = headless._parser().parse_args(["monitor", "--link", "--name", "Lab"])
    assert args.link == ""  # bare --link: no code on the command line
    monkeypatch.setenv(cloud.CODE_ENV, CODE)
    assert cloud.resolve_code(args.link, lambda _t: None) == CODE


# -- private exports ---------------------------------------------------------


@pytest.mark.skipif(os.name == "nt", reason="POSIX modes")
def test_a_copied_config_is_private_even_over_a_loose_file(tmp_path: Path) -> None:
    source = tmp_path / "config.json"
    source.write_text('{"cameras": []}')
    source.chmod(0o644)
    target = tmp_path / "export.json"
    target.write_text("old, and readable by everyone")
    target.chmod(0o666)
    copy_private(source, target)
    assert target.read_text() == '{"cameras": []}'
    assert stat.S_IMODE(target.stat().st_mode) == 0o600
    fresh = tmp_path / "fresh.json"
    copy_private(source, fresh)
    assert stat.S_IMODE(fresh.stat().st_mode) == 0o600


# -- private logs ------------------------------------------------------------


def _mode(path: Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


@pytest.mark.skipif(sys.platform == "win32", reason="Unix permissions")
def test_logs_are_opened_owner_only_and_loose_ones_are_tightened(tmp_path: Path) -> None:
    old = os.umask(0o002)  # a typical desktop umask: files 0664 by default
    try:
        fresh = tmp_path / "fresh.log"
        with open_private(fresh) as log:
            log.write(b"one\n")
        assert _mode(fresh) == 0o600
        loose = tmp_path / "loose.log"
        loose.write_text("written by an earlier version\n")
        loose.chmod(0o664)
        with open_private(loose, "a", encoding="utf-8") as log:
            log.write("two\n")
        assert _mode(loose) == 0o600
        # Appends, never truncates.
        assert loose.read_text() == "written by an earlier version\ntwo\n"
        with pytest.raises(ValueError):
            open_private(loose, "w")
    finally:
        os.umask(old)


@pytest.mark.skipif(sys.platform == "win32", reason="Unix permissions")
def test_log_folders_and_every_rotated_log_are_owner_only(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    old = os.umask(0o002)
    try:
        state = tmp_path / "state"
        (state / "neurowatch" / "logs").mkdir(parents=True)
        (state / "neurowatch").chmod(0o775)
        (state / "neurowatch" / "logs").chmod(0o775)
        monkeypatch.setenv("XDG_STATE_HOME", str(state))
        monkeypatch.setattr(paths, "IS_WINDOWS", False)
        log = paths.log_path("service")
        assert _mode(log.parent) == 0o700 and _mode(log.parent.parent) == 0o700
        assert _mode(private_dir(tmp_path / "new" / "deeper")) == 0o700

        handler = PrivateRotatingFileHandler(
            tmp_path / "ui.log", maxBytes=64, backupCount=2, encoding="utf-8"
        )
        record = logging.LogRecord("t", logging.INFO, __file__, 1, "x" * 50, None, None)
        for _ in range(5):
            handler.emit(record)
        handler.close()
        written = sorted(tmp_path.glob("ui.log*"))
        assert len(written) == 3, written
        assert all(_mode(path) == 0o600 for path in written), [oct(_mode(p)) for p in written]
    finally:
        os.umask(old)


@pytest.mark.skipif(sys.platform == "win32", reason="Unix permissions")
def test_the_service_log_the_backend_writes_into_is_owner_only(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import subprocess

    from neurowatch.service import manager

    seen: dict[str, Any] = {}

    def fake_popen(argv: list[str], **kwargs: Any) -> Any:
        seen["mode"] = stat.S_IMODE(os.fstat(kwargs["stdout"].fileno()).st_mode)
        return object()

    monkeypatch.setattr(manager, "IS_WINDOWS", False)
    monkeypatch.setattr(subprocess, "Popen", fake_popen)
    old = os.umask(0o002)
    try:
        log = tmp_path / "service.log"
        log.write_text("an earlier version's log\n")
        log.chmod(0o664)
        manager.spawn_detached(ServiceCommand(["true"]), log)
    finally:
        os.umask(old)
    assert seen["mode"] == 0o600 and _mode(log) == 0o600


# -- startup files -----------------------------------------------------------


def _unit(argv: list[str], env: dict[str, str] | None = None, cwd: Path | None = None) -> str:
    return systemd_unit(ServiceCommand(argv, cwd=cwd, env=env or {}))


def test_unit_text_escapes_specifiers_and_variables() -> None:
    text = _unit(["/opt/neu %h/nw $HOME", "a\\b"], {"X": "50%"}, Path("/srv/100%"))
    exec_line = next(line for line in text.splitlines() if line.startswith("ExecStart="))
    assert "%%h" in exec_line and "$$HOME" in exec_line and "a\\\\b" in exec_line
    assert "%h" not in exec_line.replace("%%h", "")
    assert 'Environment="X=50%%"' in text and "WorkingDirectory=/srv/100%%" in text


@pytest.mark.parametrize("bad", ["a\nExecStartPre=/bin/evil", "a\rb"])
def test_unit_text_refuses_line_breaks(bad: str) -> None:
    with pytest.raises(ServiceError):
        _unit(["/bin/nw"], {"X": bad})
    with pytest.raises(ServiceError):
        _unit(["/bin/nw", bad])
    with pytest.raises(ServiceError):
        _unit(["/bin/nw"], cwd=Path(bad))


def test_desktop_entry_quotes_the_command_and_doubles_field_codes() -> None:
    entry = tray_desktop_entry(["/opt/My %U/ui", "--tray", 'q"$x`'])
    exec_line = next(line for line in entry.splitlines() if line.startswith("Exec="))
    # In quotes the spec escapes " ` $ \ with a backslash; the file then doubles backslashes.
    assert exec_line == r'Exec="/opt/My %%U/ui" --tray "q\\"\\$x\\`"'
    assert len([line for line in entry.splitlines() if line.startswith("Exec=")]) == 1
    plain = tray_desktop_entry(["/opt/neurowatch/ui", "--tray"])
    assert "Exec=/opt/neurowatch/ui --tray" in plain


def test_desktop_entry_refuses_line_breaks() -> None:
    with pytest.raises(ServiceError):
        tray_desktop_entry(["/bin/x", "a\nTerminal=true"])


# -- the backend of a frozen build ------------------------------------------


def test_a_frozen_build_runs_only_its_bundled_backend(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    other = tmp_path / "elsewhere"
    other.mkdir()
    (other / "nw-backend").write_text("#!/bin/sh\n")
    (other / "nw-backend").chmod(0o755)
    monkeypatch.setenv("NW_BACKEND_BIN", str(other / "nw-backend"))
    monkeypatch.setenv("PATH", str(other))
    monkeypatch.setenv("NW_SOURCE_ROOT", str(Path(__file__).resolve().parents[2]))
    bundle = tmp_path / "bundle/app/neurowatch-ui"
    bundle.mkdir(parents=True)
    exe = bundle / "neurowatch-ui"
    exe.write_text("")
    monkeypatch.setattr(sys, "executable", str(exe))
    monkeypatch.setattr(sys, "frozen", True, raising=False)

    assert paths.source_root() is None  # the override does not make it a checkout
    assert paths.packaged_backend() is None  # neither override nor PATH

    (tmp_path / "bundle/bin").mkdir()
    (tmp_path / "bundle/bin/nw-backend").write_text("")
    assert paths.packaged_backend() == (tmp_path / "bundle/bin/nw-backend").resolve()


def test_a_checkout_keeps_the_backend_overrides(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delattr(sys, "frozen", raising=False)
    monkeypatch.setenv("NW_BACKEND_BIN", str(tmp_path / "mine"))
    assert paths.packaged_backend() == tmp_path / "mine"
