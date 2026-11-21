"""Settings → Video Wall → Video decoding (``video.decoder``)."""

from __future__ import annotations

from typing import Any

from neurowatch.dialogs import settings_dialog
from neurowatch.dialogs.settings_dialog import SettingsDialog
from neurowatch.models.video_decoder import (
    AccelerationNotice,
    DecoderStatus,
    acceleration_notice,
    decoder_choices,
    decoder_from_config,
    restart_reply,
)

_STATUS = {
    "status": "running",
    "video_acceleration": {
        "kind": "nvdec",
        "label": "NVDEC",
        "hardware": True,
        "available": True,
        "decoder_preference": "auto",
        "available_decoders": [
            {"id": "nvdec", "label": "NVIDIA NVDEC"},
            {"id": "d3d11", "label": "Direct3D 11"},
            {"id": "bogus", "label": "Not a decoder"},
        ],
    },
}

_DRIVER_MISSING = {
    "kind": "software",
    "label": "Software (avdec)",
    "hardware": False,
    "available": True,
    "reason": (
        "This PC has Intel graphics that can decode video, but its VA-API driver (iHD)"
        " isn't installed, so video is decoded on the CPU."
    ),
    "reason_code": "VAAPI_DRIVER_MISSING",
    "cause": "driver_not_installed",
    "gpu": {
        "vendor": "Intel",
        "kernel_driver": "i915",
        "render_node": "/dev/dri/renderD128",
        "va_driver": "iHD",
    },
    "suggested_packages": {
        "deb": ["intel-media-va-driver"],
        "rpm": ["libva-intel-media-driver"],
        "arch": ["intel-media-driver"],
    },
}


def _dialog(qtbot, monkeypatch, config: dict | None = None, **kwargs: Any) -> SettingsDialog:
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    monkeypatch.setattr(SettingsDialog, "_read_archive_status", lambda self: None)
    monkeypatch.setattr(SettingsDialog, "_read_decoder_status", lambda self: None)
    dlg = SettingsDialog(config or {}, **kwargs)
    qtbot.addWidget(dlg)
    return dlg


def _items(dlg: SettingsDialog) -> list[tuple[str, str]]:
    combo = dlg._decoder
    return [(combo.itemData(i), combo.itemText(i)) for i in range(combo.count())]


def test_the_status_lists_the_decoders_found() -> None:
    status = DecoderStatus.from_status(_STATUS)
    assert status.available == (("nvdec", "NVIDIA NVDEC"), ("d3d11", "Direct3D 11"))
    assert status.active == "NVIDIA NVDEC" and status.warning == ""
    # An older recorder, or none: nothing found, nothing in use known.
    assert DecoderStatus.from_status({"video_acceleration": {"kind": "vaapi"}}).available == ()
    assert DecoderStatus.from_status(None) == DecoderStatus()
    cpu = DecoderStatus.from_status(
        {
            "video_acceleration": {
                "kind": "software",
                "decoder_preference_warning": "NVIDIA NVDEC is not available on this computer.",
            }
        }
    )
    assert cpu.active == "the CPU (software)" and "not available" in cpu.warning


def test_choices_are_automatic_the_ones_found_and_cpu_only() -> None:
    found = [("vaapi", "VA-API")]
    assert decoder_choices("auto", found) == [
        ("auto", "Automatic (recommended)"),
        ("vaapi", "VA-API"),
        ("software", "CPU only (software)"),
    ]
    # A configured decoder this computer lacks stays selectable, marked.
    assert ("nvdec", "NVIDIA NVDEC (not available)") in decoder_choices("nvdec", found)
    assert decoder_from_config({"video": {"decoder": "d3d12"}}) == "d3d12"
    assert decoder_from_config({"video": {"decoder": "cuda"}}) == "auto"
    assert decoder_from_config({}) == "auto"
    assert restart_reply({"ok": True, "restarting": True})
    assert not restart_reply({"ok": True}) and not restart_reply(None)


def test_the_video_wall_page_offers_the_decoders_and_saves_the_choice(qtbot, monkeypatch) -> None:
    dlg = _dialog(qtbot, monkeypatch, {"video": {"decoder": "auto"}})
    assert dlg._stack.widget(1).isAncestorOf(dlg._decoder), "on the Video Wall page"
    assert [value for value, _ in _items(dlg)] == ["auto", "software"], "before the status"
    assert dlg.get_config()["video"] == {"decoder": "auto"}
    assert not dlg.decoder_changed()

    dlg._on_decoder_status(DecoderStatus.from_status(_STATUS))
    assert _items(dlg) == [
        ("auto", "Automatic (recommended)"),
        ("nvdec", "NVIDIA NVDEC"),
        ("d3d11", "Direct3D 11"),
        ("software", "CPU only (software)"),
    ]
    assert dlg._decoder.currentData() == "auto"
    assert dlg._decoder_now.text() == "Now decoding with: NVIDIA NVDEC"
    assert dlg._decoder_warning.isHidden()

    dlg._decoder.setCurrentIndex(dlg._decoder.findData("d3d11"))
    assert dlg.decoder_changed()
    config = dlg.get_config()
    assert config["video"] == {"decoder": "d3d11"}

    # The status coming in again keeps what the user picked.
    dlg._on_decoder_status(DecoderStatus.from_status(_STATUS))
    assert dlg.video_decoder() == "d3d11"


