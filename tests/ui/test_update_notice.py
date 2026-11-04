"""Version comparison, bounded background checks and an accessible update notice."""
from __future__ import annotations

import json

import pytest
from PySide6.QtCore import Qt

from neurowatch.models.updates import newer_version, read_latest
from neurowatch.widgets import update_notice


@pytest.mark.parametrize(('current', 'versions', 'expected'), [
    ('0.1.9', ['v0.1.10', 'v0.1.8'], 'v0.1.10'),
    ('v1.2.3', ['v1.2.3', 'v1.2.2'], None),
    ('2.0.0', ['v1.9.99'], None),
    ('1.0.0', ['v1.1.0-rc.1', 'v1.0.0'], None),
    ('1.1.0-rc.1', ['v1.1.0'], 'v1.1.0'),
    ('1.1.0+build.2', ['v1.1.0'], None),
    ('d3adbeef', ['v9.0.0'], None),
    ('1.0.0', ['<b>v9.0.0</b>', 'v01.2.3', 'v2.0.0'], 'v2.0.0'),
])
def test_compares_release_versions(current, versions, expected):
    payload = {'versions': [{'version': version, 'prerelease': '-' in version} for version in versions]}
    assert newer_version(current, payload) == expected


def test_release_marked_prerelease_is_not_offered_as_stable():
    assert newer_version('1.0.0', {'versions': [{'version': 'v2.0.0', 'prerelease': True}]}) is None
    for payload in [None, [], {'versions': 'bad'}, {'versions': [None, 5]}, {'versions': []}]:
        assert newer_version('1.0.0', payload) is None


def test_network_read_is_bounded_and_timeout_is_set(monkeypatch):
    seen = []
    class Reply:
        def __enter__(self): return self
        def __exit__(self, *args): pass
        def read(self, size):
            seen.append(size)
            return json.dumps({'versions': [{'version': 'v1.2.3', 'prerelease': False}]}).encode()
    def open_request(request, *, timeout):
        assert request.full_url == 'https://neurowatch.app/v1/downloads?version=latest'
        assert timeout <= 10
        return Reply()
    monkeypatch.setattr('neurowatch.models.updates.urlopen', open_request)
    assert read_latest('1.0.0') == 'v1.2.3'
    assert seen == [256 * 1024 + 1]


def test_notice_is_hidden_until_newer_and_opens_matching_edition(qtbot, monkeypatch):
    opened = []
    monkeypatch.setattr(update_notice.QDesktopServices, 'openUrl', lambda url: opened.append(url.toString()))
    notice = update_notice.UpdateNotice()
    qtbot.addWidget(notice)
    assert notice.isHidden()
    notice.set_version('v1.2.3', edition='unlimited')
    notice.show()
    assert notice.text() == 'Update available: v1.2.3'
    assert 'Download' in notice.toolTip()
    assert notice.focusPolicy() == Qt.FocusPolicy.StrongFocus
    qtbot.keyClick(notice, Qt.Key.Key_Return)
    assert opened == ['https://neurowatch.app/downloads?version=v1.2.3&edition=unlimited']
    notice.set_version(None)
    assert notice.isHidden()


def test_controller_checks_on_start_hourly_and_keeps_notice_during_network_outage(qtbot):
    controller = update_notice.UpdateChecker('1.0.0', fetch=lambda current: 'v1.2.3')
    notice = controller.notice
    qtbot.addWidget(notice)
    assert controller.timer.interval() == 3_600_000
    controller.start()
    qtbot.waitUntil(lambda: notice.text() == 'Update available: v1.2.3')
    qtbot.waitUntil(lambda: controller.worker is None)
    controller.fetch = lambda current: (_ for _ in ()).throw(OSError('offline'))
    controller.check()
    qtbot.waitUntil(lambda: controller.worker is None)
    assert notice.text() == 'Update available: v1.2.3'
    controller.stop()
    assert not controller.timer.isActive()


def test_install_adds_status_notice_and_window_shutdown_cleanup(qtbot, monkeypatch):
    from types import SimpleNamespace
    from PySide6.QtWidgets import QWidget

    class Window(QWidget):
        def __init__(self):
            super().__init__()
            self.indicators = []
            self.callbacks = []
        def add_status_indicator(self, widget): self.indicators.append(widget)
        def add_shutdown_callback(self, callback): self.callbacks.append(callback)

    window = Window()
    qtbot.addWidget(window)
    monkeypatch.setattr(update_notice, 'build_identity', lambda: SimpleNamespace(label='1.0.0'))
    checker = update_notice.install(window)
    checker.fetch = lambda current: 'v1.0.1'
    qtbot.waitUntil(lambda: checker.notice.text() == 'Update available: v1.0.1')
    qtbot.waitUntil(lambda: checker.worker is None)
    assert window.indicators == [checker.notice]
    window.callbacks[0]()
    assert not checker.timer.isActive()
