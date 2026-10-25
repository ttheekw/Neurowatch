"""Camera search results as the desktop shows them (no Qt)."""

from __future__ import annotations

from neurowatch.models.discovery import (
    Candidate,
    StreamChoice,
    check_range,
    discovered_streams,
    parse_range,
    range_suggestions,
    widen_to_network,
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
    assert "192.168.1.x" in (check_range("") or "")
    assert "not a camera network" in (check_range("127.0.0.1") or "")


def test_a_network_needs_no_slash_and_ranges_and_lists_are_read_like_the_backend() -> None:
    lan = "Searches 192.168.1.1 – 192.168.1.254 · 254 addresses"
    for text in ("192.168.1.0/24", "192.168.1.77/24", "192.168.1", "192.168.1.*", "192.168.1.x"):
        found = parse_range(text)
        assert (found.label, found.count, found.summary) == ("192.168.1.0/24", 254, lan), text
    assert parse_range(" 192.168.1.X ").label == "192.168.1.0/24"
    assert parse_range("192.168.1.0").count == 254  # the network, not a camera

    span = parse_range("192.168.1.10 – 50")
    assert (span.label, span.count) == ("192.168.1.10-50", 41)
    assert span.summary == "Searches 192.168.1.10 – 192.168.1.50 · 41 addresses"
    assert parse_range("192.168.1.10-192.168.1.50") == span
    assert parse_range("10.0.0.250-10.0.1.5").label == "10.0.0.250-10.0.1.5"

    one = parse_range("192.168.1.64")
    assert (one.single, one.summary) == ("192.168.1.64", "Searches 192.168.1.64 only")
    assert widen_to_network("192.168.1.64") == "192.168.1.x"

    many = parse_range("192.168.1.x, 192.168.1.64; 10.0.0.5-6")
    assert many.label == "192.168.1.0/24, 192.168.1.64, 10.0.0.5-6"
    assert many.count == 256  # the address inside the network counted once
    assert many.summary == "Searches 256 addresses in 2 ranges, 10.0.0.5 – 192.168.1.254"

    assert "ends before it starts" in (check_range("192.168.1.50-10") or "")
    for bad in ("192.168.01.5", "192.168.*.*", "192.168.1.10-300", "192.168.1.0/33"):
        assert "Try 192.168.1.x" in (check_range(bad) or ""), bad
    big = check_range("10.0.0.1-10.0.4.1") or ""
    assert "1025" in big and "1024" in big


def test_one_click_networks_are_the_recorders_then_the_cameras_factory_ones() -> None:
    chips = range_suggestions(
        [
            {"cidr": "192.168.1.0/24", "address": "192.168.1.23", "name": "Wi-Fi"},
            {"cidr": "10.0.0.0/16", "address": "10.0.5.4", "name": "Ethernet"},
            {"cidr": "10.1.0.0/22", "address": "10.1.1.4", "name": "Ethernet 2"},
            {"cidr": "bad", "address": "x"},
        ]
    )
    assert [c.text for c in chips] == ["192.168.1.x", "10.0.5.x", "10.1.0.0/22", "192.168.0.x"]
    assert "Wi-Fi" in chips[0].tooltip and "Hikvision" in chips[0].tooltip
    assert [c.text for c in range_suggestions([])] == ["192.168.1.x", "192.168.0.x"]
    assert all(check_range(c.text) is None for c in chips)


def test_devices_found_without_onvif_say_what_they_are_and_what_they_need() -> None:
    sadp = _candidate(
        manufacturer="Hikvision",
        endpoint_reference=None,
        found_by=["sadp"],
        serial="DS-2CD2143G2-I2022AAWRJ1",
        mac="bc:ba:c2:01:02:03",
        activated=True,
        note="Its ONVIF service did not answer.",
    )
    assert sadp.title == "Hikvision DS-2CD2143G2-I"
    assert sadp.can_add and sadp.status_text == "Needs password"
    assert "Serial number DS-2CD2143G2-I2022AAWRJ1" in sadp.details
    assert "Found by Hikvision SADP" in sadp.details
    assert "did not answer" in sadp.details

    inactive = _candidate(state="unavailable", found_by=["sadp"], activated=False)
    assert inactive.status_text == "Not activated"
    assert not inactive.can_add and not inactive.can_set_up

    rtsp = Candidate.from_dict(
        {
            "candidate_id": "r",
            "address": "192.168.1.70",
            "state": "unavailable",
            "found_by": ["rtsp"],
            "stream_url_needed": True,
        }
    )
    assert rtsp.title == "RTSP device"
    assert rtsp.status_text == "Needs stream URL"
    assert not rtsp.can_add and rtsp.can_set_up
    # Old backends send none of it.
    assert _candidate().found_by == () and not _candidate().stream_url_needed


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
