"""Network activity: rates in the status bar and on the Diagnostics page."""

from __future__ import annotations

import pytest

from neurowatch.utils.network import format_bitrate


@pytest.mark.parametrize(
    ("bits", "shown"),
    [
        (None, "—"),
        (0, "0 Kbps"),
        (400, "0.4 Kbps"),
        (950_000, "950 Kbps"),
        (12_340_000, "12.3 Mbps"),
        (128_000_000, "128 Mbps"),
        (999_600_000, "1 Gbps"),  # would round to 1000 Mbps
        (1_024_000_000, "1.02 Gbps"),  # 1024 Mbps shows in Gbps
        (2_500_000_000_000, "2.5 Tbps"),
    ],
)
def test_rates_use_the_largest_unit_under_1000(bits: float | None, shown: str) -> None:
    assert format_bitrate(bits) == shown


NET_DEV = """\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets ...
    lo: 9000 10 0 0 0 0 0 0 9000 10 0 0 0 0 0 0
enp3s0: 1000 10 0 0 0 0 0 0 200 5 0 0 0 0 0 0
wlp2s0: 500 4 0 0 0 0 0 0 100 2 0 0 0 0 0 0
docker0: 7000 9 0 0 0 0 0 0 7000 9 0 0 0 0 0 0
"""


def test_the_diagnostics_page_shows_the_backends_network_rates(qtbot) -> None:
    from neurowatch.widgets.performance_page import PerformancePage

    page = PerformancePage(client_factory=lambda: None, refresh_interval_ms=60_000)
    qtbot.addWidget(page)
    page._render_snapshot(
        {"process": {"network": {"rx_bits_per_sec": 950_000, "tx_bits_per_sec": 2_000_000_000}}}
    )
    assert page._process_grid["Network down"].val.text() == "950 Kbps"
    assert page._process_grid["Network up"].val.text() == "2 Gbps"
    page._render_snapshot({"process": {"network": None}})  # not measured yet
    assert page._process_grid["Network down"].val.text() == "—"
