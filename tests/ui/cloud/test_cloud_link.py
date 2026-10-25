from __future__ import annotations

from datetime import timedelta, timezone

import pytest

from neurowatch.cloud.cloud_link import (
    CloudLinkStatus,
    begin_params,
    clock_warning,
    local_time,
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
    assert "Linked" in linked.summary
    # Shown in this computer's time, not as the raw UTC string.
    assert local_time("2026-09-20T12:06:00Z") in linked.summary
    assert "2026-09-20T12:06:00Z" not in linked.summary


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


COLOMBO = timezone(timedelta(hours=5, minutes=30), "+0530")
TOKYO_NAMED = timezone(timedelta(hours=9), "JST")
WINDOWS_NAMED = timezone(timedelta(hours=5, minutes=30), "Sri Lanka Standard Time")


def test_cloud_times_are_shown_in_local_time_with_the_zone():
    assert local_time("2026-10-25T12:06:00Z", local_zone=TOKYO_NAMED) == "2026-10-25 21:06 JST"
    assert local_time("2026-10-25T12:06:00+00:00", local_zone=COLOMBO) == "2026-10-25 17:36 +0530"
    # Windows' long zone names become an offset.
    assert (
        local_time("2026-10-25T23:50:00Z", local_zone=WINDOWS_NAMED) == "2026-10-26 05:20 UTC+05:30"
    )


def test_text_that_is_not_a_cloud_time_is_shown_unchanged():
    assert local_time("now") == "now"
    assert local_time("2026-10-25T12:06:00") == "2026-10-25T12:06:00"
    assert local_time(None) == ""


def test_the_clock_warning_and_offset_are_read_from_the_status():
    warning = "This computer's clock is 5 h 30 min ahead of NeuroWatch Cloud."
    status = CloudLinkStatus.from_result(
        {"phase": "linked", "clock_offset_sec": -19800, "clock_warning": warning}
    )
    assert status.clock_warning == warning
    assert status.clock_offset_sec == -19800
    assert clock_warning({"clock_warning": warning}) == warning
    assert clock_warning({"clock_warning": ""}) is None
    assert clock_warning(None) is None
    older = CloudLinkStatus.from_result({"phase": "linked", "clock_offset_sec": True})
    assert (older.clock_warning, older.clock_offset_sec) == (None, None)


@pytest.mark.parametrize(
    "message",
    [
        "cloud.begin failed: invalid params: unsafe or invalid cloud configuration: the "
        "enrollment code has expired (codes last five minutes): create a new one",
        # Backends before the cloud clock said only this.
        "cloud.begin failed: unsafe or invalid cloud configuration: cloud timestamp has expired",
    ],
)
def test_an_expired_code_error_mentions_the_clock_and_time_zone(message):
    text = operator_error(message)
    assert "expired" in text
    assert "time zone" in text
    assert "invalid cloud configuration" not in text
