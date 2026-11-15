from __future__ import annotations

from neurowatch.models.mqtt import KEPT_SECRET, MqttSettings, MqttStatus, topics_help
from neurowatch.widgets.mqtt_panel import MqttPanel


class _FakeRecorder:
    def __init__(self) -> None:
        self.config: dict = {
            "enabled": False,
            "host": "",
            "port": 1883,
            "tls": False,
            "username": "",
            "password": "",
            "topic_prefix": "neurowatch",
            "discovery": True,
            "discovery_prefix": "homeassistant",
        }
        self.status: dict = {"state": "off"}
        self.calls: list[tuple[str, dict]] = []
        self.test_result: dict = {"ok": True}

    def __call__(self, method: str, params: dict, timeout: float) -> dict:
        self.calls.append((method, params))
        if method == "mqtt.set":
            saved = dict(params)
            if saved.get("password") and saved["password"] != KEPT_SECRET:
                saved["password"] = KEPT_SECRET
            self.config = saved
            self.status = {"state": "connected", "broker": f"{saved['host']}:{saved['port']}"}
        if method == "mqtt.test":
            return dict(self.test_result)
        if method == "mqtt.status":
            return dict(self.status)
        return {"config": dict(self.config), "status": dict(self.status)}


def test_the_panel_saves_a_broker_and_shows_the_connection(app, qtbot) -> None:
    recorder = _FakeRecorder()
    panel = MqttPanel(rpc=recorder)
    qtbot.addWidget(panel)
    qtbot.waitUntil(lambda: panel.save_button.isEnabled())
    assert panel.status_label.text().startswith("Off")

    panel.enabled_box.setChecked(True)
    panel.host_edit.setText("ha.local")
    panel.tls_box.setChecked(True)
    assert panel.port_box.value() == 8883, "TLS takes the usual TLS port"
    panel.username_edit.setText("nw")
    panel.password_edit.setText("secret")
    panel.save_button.click()
    qtbot.waitUntil(lambda: panel.status_label.text().startswith("Connected"))
    method, sent = recorder.calls[-1]
    assert method == "mqtt.set"
    assert sent["host"] == "ha.local" and sent["port"] == 8883 and sent["tls"] is True
    assert sent["password"] == "secret"
    assert panel.password_edit.text() == KEPT_SECRET, "the saved password is never shown"
    assert "ha.local:8883" in panel.status_label.text()


def test_a_missing_address_is_reported_without_calling_the_recorder(app, qtbot) -> None:
    recorder = _FakeRecorder()
    panel = MqttPanel(rpc=recorder)
    qtbot.addWidget(panel)
    qtbot.waitUntil(lambda: panel.save_button.isEnabled())
    calls = len(recorder.calls)
    panel.enabled_box.setChecked(True)
    panel.save_button.click()
    assert "address" in panel.error_label.text()
    assert len(recorder.calls) == calls


def test_a_failed_test_says_why(app, qtbot) -> None:
    recorder = _FakeRecorder()
    recorder.test_result = {"ok": False, "error": "The broker refused the user name or password"}
    panel = MqttPanel(rpc=recorder)
    qtbot.addWidget(panel)
    qtbot.waitUntil(lambda: panel.save_button.isEnabled())
    panel.host_edit.setText("ha.local")
    panel.test_button.click()
    qtbot.waitUntil(lambda: panel.status_label.text().startswith("Test failed"))
    assert "user name or password" in panel.status_label.text()
    assert recorder.calls[-1][0] == "mqtt.test"


def test_settings_and_status_read_what_the_recorder_sends() -> None:
    settings = MqttSettings.from_json({"enabled": True, "host": "h", "port": "bad"})
    assert settings.port == 1883 and settings.topic_prefix == "neurowatch"
    assert MqttSettings(enabled=True).problem() is not None
    assert MqttSettings(host="mqtt://h").problem() is not None
    assert MqttSettings(host="h", topic_prefix="a/#").problem() is not None
    assert MqttSettings(enabled=True, host="h").problem() is None
    error = MqttStatus.from_json(
        {"state": "error", "detail": "Could not reach", "broker": "h:1883"}
    )
    assert error.text == "Not connected to h:1883: Could not reach." and error.tone == "error"
    assert "site/events" in topics_help("site")
