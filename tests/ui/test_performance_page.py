"""Behavior tests for the performance diagnostics page.

These tests use pytest-qt to drive a real ``PerformancePage`` widget with a
stubbed IPC client. Nothing here depends on a live backend, live camera, or
real hardware — the client is a plain object with a ``call`` method.
"""

from __future__ import annotations

import threading
from typing import Any

import pytest
from PySide6.QtWidgets import QSizePolicy

from neurowatch.widgets.performance_page import PerformancePage


def _wait_render(qtbot, page, timeout_ms: int = 2000):
    """Fire refresh_now and wait for the render signal.

    The production refresh path is non-blocking, so tests must observe the
    ``snapshot_rendered`` signal to know when the widget has consumed the
    reply — never call ``worker.wait()`` or ``processEvents()`` on the main
    thread.
    """
    with qtbot.waitSignal(page.snapshot_rendered, timeout=timeout_ms):
        page.refresh_now()


class _StubClient:
    """Minimal SyncIpcClient stand-in that records calls and returns queued responses."""

    def __init__(self, responses: list[Any] | Any = None) -> None:
        if responses is None:
            responses = []
        if not isinstance(responses, list):
            responses = [responses]
        self._responses = list(responses)
        self.calls: list[tuple[str, dict | None]] = []

    def call(self, method: str, params: dict | None = None) -> Any:
        self.calls.append((method, params))
        if not self._responses:
            raise RuntimeError("no queued response")
        head = self._responses.pop(0)
        if isinstance(head, Exception):
            raise head
        return head


def test_motion_diagnostics_distinguish_analysis_from_desktop_decode(qtbot):
    snapshot = _snapshot(
        motion_decoders=1,
        motion=[
            {
                "camera_id": "front-door",
                "state": "running",
                "low_cpu": True,
                "using_substream": True,
                "decoder": "avdec_h264",
                "analysis_fps": 0.5,
                "analysis_ms": 0.7,
                "dropped_frames": 3,
                "dropped_fps": 0,
            }
        ],
        bottleneck={"kind": "motion", "severity": "warning", "explanation": "Analysis queue loss"},
    )
    page = PerformancePage(ipc_client=_StubClient(snapshot))
    qtbot.addWidget(page)
    _wait_render(qtbot, page)
    text = page.snapshot_text()
    for expected in (
        "1 analysis decoder running",
        "Keyframes only · substream",
        "avdec_h264",
        "0.5 fps",
        "0.7 ms / picture",
        "3 (0.0/s)",
    ):
        assert expected in text
    assert "Motion detection" in page.bottleneck_text()


def _snapshot(**overrides: Any) -> dict:
    base: dict = {
        "captured_at_utc": "2026-09-16T12:00:00Z",
        "video_acceleration": {
            "kind": "nvdec",
            "label": "NVDEC",
            "hardware": True,
        },
        "process": {
            "cpu_percent": 12.5,
            "rss_bytes": 268_435_456,
            "memory_percent": 3.1,
            "system_cpu_percent": 40.0,
            "system_memory_percent": 55.0,
            "siblings": [
                {"label": "nw-backend", "pid": 42, "cpu_percent": 12.5, "rss_bytes": 268_435_456}
            ],
        },
        "gpu": {
            "status": "available",
            "metrics": {
                "name": "NVIDIA Test GPU",
                "source": "nvidia-smi",
                "utilization_percent": 30.0,
                "memory_used_bytes": 1_073_741_824,
                "memory_total_bytes": 8_589_934_592,
                "decoder_utilization_percent": 20.0,
            },
        },
        "ipc": {
            "requests_total": 8,
            "messages_total": 8,
            "bytes_in_total": 1024,
            "bytes_out_total": 4096,
            "avg_latency_ms": 2.1,
            "p95_latency_ms": 4.7,
            "drops_total": 0,
            "backpressure_events_total": 0,
            "inflight": 0,
        },
        "cameras": [
            {
                "camera_id": "cam-1",
                "renderer_fps": None,
                "decoder_fps": 25.0,
                "renderer_drops": 0,
                "decoder_drops": 0,
                "end_to_end_latency_ms": 41.0,
                "state": "streaming",
            }
        ],
        "bottleneck": {
            "kind": "none",
            "severity": "ok",
            "explanation": "all monitored subsystems within normal range",
        },
    }
    base.update(overrides)
    return base


def test_performance_page_renders_process_and_ipc_fields(qtbot):
    client = _StubClient(_snapshot())
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    text = page.snapshot_text()
    assert "12.5" in text, "backend CPU% must be shown"
    assert "256" in text or "268" in text, "RSS must be shown in MB or bytes: got {text!r}"
    assert "55" in text, "system memory% must be shown"
    assert "2.1" in text, "avg IPC latency must be shown"
    assert "4.7" in text, "p95 IPC latency must be shown"


def test_performance_page_renders_gpu_when_available(qtbot):
    client = _StubClient(_snapshot())
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    text = page.snapshot_text()
    assert "NVIDIA Test GPU" in text
    assert "nvidia-smi" in text
    assert "30" in text  # utilization
    assert "NVIDIA NVDEC" in page.acceleration_text()
    assert "hardware video decoding is active" in page.acceleration_text().lower()
    assert "border: none" in page._accel_badge_label.styleSheet()
    assert "background: transparent" in page._accel_badge_label.styleSheet()
    assert page._gpu_container.sizePolicy().verticalPolicy() == QSizePolicy.Policy.Maximum, (
        "decoder summary must remain compact instead of stretching with camera health"
    )


