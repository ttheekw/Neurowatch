from __future__ import annotations

from neurowatch.cloud.link_panel import CloudLinkPanel

CODE = (
    "6ba7b811-9dad-11d1-80b4-00c04fd430c8."
    "6ba7b810-9dad-11d1-80b4-00c04fd430c8."
    "KFPtAkVSlNOLCOT1Rdp634jsz4foAMaBcKzXXZBPZi0"
)


class _FakeBackend:
    def __init__(self, status: dict | None = None) -> None:
        self.result: dict = dict(
            status or {"phase": "unlinked", "cloud_endpoint": "http://127.0.0.1:8000"}
        )
        self.calls: list[tuple[str, dict, float]] = []

    def __call__(self, method: str, params: dict, timeout: float) -> dict:
        self.calls.append((method, params, timeout))
        if method in ("cloud.begin", "cloud.confirm"):
            self.result["phase"] = "linked"
        return dict(self.result)


def test_link_button_links_in_one_step_and_says_success(app, qtbot):
    backend = _FakeBackend()
    dialog = CloudLinkPanel(rpc=backend)
    qtbot.addWidget(dialog)
    qtbot.waitUntil(lambda: dialog.link_button.isEnabled())

    dialog.code_edit.setText(CODE)
    dialog.name_edit.setText("Front office")
    dialog.link_button.click()
    qtbot.waitUntil(lambda: dialog.unlink_button.isEnabled())
    method, params, _ = backend.calls[-1]
    assert (method, params) == (
        "cloud.begin",
        {"enrollment_code": CODE, "display_label": "Front office"},
    )
    assert [call[0] for call in backend.calls].count("cloud.confirm") == 0
    assert dialog.status_label.text().startswith("Success")
    assert not dialog.link_button.isEnabled()
    assert dialog.cloud_label.text() == "", "the cloud's address is not shown"


def test_link_button_finishes_a_link_that_stopped_after_the_claim(app, qtbot):
    backend = _FakeBackend({"phase": "awaiting_local_confirmation", "cloud_configured": True})
    dialog = CloudLinkPanel(rpc=backend)
    qtbot.addWidget(dialog)
    qtbot.waitUntil(lambda: dialog.link_button.isEnabled())
    assert not dialog.code_edit.isEnabled()

    dialog.link_button.click()
    qtbot.waitUntil(lambda: dialog.unlink_button.isEnabled())
    assert backend.calls[-1][:2] == ("cloud.confirm", {})
    assert dialog.status_label.text().startswith("Success")


def test_invalid_link_code_is_reported_without_calling_the_backend(app, qtbot):
    backend = _FakeBackend()
    dialog = CloudLinkPanel(rpc=backend)
    qtbot.addWidget(dialog)
    qtbot.waitUntil(lambda: dialog.link_button.isEnabled())
    calls_before = len(backend.calls)

    dialog.code_edit.setText("not a code")
    dialog.name_edit.setText("Front office")
    dialog.link_button.click()

    assert len(backend.calls) == calls_before
    assert dialog.error_label.text()


def test_dialog_explains_an_unconfigured_cloud_instead_of_offering_to_link(app, qtbot):
    backend = _FakeBackend({"phase": "unlinked", "cloud_configured": False, "cloud_endpoint": None})
    dialog = CloudLinkPanel(rpc=backend)
    qtbot.addWidget(dialog)

    qtbot.waitUntil(lambda: "not set up" in dialog.status_label.text())

    assert not dialog.link_button.isEnabled()
    assert not dialog.code_edit.isEnabled()
    assert "not configured" in dialog.cloud_label.text().lower()
    for text in (dialog.status_label.text(), dialog.cloud_label.text(), dialog.error_label.text()):
        assert "NW_CLOUD" not in text


def test_dialog_asks_for_a_new_enrollment_code_when_the_old_one_expired(app, qtbot):
    backend = _FakeBackend(
        {
            "phase": "awaiting_local_confirmation",
            "pending_expired": True,
            "error": "enrollment_expired",
            "cloud_configured": True,
            "cloud_endpoint": "https://cloud.example.com",
        }
    )
    dialog = CloudLinkPanel(rpc=backend)
    qtbot.addWidget(dialog)

    qtbot.waitUntil(lambda: "new enrollment code" in dialog.status_label.text())

    assert dialog.link_button.isEnabled()
    assert dialog.code_edit.isEnabled()
    assert "example.com" not in dialog.cloud_label.text()


def test_the_panel_uses_the_theme_and_names_where_to_find_the_code(app, qtbot):
    dialog = CloudLinkPanel(rpc=_FakeBackend())
    qtbot.addWidget(dialog)
    assert dialog.link_button.property("variant") == "primary"
    assert dialog.unlink_button.property("variant") == "danger"
    # Where to find the code, in the portal's own words.
    texts = " ".join(label.text() for label in dialog.findChildren(type(dialog.status_label)))
    assert "Recorders → Create enrollment code" in texts
