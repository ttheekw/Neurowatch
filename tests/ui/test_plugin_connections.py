"""Plugin connection failures remain visible until recovery or deliberate disablement."""

from __future__ import annotations

from neurowatch.models.plugins import (
    PluginConnectionNotice,
    PluginsConfig,
    PluginServer,
    ServerStatus,
    connection_notices,
)


def test_connection_failure_retry_and_recovery() -> None:
    config = PluginsConfig(servers=[PluginServer("motion", "unix:/motion")])
    notices = connection_notices(config, {"motion": ServerStatus("motion", "connecting")})
    assert notices == []
    notices = connection_notices(config, {"motion": ServerStatus("motion", "connected")}, notices)
    assert notices == [PluginConnectionNotice("motion", "motion connected.", "success")]
    notices = connection_notices(
        config,
        {"motion": ServerStatus("motion", "error", "The plugin closed the session")},
        notices,
    )
    assert notices[0].tone == "error"
    assert "disconnected" in notices[0].text
    assert "The plugin closed the session" in notices[0].text
    retry = connection_notices(config, {"motion": ServerStatus("motion", "connecting")}, notices)
    assert retry == notices
    recovered = connection_notices(config, {"motion": ServerStatus("motion", "connected")}, retry)
    assert recovered == [PluginConnectionNotice("motion", "motion reconnected.", "success")]
    assert (
        connection_notices(config, {"motion": ServerStatus("motion", "connected")}, recovered)
        == recovered
    )


def test_initial_failure_disable_removal_and_approval() -> None:
    config = PluginsConfig(servers=[PluginServer("motion", "unix:/motion")])
    status = {"motion": ServerStatus("motion", "error", "Connection refused")}
    notices = connection_notices(config, status)
    assert "unavailable" in notices[0].text and "Connection refused" in notices[0].text
    assert connection_notices(config.with_server_enabled("motion", False), status, notices) == []
    assert connection_notices(config.without_server("motion"), status, notices) == []
    assert (
        connection_notices(config, {"motion": ServerStatus("motion", "pending_approval")}, notices)
        == []
    )


def test_a_retry_seen_between_polls_reports_connection_loss() -> None:
    config = PluginsConfig(servers=[PluginServer("motion", "unix:/motion")])
    connected = connection_notices(config, {"motion": ServerStatus("motion", "connected")})
    retry = connection_notices(config, {"motion": ServerStatus("motion", "authorizing")}, connected)
    assert retry[0].tone == "error" and "disconnected" in retry[0].text


def test_plugins_page_displays_connection_changes_and_status_read_failures(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import Snapshot
    from neurowatch.widgets.plugins_view import PluginsView

    view = PluginsView(None, rpc=lambda *_: None)
    qtbot.addWidget(view)
    view._on_loaded(
        Snapshot(
            PluginsConfig(servers=[PluginServer("motion", "unix:/motion")]),
            [],
            {"motion": ServerStatus("motion", "connected")},
        )
    )
    assert view.connection_messages["motion"].text() == "motion connected."
    view._on_status({"motion": ServerStatus("motion", "error", "Connection refused")})
    assert "Connection refused" in view.connection_messages["motion"].text()
    view._on_status(RuntimeError("Recorder offline"))
    assert "Recorder offline" in view.connection_problem.text()
    assert "Connection refused" in view.connection_messages["motion"].text()
    view._on_status({"motion": ServerStatus("motion", "connected")})
    assert view.connection_messages["motion"].text() == "motion reconnected."
    assert view.connection_problem.isHidden()
    view._on_loaded(
        Snapshot(
            view._snapshot.config.with_server_enabled("motion", False),
            [],
            {"motion": ServerStatus("motion", "connected")},
        )
    )
    assert view.connection_messages == {}


def test_many_plugin_messages_leave_room_for_the_selected_page(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import Snapshot
    from neurowatch.widgets.plugins_view import PluginsView

    view = PluginsView(None, rpc=lambda *_: None)
    qtbot.addWidget(view)
    servers = [PluginServer(f"plugin{i}", "unix:/test") for i in range(32)]
    view._on_loaded(
        Snapshot(
            PluginsConfig(servers=servers),
            [],
            {server.id: ServerStatus(server.id, "connected") for server in servers},
        )
    )
    view.resize(1100, 700)
    view.show()
    assert view.height() <= 700
    assert view.pages.height() >= 300