def test_performance_page_reports_blocked_local_live_view(qtbot):
    client = _StubClient(
        _snapshot(
            video_acceleration={
                "kind": "blocked",
                "label": "Live view blocked — update GStreamer",
                "hardware": False,
                "available": False,
            }
        )
    )
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    assert "Local live view blocked" in page.acceleration_text()
    assert "1.24.6" in page.acceleration_text()


def test_performance_page_shows_vaapi_available_with_unknown_counters(qtbot):
    snap = _snapshot(
        video_acceleration={
            "kind": "vaapi",
            "label": "VA-API",
            "hardware": True,
        },
        gpu={
            "status": "available",
            "metrics": {
                "name": "VA-API device",
                "source": "vaapi",
                "utilization_percent": None,
                "memory_used_bytes": None,
                "memory_total_bytes": None,
                "decoder_utilization_percent": None,
            },
        },
    )
    client = _StubClient(snap)
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    text = page.snapshot_text().lower()
    assert "va-api device" in text
    assert "source: vaapi" in text
    assert "gpu: unavailable" not in text
    assert page.gpu_utilization_text() == "Utilization: unavailable"
    assert "VA-API" in page.acceleration_text()


def test_performance_page_shows_gpu_unavailable_with_reason(qtbot):
    snap = _snapshot(gpu={"status": "unavailable", "reason": "no supported GPU counter source"})
    client = _StubClient(snap)
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    text = page.snapshot_text().lower()
    assert "unavailable" in text
    assert "no supported gpu counter source" in text
    # No fabricated numbers — no fake 0% GPU utilization shown as a real value.
    assert "0%" not in page.gpu_utilization_text()


def test_performance_page_renders_per_camera_health(qtbot):
    client = _StubClient(_snapshot())
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    text = page.snapshot_text()
    assert "cam-1" in text
    assert "24.5" in text or "25" in text
    assert "streaming" in text


def test_performance_page_shows_bottleneck_severity_and_explanation(qtbot):
    snap = _snapshot(
        bottleneck={
            "kind": "cpu",
            "severity": "critical",
            "explanation": "CPU saturated at 96% — pipelines will drop frames",
        }
    )
    client = _StubClient(snap)
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    b_text = page.bottleneck_text()
    assert "cpu" in b_text.lower()
    assert "critical" in b_text.lower()
    assert "96%" in b_text


def test_performance_page_shows_error_state_on_ipc_failure(qtbot):
    client = _StubClient(RuntimeError("backend not reachable"))
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    text = page.snapshot_text().lower()
    assert "error" in text or "unavailable" in text
    assert "backend not reachable" in page.snapshot_text()


def test_performance_page_periodic_refresh_triggers_call(qtbot):
    # Two snapshots — one for the first refresh, one for the timer-driven refresh.
    snap1 = _snapshot()
    snap2 = _snapshot(bottleneck={"kind": "ipc", "severity": "warning", "explanation": "slow"})
    client = _StubClient([snap1, snap2])
    page = PerformancePage(ipc_client=client, refresh_interval_ms=50)
    qtbot.addWidget(page)

    page.start_refresh()
    # The timer-driven refresh is asynchronous: wait for the second snapshot
    # to be *rendered*, not merely requested.
    qtbot.waitUntil(lambda: page.bottleneck_text().lower().startswith("ipc"), timeout=1500)
    page.stop_refresh()

    assert len(client.calls) >= 2
    assert client.calls[0][0] == "stats.get_performance"


def test_performance_page_calls_client_with_stats_get_performance(qtbot):
    client = _StubClient(_snapshot())
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    assert client.calls, "refresh must invoke the IPC client"
    method, params = client.calls[0]
    assert method == "stats.get_performance"
    # Bounded, credential-free: params are empty or None.
    assert params in (None, {}), f"stats method should not receive params, got {params!r}"


@pytest.mark.parametrize(
    "severity, expected_color_hex_substring",
    [
        ("ok", "#3fbf6f"),
        ("warning", "#e0a020"),
        ("critical", "#d0453d"),
    ],
)
def test_performance_page_bottleneck_color_reflects_severity(
    qtbot, severity, expected_color_hex_substring
):
    snap = _snapshot(bottleneck={"kind": "cpu", "severity": severity, "explanation": "test"})
    client = _StubClient(snap)
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)

    _wait_render(qtbot, page)

    assert expected_color_hex_substring.lower() in page.bottleneck_style().lower(), (
        f"severity {severity} should render with color {expected_color_hex_substring}"
    )


def test_performance_page_refresh_does_not_block_main_thread(qtbot):
    """A slow backend must not freeze the UI: refresh_now returns while the
    IPC call is still in flight, and the render arrives later via signal."""
    release = threading.Event()
    entered = threading.Event()

    class _SlowClient(_StubClient):
        def call(self, method: str, params: dict | None = None) -> Any:
            entered.set()
            release.wait(timeout=5)
            return super().call(method, params)

    client = _SlowClient(_snapshot())
    page = PerformancePage(ipc_client=client, refresh_interval_ms=0)
    qtbot.addWidget(page)
    rendered: list[bool] = []
    page.snapshot_rendered.connect(lambda: rendered.append(True))

    try:
        page.refresh_now()
        assert entered.wait(timeout=2), "worker never issued the IPC call"
        assert rendered == [], "refresh_now must not wait for the backend reply"
        assert "NVIDIA Test GPU" not in page.snapshot_text()
    finally:
        release.set()

    qtbot.waitUntil(lambda: rendered == [True], timeout=2000)
    assert "NVIDIA Test GPU" in page.snapshot_text()