def test_a_chosen_decoder_that_is_missing_is_kept_and_explained(qtbot, monkeypatch) -> None:
    dlg = _dialog(qtbot, monkeypatch, {"video": {"decoder": "qsv"}})
    status = DecoderStatus(
        available=(("vaapi", "VA-API"),),
        active="VA-API",
        warning="Intel Quick Sync is not available on this computer.",
    )
    dlg._on_decoder_status(status)
    assert dlg.video_decoder() == "qsv", "never changed behind the user's back"
    assert ("qsv", "Intel Quick Sync (not available)") in _items(dlg)
    assert not dlg._decoder_warning.isHidden()
    assert "not available" in dlg._decoder_warning.text()


def test_the_status_is_read_off_the_ui_thread(monkeypatch) -> None:
    calls: list[str] = []

    class Client:
        def __init__(self, endpoint: str | None = None) -> None:
            pass

        def call(self, method: str, params: Any = None) -> Any:
            calls.append(method)
            return _STATUS

    monkeypatch.setattr(settings_dialog, "SyncIpcClient", Client)
    worker = settings_dialog._DecoderStatusWorker("unix:/tmp/nw.sock")
    results: list[object] = []
    worker.done.connect(results.append)
    worker.run()
    assert calls == ["system.status"]
    assert isinstance(results[0], DecoderStatus) and results[0].available


def test_saving_a_new_decoder_rides_out_the_restart(monkeypatch) -> None:
    from neurowatch import main

    calls: list[str] = []

    class Client:
        def __init__(self, endpoint: str | None = None) -> None:
            pass

        def call(self, method: str, params: Any = None) -> Any:
            calls.append(method)
            if method == "config.set":
                return {"ok": True, "restarting": True}
            raise AssertionError("the restarting recorder is not asked again")

    monkeypatch.setattr(main, "SyncIpcClient", Client)
    sent = {"site_name": "A", "video": {"decoder": "software"}}
    worker = main._SettingsSaveWorker("unix:/tmp/nw.sock", sent)
    results: list[object] = []
    worker.result.connect(results.append)
    worker.run()
    assert calls == ["config.set"]
    assert results == [sent], "what was sent is what the recorder saved"


def test_a_missing_driver_is_told_with_the_card_and_the_packages() -> None:
    notice = acceleration_notice(_DRIVER_MISSING)
    assert notice == AccelerationNotice(
        code="VAAPI_DRIVER_MISSING",
        text=_DRIVER_MISSING["reason"],
        vendor="Intel",
        kernel_driver="i915",
        render_node="/dev/dri/renderD128",
        packages=(
            ("Debian/Ubuntu", ("intel-media-va-driver",)),
            ("Fedora", ("libva-intel-media-driver",)),
            ("Arch", ("intel-media-driver",)),
        ),
    )
    assert notice.detail == (
        "Debian/Ubuntu: intel-media-va-driver\n"
        "Fedora: libva-intel-media-driver\n"
        "Arch: intel-media-driver"
    )
    assert notice.card == "Intel (i915, /dev/dri/renderD128)"
    assert notice.short == "Intel GPU driver missing"
    # Nothing to tell: another reason, no reason, an older recorder.
    pending = {"kind": "software", "reason": "x", "reason_code": "GPU_NOT_ACCESSIBLE"}
    assert acceleration_notice(pending) is None
    assert acceleration_notice({"kind": "software", "reason": "x"}) is None
    assert acceleration_notice({"reason_code": "VAAPI_DRIVER_MISSING"}) is None, "no reason"
    assert acceleration_notice(None) is None
    bare = acceleration_notice({"reason": "y", "reason_code": "NVDEC_LIBRARY_MISSING"})
    assert bare is not None and (bare.detail, bare.card, bare.short) == (
        "",
        "Unknown",
        "GPU driver missing",
    )


def test_settings_warns_that_the_graphics_card_has_no_driver(qtbot, monkeypatch) -> None:
    status = DecoderStatus.from_status({"video_acceleration": _DRIVER_MISSING})
    assert status.active == "the CPU (software)"
    assert status.warning == _DRIVER_MISSING["reason"]
    dlg = _dialog(qtbot, monkeypatch)
    dlg._on_decoder_status(status)
    assert dlg._decoder_warning.text() == _DRIVER_MISSING["reason"]
    assert not dlg._decoder_warning.isHidden()
