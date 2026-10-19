"""The camera dialog's Camera tab: streams found over ONVIF or entered as
URLs, saved by the chosen method whichever tab is showing, and an edit that
changes nothing keeps the camera working."""

from __future__ import annotations

import base64

from neurowatch.dialogs.camera_dialog import AddEditCameraDialog
from neurowatch.models.camera import CameraConfig

SECRET_B64 = base64.b64encode(b"secret").decode()

STREAMS = {
    "streams": [
        {
            "stream_id": "s1",
            "name": "Main",
            "width": 1920,
            "height": 1080,
            "url": "rtsp://10.0.0.5/main",
        },
        {
            "stream_id": "s2",
            "name": "Sub",
            "width": 640,
            "height": 360,
            "url": "rtsp://10.0.0.5/sub",
        },
        {
            "stream_id": "s3",
            "name": "Third",
            "width": 1280,
            "height": 720,
            "url": "rtsp://10.0.0.5/third",
        },
    ],
    "main_stream_id": "s1",
    "sub_stream_id": "s2",
}


def _searched_camera() -> CameraConfig:
    """As a network search adds it: credentials in fields, URLs without them."""
    return CameraConfig.from_dict(
        {
            "id": "c1",
            "name": "Gate",
            "url": "rtsp://10.0.0.5/main",
            "sub_url": "rtsp://10.0.0.5/sub",
            "username": "admin",
            "password_b64": SECRET_B64,
            "streams": [
                {
                    "id": "1",
                    "url": "rtsp://10.0.0.5/main",
                    "name": "Main",
                    "width": 1920,
                    "height": 1080,
                },
                {
                    "id": "2",
                    "url": "rtsp://10.0.0.5/sub",
                    "name": "Sub",
                    "width": 640,
                    "height": 360,
                },
            ],
            "discovery": {"endpoint_reference": "urn:uuid:gate", "source": "p1"},
            "onvif": {"device_url": "http://10.0.0.5:8080/onvif/device_service"},
        }
    )


def test_saving_an_edit_unchanged_keeps_the_camera_working(qtbot):
    cam = _searched_camera()
    dlg = AddEditCameraDialog(camera=cam)
    qtbot.addWidget(dlg)
    # It opens on its saved streams, without asking the camera again.
    assert dlg._tabs.currentWidget() is dlg._camera_tab
    assert dlg._mode() == "onvif"
    assert dlg._s_main_combo.currentText().startswith("Main")
    assert dlg._s_sub_combo.currentText().startswith("Sub")
    assert dlg._s_port.value() == 8080
    assert dlg._pass.text() == ""  # never shown

    dlg._on_accept()
    assert dlg.result() == dlg.DialogCode.Accepted
    out = dlg.get_config()
    assert out.url == cam.url
    assert out.sub_url == cam.sub_url
    assert out.password_b64 == SECRET_B64
    assert out.username == "admin"
    assert out.discovery == cam.discovery
    assert [s.url for s in out.streams] == [s.url for s in cam.streams]


def test_an_edit_can_pick_another_saved_stream_without_loading(qtbot):
    dlg = AddEditCameraDialog(camera=_searched_camera())
    qtbot.addWidget(dlg)
    dlg._s_sub_combo.setCurrentIndex(0)  # same as main
    assert dlg.get_config().sub_url == ""


