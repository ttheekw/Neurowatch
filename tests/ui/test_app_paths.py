"""Installed UI paths must match the backend and respect test/user isolation."""

from __future__ import annotations

import logging.handlers
from pathlib import Path
from unittest.mock import Mock

import pytest

from neurowatch import main


@pytest.mark.parametrize(
    ("platform", "config_variable", "data_variable"),
    [("linux", "XDG_CONFIG_HOME", "XDG_DATA_HOME"), ("win32", "APPDATA", "LOCALAPPDATA")],
)
def test_platform_config_and_log_paths(
    tmp_path, monkeypatch, platform, config_variable, data_variable
):
    monkeypatch.setattr(main.sys, "platform", platform)
    monkeypatch.delenv("NW_CONFIG", raising=False)
    monkeypatch.setenv(config_variable, str(tmp_path / "config"))
    monkeypatch.setenv(data_variable, str(tmp_path / "data"))
    assert Path(main._active_config_path()) == tmp_path / "config/neurowatch/config.json"

    logger = Mock()
    monkeypatch.setattr(logging, "getLogger", lambda *args: logger)
    main._configure_logging()
    handlers = [call.args[0] for call in logger.addHandler.call_args_list]
    try:
        file_handler = next(
            h for h in handlers if isinstance(h, logging.handlers.RotatingFileHandler)
        )
        assert Path(file_handler.baseFilename) == tmp_path / "data/neurowatch/neurowatch.log"
    finally:
        for handler in handlers:
            handler.close()


def test_explicit_config_override_wins(monkeypatch):
    monkeypatch.setenv("NW_CONFIG", "/explicit/config.json")
    assert main._active_config_path() == "/explicit/config.json"
