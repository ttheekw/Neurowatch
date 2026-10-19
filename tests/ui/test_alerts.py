from __future__ import annotations

SETTINGS = {
    "channels": [
        {"id": "phone", "name": "My phone", "kind": "ntfy", "enabled": True,
         "attach_picture": True, "server": "https://ntfy.sh", "topic": "gate", "token": "••••••••"},
        {"id": "desk", "name": "This computer", "kind": "desktop", "enabled": True,
         "attach_picture": False},
    ],
    "rules": [
        {"id": "night", "name": "Night motion", "enabled": True, "kinds": ["motion", "tamper"],
         "cameras": ["cam-1"], "cooldown_sec": 300, "channels": ["phone"],
         "schedule": [{"days": [0, 1, 2, 3, 4, 5, 6], "start": "22:00", "end": "06:00"}]}
    ],
}


def test_settings_round_trip_keeping_the_secret_marker():
    from neurowatch.models.alerts import AlertSettings

    settings = AlertSettings.from_dict(SETTINGS)
    assert settings.to_dict() == SETTINGS


def test_summaries_ids_and_problems_read_naturally():
    from neurowatch.models.alerts import (
        AlertSettings,
        Channel,
        Rule,
        channel_problem,
        channel_summary,
        new_id,
        rule_problem,
        rule_summary,
    )

    settings = AlertSettings.from_dict(SETTINGS)
    assert channel_summary(settings.channels[0]) == "ntfy push · ntfy.sh/gate · with picture"
    assert rule_summary(settings.rules[0], {"cam-1": "Gate"}, {"phone": "My phone"}) == (
        "Motion, Camera tampering · Gate · 22:00–06:00 daily · at most every 5 min → My phone"
    )
    assert new_id("My Phone!", {"my-phone"}) == "my-phone-2"
    assert channel_problem(Channel("x", "", "ntfy")) == "Give the channel a name."
    assert channel_problem(Channel("x", "Hook", "webhook", fields={"url": "ftp://x"})) is not None
    assert channel_problem(Channel("x", "Bot", "telegram", fields={"bot_token": "1:a"})) == (
        "Enter the chat id."
    )
    assert rule_problem(Rule("r", "R", [], ["phone"])) == "Choose at least one kind of event."
    assert rule_problem(Rule("r", "R", ["motion"], [])) == "Choose at least one channel to send to."
    orphaned = settings.remove_channel("phone")
    assert orphaned == ["Night motion"] and settings.rules[0].channels == []


def test_the_desktop_feed_starts_from_now_and_follows_a_restarted_recorder(qtbot):
    from neurowatch.ipc.alert_feed import AlertFeedWorker

    replies = [
        {"seq": 4, "alerts": []},
        {"seq": 5, "alerts": [{"seq": 5, "title": "Motion · Gate"}]},
        None,  # recorder down
        {"seq": 1, "alerts": []},  # restarted: numbered from zero again
        {"seq": 2, "alerts": [{"seq": 2, "title": "Tamper · Gate"}]},
    ]
    asked = []

    def call(method, params):
        asked.append((method, params))
        return replies.pop(0)

    feed = AlertFeedWorker(call=call)
    assert feed.poll_once() == [], "nothing older than now is shown"
    assert [a["title"] for a in feed.poll_once()] == ["Motion · Gate"]
    assert feed.poll_once() == []
    assert feed.poll_once() == []
    assert [a["title"] for a in feed.poll_once()] == ["Tamper · Gate"]
    assert asked[0] == ("alerts.desktop", {}) and asked[1] == ("alerts.desktop", {"after": 4})
    assert asked[3] == ("alerts.desktop", {"after": 5}) and asked[4][1] == {"after": 1}


def test_toasts_stack_three_newest_first_and_open_on_click(qtbot):
    from PySide6.QtCore import QPoint, Qt
    from PySide6.QtWidgets import QWidget

    from neurowatch.widgets.alert_toasts import AlertToasts

    host = QWidget()
    qtbot.addWidget(host)
    host.resize(900, 600)
    host.show()
    toasts = AlertToasts(host)
    for n in range(4):
        toasts.show_alert({"title": f"Alert {n}", "message": "Motion on Gate"})
    assert [t.title.text() for t in toasts.shown] == ["Alert 3", "Alert 2", "Alert 1"]
    first = toasts.shown[0]
    assert first.x() + first.width() <= host.width() and first.y() < toasts.shown[1].y()
    with qtbot.waitSignal(toasts.opened) as opened:
        qtbot.mouseClick(first, Qt.MouseButton.LeftButton, pos=QPoint(20, 40))
    assert opened.args[0]["title"] == "Alert 3"
    assert len(toasts.shown) == 2


