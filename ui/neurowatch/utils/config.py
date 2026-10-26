"""JSON config persistence — load and atomic save."""

from __future__ import annotations

import json
import logging
import os
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

_CONFIG_DIR = Path.home() / ".config" / "neurowatch"
CONFIG_PATH = _CONFIG_DIR / "config.json"

_DEFAULTS: dict[str, Any] = {
    "version": 3,
    "site_name": "My Site",
    "wall": {"rows": 4, "cols": 8, "auto_fit": True, "page_interval_sec": 0},
    "cameras": [],
    "ui": {"theme": "dark", "fullscreen": True},
    "debug": {"gst_log": "warning", "extended": False},
}


def load_config() -> dict[str, Any]:
    """Load config from disk, merging defaults for missing keys."""
    if not CONFIG_PATH.exists():
        logger.info("Config not found at %s — using defaults", CONFIG_PATH)
        return dict(_DEFAULTS)

    try:
        data: dict[str, Any] = json.loads(CONFIG_PATH.read_text())
        # Merge top-level defaults for any missing keys
        for k, v in _DEFAULTS.items():
            data.setdefault(k, v)
        if isinstance(data.get("wall"), dict):
            for k, v in _DEFAULTS["wall"].items():
                data["wall"].setdefault(k, v)
        if isinstance(data.get("ui"), dict):
            for k, v in _DEFAULTS["ui"].items():
                data["ui"].setdefault(k, v)
        if isinstance(data.get("debug"), dict):
            for k, v in _DEFAULTS["debug"].items():
                data["debug"].setdefault(k, v)
        return data
    except (json.JSONDecodeError, OSError) as e:
        logger.error("Failed to load config: %s — using defaults", e)
        return dict(_DEFAULTS)


def save_config(data: dict[str, Any]) -> None:
    """Atomically write config to disk (write .tmp → rename)."""
    _CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    tmp = CONFIG_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2))
    os.replace(tmp, CONFIG_PATH)
    logger.debug("Config saved to %s", CONFIG_PATH)
