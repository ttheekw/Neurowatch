"""Free (16-camera) and unlimited builds in the desktop UI."""

from __future__ import annotations

from types import SimpleNamespace

import neurowatch.main as main_mod
import neurowatch.widgets.edition_notice as notice_mod
from neurowatch.ipc.sync_client import IpcError
from neurowatch.models.edition import CAMERA_LIMIT_REACHED, Edition, limit_message
from neurowatch.widgets.edition_notice import EditionNotice

FREE_STATUS = {
    "edition": "free",
    "camera_limit": 16,
    "cameras_over_limit": [{"id": "a", "name": "Gate"}, {"id": "b", "name": "Yard"}],
    "downloads_url": "https://neurowatch.app/downloads",
}


def test_the_status_names_the_cameras_held_back() -> None:
    edition = Edition.from_status(FREE_STATUS)
    assert edition.camera_limit == 16
    assert edition.over_limit == ("Gate", "Yard")
    assert edition.notice == "2 cameras not running (free version: 16 cameras)"
    assert "Gate, Yard" in edition.notice_detail
    assert edition.unlimited_url == "https://neurowatch.app/downloads?edition=unlimited"


def test_an_unlimited_or_older_recorder_shows_no_notice() -> None:
    assert Edition.from_status({"edition": "unlimited", "camera_limit": None}).notice == ""
    assert Edition.from_status({"status": "running"}).notice == ""
    assert Edition.from_status(None).notice == ""
    odd = Edition.from_status({**FREE_STATUS, "downloads_url": "http://evil.example"})
    assert odd.downloads_url == "https://neurowatch.app/downloads"


def test_the_notice_shows_only_while_cameras_are_held_and_opens_the_downloads_page(
    qtbot, monkeypatch
) -> None:
    opened: list[str] = []
    monkeypatch.setattr(notice_mod, "open_unlimited", lambda e: opened.append(e.unlimited_url))
    notice = EditionNotice()
    qtbot.addWidget(notice)
    notice.set_edition(Edition.from_status(FREE_STATUS))
    assert not notice.isHidden()
    assert notice.text().startswith("2 cameras not running")
    notice.clicked.emit()
    assert opened == ["https://neurowatch.app/downloads?edition=unlimited"]
    notice.set_edition(Edition())
    assert notice.isHidden()


def test_a_refused_camera_offers_neurowatch_unlimited(monkeypatch) -> None:
    asked: list[str] = []
    opened: list[Edition] = []
    monkeypatch.setattr(main_mod, "ask", lambda _p, title, text, **_k: asked.append(text) or True)
    monkeypatch.setattr(main_mod, "open_unlimited", opened.append)
    window = SimpleNamespace(_edition=Edition.from_status(FREE_STATUS), _cameras=[])
    error = IpcError(CAMERA_LIMIT_REACHED, "limit", {"camera_limit": 16})

    main_mod.MainWindow._on_add_camera_result(window, error, None)  # type: ignore[arg-type]

    assert asked == [limit_message({"camera_limit": 16})]
    assert "up to 16 cameras" in asked[0]
    assert opened == [window._edition]