def test_rule_editor_reads_cameras_hours_channels_and_quiet_time(qtbot):
    from neurowatch.dialogs.alert_editors import RuleDialog
    from neurowatch.models.alerts import AlertSettings, Rule

    settings = AlertSettings.from_dict(SETTINGS)
    dialog = RuleDialog(
        Rule("new", "", ["motion"], []), [("cam-1", "Gate"), ("cam-2", "Yard")], settings.channels
    )
    qtbot.addWidget(dialog)
    dialog._name.setText("Yard at night")
    dialog._kinds["tamper"].setChecked(True)
    dialog._some_cameras.setChecked(True)
    dialog._cameras["cam-2"].setChecked(True)
    dialog._hours.setChecked(True)
    dialog._channels["desk"].setChecked(True)
    dialog._quiet.setCurrentIndex(dialog._quiet.findData(900))
    rule = dialog.read()
    assert rule.kinds == ["motion", "tamper"] and rule.cameras == ["cam-2"]
    assert [(w.start, w.end, len(w.days)) for w in rule.schedule] == [("22:00", "06:00", 7)]
    assert rule.channels == ["desk"] and rule.cooldown_sec == 900
    dialog._channels["desk"].setChecked(False)
    dialog._accept()
    assert dialog._error.isVisibleTo(dialog) and "channel" in dialog._error.text()


def test_channel_editor_keeps_a_saved_secret_and_tests_off_the_ui_thread(qtbot, monkeypatch):
    from neurowatch.dialogs import alert_editors
    from neurowatch.models.alerts import AlertSettings

    sent = []

    class Client:
        def __init__(self, endpoint):
            self.endpoint = endpoint

        def call(self, method, params):
            sent.append((method, params))
            return {"ok": False, "error": "HTTP 401: unauthorized"}

    monkeypatch.setattr(alert_editors, "SyncIpcClient", Client)
    channel = AlertSettings.from_dict(SETTINGS).channels[0]
    dialog = alert_editors.ChannelDialog(None, channel)
    qtbot.addWidget(dialog)
    assert "leave as is" in dialog._fields["token"].placeholderText()
    dialog._send_test()
    qtbot.waitUntil(lambda: "Not sent" in dialog._status.text())
    assert "HTTP 401" in dialog._status.text()
    assert sent[0][0] == "alerts.test" and sent[0][1]["channel"]["token"] == "••••••••"


def test_alerts_dialog_loads_saves_and_lists_what_was_sent(qtbot, monkeypatch):
    from neurowatch.dialogs import alerts_dialog

    calls = []

    class Client:
        def __init__(self, endpoint):
            pass

        def call(self, method, params):
            calls.append((method, params))
            if method == "alerts.history":
                return {"deliveries": [{"at_ms": 0, "channel_name": "My phone", "title": "Motion · Gate",
                                        "ok": False, "error": "timed out", "test": False}]}
            return SETTINGS if method == "alerts.get" else params

    monkeypatch.setattr(alerts_dialog, "SyncIpcClient", Client)
    dialog = alerts_dialog.AlertsDialog(None, [("cam-1", "Gate")])
    qtbot.addWidget(dialog)
    qtbot.waitUntil(lambda: dialog._rules.count() == 1)
    assert "Night motion" in dialog._rules.item(0).text()
    assert dialog._channels.count() == 2
    assert "Not sent: timed out" in dialog._history.item(0).text()
    assert not dialog.dirty
    dialog._rules.item(0).setCheckState(alerts_dialog.Qt.CheckState.Unchecked)
    assert dialog.dirty
    dialog._save_settings()
    qtbot.waitUntil(lambda: "Saved" in dialog._status.text())
    method, params = calls[-1]
    assert method == "alerts.set" and params["rules"][0]["enabled"] is False
    assert params["channels"][0]["token"] == "••••••••", "the stored secret is kept"
    assert not dialog.dirty