def test_saving_from_another_tab_uses_the_streams_chosen(qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._show_camera_tab("onvif")
    dlg._s_host.setText("10.0.0.5")
    dlg._user.setText("admin")
    dlg._pass.setText("secret")
    dlg._on_discover_done(STREAMS)
    dlg._tabs.setCurrentIndex(dlg._tabs.count() - 1)  # Motion

    dlg._on_accept()
    assert dlg.result() == dlg.DialogCode.Accepted
    cam = dlg.get_config()
    assert (cam.url, cam.sub_url) == ("rtsp://10.0.0.5/main", "rtsp://10.0.0.5/sub")
    assert cam.name == "10.0.0.5"
    assert cam.password_b64 == SECRET_B64


def test_without_streams_save_says_what_is_missing_on_the_camera_tab(qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._show_camera_tab("onvif")
    dlg._tabs.setCurrentIndex(dlg._tabs.count() - 2)  # Recording

    dlg._on_accept()
    assert dlg.result() != dlg.DialogCode.Accepted
    assert dlg._tabs.currentWidget() is dlg._camera_tab
    assert dlg._discover_status.property("tone") == "warning"
    assert "Load the camera's streams" in dlg._discover_status.text()

    dlg._set_mode("urls")
    dlg._on_accept()
    assert dlg._status_label.property("tone") == "warning"
    assert "main stream URL" in dlg._status_label.text()


def test_reloading_streams_keeps_the_streams_chosen(qtbot):
    dlg = AddEditCameraDialog(camera=_searched_camera())
    qtbot.addWidget(dlg)
    dlg._s_main_combo.setCurrentIndex(1)  # Sub as main, for this test
    main, _ = dlg._chosen_streams()
    dlg._keep_choice = (main.url if main else None, None)
    dlg._on_discover_done(STREAMS)
    assert dlg.get_config().url == "rtsp://10.0.0.5/sub"
    assert len(dlg._discovered) == 3


def test_a_failed_reload_leaves_the_saved_streams(qtbot):
    dlg = AddEditCameraDialog(camera=_searched_camera())
    qtbot.addWidget(dlg)
    dlg._keep_choice = ("rtsp://10.0.0.5/main", "rtsp://10.0.0.5/sub")
    dlg._on_discover_done(RuntimeError("timed out"))
    assert dlg._discover_status.property("tone") == "error"
    assert dlg.get_config().url == "rtsp://10.0.0.5/main"


def test_credentials_in_a_url_become_fields_and_leave_the_url(qtbot):
    cam = CameraConfig.from_dict(
        {"id": "c2", "name": "Old", "url": "rtsp://bob:p%40ss@10.0.0.9:554/live"}
    )
    dlg = AddEditCameraDialog(camera=cam)
    qtbot.addWidget(dlg)
    assert dlg._mode() == "urls"
    assert dlg._a_url.text() == "rtsp://10.0.0.9:554/live"
    assert (dlg._user.text(), dlg._pass.text()) == ("bob", "p@ss")
    out = dlg.get_config()
    assert out.url == "rtsp://10.0.0.9:554/live"
    assert out.username == "bob"
    assert base64.b64decode(out.password_b64).decode() == "p@ss"


def test_a_new_password_replaces_the_saved_one_and_no_username_means_none(qtbot):
    dlg = AddEditCameraDialog(camera=_searched_camera())
    qtbot.addWidget(dlg)
    dlg._pass.setText("changed")
    assert base64.b64decode(dlg.get_config().password_b64).decode() == "changed"
    dlg._pass.clear()
    dlg._user.clear()
    out = dlg.get_config()
    assert (out.username, out.password_b64) == ("", "")


def test_switching_to_urls_starts_from_the_streams_found(qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._show_camera_tab("onvif")
    dlg._on_discover_done(STREAMS)
    dlg._set_mode("urls")
    assert dlg._a_url.text() == "rtsp://10.0.0.5/main"
    assert dlg._a_sub_url.text() == "rtsp://10.0.0.5/sub"
    assert dlg._a_more_streams.toPlainText() == "rtsp://10.0.0.5/third"
    dlg._set_mode("onvif")
    assert dlg._s_host.text() == "10.0.0.5"


def test_the_connection_test_uses_the_saved_password_and_a_clean_url(qtbot):
    dlg = AddEditCameraDialog(camera=_searched_camera())
    qtbot.addWidget(dlg)
    url, user, password_b64 = dlg._effective_url_and_creds()
    assert (url, user, password_b64) == ("rtsp://10.0.0.5/main", "admin", SECRET_B64)


def test_the_onvif_address_and_port_typed_are_saved_for_camera_events(qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._show_camera_tab("onvif")
    dlg._s_host.setText("10.0.0.5")
    dlg._s_port.setValue(18680)
    dlg._on_discover_done(STREAMS)
    assert dlg.get_config().onvif == {"device_url": "http://10.0.0.5:18680/onvif/device_service"}

    # Editing keeps what the backend found; a camera that moved drops it.
    edited = AddEditCameraDialog(camera=_searched_camera())
    qtbot.addWidget(edited)
    assert edited.get_config().onvif == {"device_url": "http://10.0.0.5:8080/onvif/device_service"}
    edited._set_mode("urls")
    edited._a_url.setText("rtsp://10.0.0.99/main")
    assert edited.get_config().onvif is None
