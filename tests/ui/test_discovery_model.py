"""Camera search results as the desktop shows them (no Qt)."""

from __future__ import annotations

from neurowatch.models.discovery import (
    Candidate,
    StreamChoice,
    check_range,
    discovered_streams,
)


def _candidate(**over: object) -> Candidate:
    raw = {
        "candidate_id": "c1",
        "address": "192.168.1.64",
        "state": "credentials_needed",
        "manufacturer": "HIKVISION",
        "model": "DS-2CD2143G2-I",
        "endpoint_reference": "urn:uuid:cam",
    }
    raw.update(over)
    return Candidate.from_dict(raw)


def test_candidates_read_like_an_nvr_device_list() -> None:
    assert _candidate().title == "HIKVISION DS-2CD2143G2-I"
    assert _candidate(name="Front Gate").title == "Front Gate"
    assert _candidate().status_text == "Needs password"
    assert _candidate(state="ready").status_text == "Ready"
    added = _candidate(state="already_added", matched_camera={"id": "a", "name": "Gate"})
    assert added.status_text == "Added as Gate"
    assert not added.can_add
    maybe = _candidate(state="possible_match", matched_camera={"id": "a", "name": "Gate"})
    assert maybe.status_text == "May be Gate"
    assert maybe.can_add
    assert not _candidate(state="unavailable").can_add
    # Unknown or missing fields do not break the list.
    odd = Candidate.from_dict({"candidate_id": "x", "address": "10.0.0.9", "state": "new_state"})
    assert odd.title == "ONVIF device"
    assert not odd.can_add


def test_a_typed_range_is_checked_before_anything_is_sent() -> None:
    assert check_range("10.0.10.1/24") is None
    assert check_range("192.168.1.64") is None
    assert "1024" in (check_range("10.0.0.0/16") or "")
    assert "range" in (check_range("cameras") or "")
    assert "range" in (check_range("10.0.0.300/24") or "")


def test_streams_become_camera_streams_main_first_with_the_identity() -> None:
    choices = [
        StreamChoice.from_dict(
            {
                "stream_id": "s2",
                "name": "sub",
                "width": 640,
                "height": 360,
                "url": "rtsp://192.168.1.64:554/102",
                "source": "Profile_2",
            }
        ),
        StreamChoice.from_dict(
            {
                "stream_id": "s1",
                "name": "main",
                "width": 2560,
                "height": 1440,
                "url": "rtsp://192.168.1.64:554/101",
                "source": "Profile_1",
            }
        ),
    ]
    assert choices[1].label == "main · 2560×1440"
    streams = discovered_streams(choices)
    assert [s.url for s in streams] == [
        "rtsp://192.168.1.64:554/101",
        "rtsp://192.168.1.64:554/102",
    ]
    assert [s.id for s in streams] == ["1", "2"]
    # A choice without a URL (never the case locally) is left out.
    assert discovered_streams([StreamChoice.from_dict({"stream_id": "s3", "name": "x"})]) == []
