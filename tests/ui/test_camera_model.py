"""Tests for CameraConfig's handling of the retired direct-S3 recording feature.

`CameraRecordingConfig` no longer has an `s3` field. A legacy config file or
RPC payload written before the removal may still contain a `recording.s3`
object with bucket/endpoint/access/secret keys; it must be discarded on load
and must never reappear in a subsequent `to_dict()` (e.g. before a config
save).
"""

from __future__ import annotations

import dataclasses

from neurowatch.models.camera import CameraConfig, CameraRecordingConfig

LEGACY_S3_OBJECT = {
    "enabled": True,
    "endpoint": "https://s3.example.invalid",
    "bucket": "legacy-bucket",
    "prefix": "site-a",
    "access_key": "AKIA-LEGACY",
    "secret_key": "legacy-secret",
    "quota_gb": 50,
}


def _legacy_camera_dict() -> dict:
    return {
        "id": "cam-legacy",
        "name": "Legacy Cam",
        "url": "rtsp://example/stream",
        "sub_url": "",
        "username": "",
        "password_b64": "",
        "transport": "tcp",
        "protocol": "auto",
        "notes": "",
        "wall_position": None,
        "recording": {
            "schedule": None,
            "local": {"enabled": True, "storage_path": None, "quota_gb": None},
            "s3": LEGACY_S3_OBJECT,
        },
    }


def test_recording_config_has_no_s3_field() -> None:
    fields = {f.name for f in dataclasses.fields(CameraRecordingConfig)}
    assert "s3" not in fields


def test_from_dict_discards_legacy_s3_object_without_error() -> None:
    cam = CameraConfig.from_dict(_legacy_camera_dict())

    assert cam.recording.local.enabled is True
    assert not hasattr(cam.recording, "s3")


def test_legacy_s3_credentials_do_not_reappear_after_round_trip_to_dict() -> None:
    cam = CameraConfig.from_dict(_legacy_camera_dict())

    persisted = cam.to_dict()
    rec = persisted["recording"]
    assert "s3" not in rec

    persisted_str = str(persisted)
    for secret in ("legacy-bucket", "AKIA-LEGACY", "legacy-secret", "s3.example.invalid"):
        assert secret not in persisted_str, f"leaked legacy S3 credential: {secret}"


def test_default_camera_config_serializes_without_s3_fields() -> None:
    cam = CameraConfig()
    persisted = cam.to_dict()
    assert "s3" not in persisted["recording"]


def test_discovery_identity_round_trips_and_is_optional() -> None:
    raw = {
        "id": "cam-1",
        "name": "Gate",
        "url": "rtsp://10.0.0.5/live",
        "discovery": {"endpoint_reference": "urn:uuid:cam", "source": "Profile_101"},
    }
    cam = CameraConfig.from_dict(raw)
    assert cam.discovery is not None
    assert cam.discovery.endpoint_reference == "urn:uuid:cam"
    assert cam.to_dict()["discovery"] == {
        "endpoint_reference": "urn:uuid:cam",
        "source": "Profile_101",
    }
    plain = CameraConfig.from_dict({"id": "cam-2", "name": "Yard", "url": "rtsp://x/1"})
    assert plain.discovery is None
    assert "discovery" not in plain.to_dict()
