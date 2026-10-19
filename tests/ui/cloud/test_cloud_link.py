from __future__ import annotations

import pytest

from neurowatch.cloud.cloud_link import (
    CloudLinkStatus,
    begin_params,
    operator_error,
    parse_enrollment_code,
)

TENANT = "6ba7b811-9dad-11d1-80b4-00c04fd430c8"
ENROLLMENT = "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
PROOF = "KFPtAkVSlNOLCOT1Rdp634jsz4foAMaBcKzXXZBPZi0"
CODE = f"{TENANT}.{ENROLLMENT}.{PROOF}"


def test_enrollment_code_from_the_web_portal_becomes_cloud_begin_params():
    code = parse_enrollment_code(f"  {CODE}\n")
    assert begin_params(code, "  Front office  ") == {
        "enrollment_code": CODE,
        "display_label": "Front office",
    }


@pytest.mark.parametrize(
    "text",
    [
        "",
        "not a code",
        f"{TENANT}.{ENROLLMENT}",
        f"{CODE}.extra",
        f"{TENANT.upper()}.{ENROLLMENT}.{PROOF}",
        f"{TENANT}.not-a-uuid.{PROOF}",
        f"{TENANT}.{ENROLLMENT}.",
        f"{TENANT}.{ENROLLMENT}.proof value",
        '{"endpoint": "http://127.0.0.1:5173"}',
    ],
)
def test_malformed_enrollment_codes_are_rejected(text):
    with pytest.raises(ValueError):
        parse_enrollment_code(text)


@pytest.mark.parametrize("label", ["", "   ", "x" * 65, "cam:1", "line\nbreak"])
def test_node_names_the_backend_would_refuse_are_rejected_early(label):
    with pytest.raises(ValueError):
        begin_params(parse_enrollment_code(CODE), label)


def test_status_describes_each_backend_link_phase_and_the_cloud_url():
    unlinked = CloudLinkStatus.from_result(
        {"phase": "unlinked", "cloud_endpoint": "http://127.0.0.1:8000"}
    )
    assert (unlinked.can_begin, unlinked.can_confirm, unlinked.can_unlink) == (True, False, False)
    assert unlinked.cloud_endpoint == "http://127.0.0.1:8000"

    waiting = CloudLinkStatus.from_result(
        {"phase": "awaiting_local_confirmation", "tenant_id": TENANT}
    )
    assert (waiting.can_begin, waiting.can_confirm, waiting.can_unlink) == (False, True, False)

    linked = CloudLinkStatus.from_result(
        {"phase": "linked", "node_id": "n", "last_check_in": "2026-09-20T12:06:00Z"}
    )
    assert (linked.can_begin, linked.can_confirm, linked.can_unlink) == (False, False, True)
    assert "Linked" in linked.summary and "2026-09-20T12:06:00Z" in linked.summary


def test_unknown_phase_offers_no_actions():
    status = CloudLinkStatus.from_result({"phase": "something_new"})
    assert (status.can_begin, status.can_confirm, status.can_unlink) == (False, False, False)


def test_a_cloud_that_is_not_configured_is_its_own_state_with_a_plain_explanation():
    status = CloudLinkStatus.from_result(
        {"phase": "unlinked", "cloud_configured": False, "cloud_endpoint": None}
    )

    assert status.cloud_configured is False
    assert (status.can_begin, status.can_confirm, status.can_unlink) == (False, False, False)
    assert "not set up" in status.summary
    assert "not configured" in status.cloud_description.lower()
    for text in (status.summary, status.cloud_description):
        assert "NW_CLOUD" not in text
        assert "environment" not in text.lower()


def test_an_older_backend_without_the_configured_flag_is_read_from_its_endpoint():
    status = CloudLinkStatus.from_result(
        {"phase": "unlinked", "cloud_endpoint": "https://cloud.example.com"}
    )

    assert status.cloud_configured is True
    assert status.can_begin is True
    assert status.cloud_description == "", "the cloud's address is not shown"


def test_an_expired_enrollment_asks_for_a_new_code_instead_of_finishing_it():
    status = CloudLinkStatus.from_result(
        {
            "phase": "awaiting_local_confirmation",
            "pending_expired": True,
            "error": "enrollment_expired",
            "cloud_configured": True,
            "cloud_endpoint": "https://cloud.example.com",
        }
    )

    assert status.pending_expired is True
    assert (status.can_begin, status.can_confirm, status.can_unlink) == (True, False, False)
    assert "new enrollment code" in status.summary


@pytest.mark.parametrize(
    "message",
    [
        "unsafe or invalid cloud configuration: the local development cloud URL "
        "http://127.0.0.1:8000 needs the backend started with "
        "NW_CLOUD_DEV_ALLOW_LOOPBACK_HTTP=1",
        "cloud linking is not configured; set NW_CLOUD_ENDPOINT",
    ],
)
def test_backend_errors_never_show_internal_environment_variable_instructions(message):
    text = operator_error(message)

    assert "NW_CLOUD" not in text
    assert "not set up" in text


def test_ordinary_backend_errors_are_shown_unchanged():
    assert operator_error("cloud request failed: HTTP 503") == "cloud request failed: HTTP 503"


def test_a_refused_enrollment_code_tells_the_operator_what_to_do() -> None:
    text = operator_error("cloud.begin failed: cloud request failed: HTTP 404 Not Found")
    assert "HTTP" not in text
    assert "new enrollment code" in text
