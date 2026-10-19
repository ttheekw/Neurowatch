"""Cloud-link enrollment-code parsing and backend link-status presentation.

The web portal shows a one-time *enrollment code*
(``<tenant_id>.<enrollment_id>.<transfer_proof>``) after an owner/admin creates
an enrollment. The backend links to NeuroWatch Cloud (a development build to
its development portal), so the operator only pastes this code and names the
node; the address is never shown. The backend remains the authority and
re-validates everything.
"""

from __future__ import annotations

import re
import uuid
from dataclasses import dataclass
from typing import Any

_PROOF = re.compile(r"[A-Za-z0-9_\-]{8,128}")
# Mirrors the node and cloud display-label rules (ASCII, starts alphanumeric,
# at most 64 characters) so obviously invalid names fail before any RPC.
_NODE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9 ._()#&'\-]{0,63}")
_INVALID_CODE = "The enrollment code is not valid. Copy it again from the web portal."


def parse_enrollment_code(text: str) -> str:
    """Return the normalized enrollment code; raise ``ValueError`` if malformed."""
    code = text.strip()
    parts = code.split(".")
    if len(parts) != 3:
        raise ValueError(_INVALID_CODE)
    tenant_id, enrollment_id, proof = parts
    for value in (tenant_id, enrollment_id):
        try:
            canonical = str(uuid.UUID(value))
        except ValueError as exc:
            raise ValueError(_INVALID_CODE) from exc
        if canonical != value:
            raise ValueError(_INVALID_CODE)
    if not _PROOF.fullmatch(proof):
        raise ValueError(_INVALID_CODE)
    return code


def begin_params(code: str, node_name: str) -> dict[str, str]:
    """Build ``cloud.begin`` params from a parsed enrollment code and a node name."""
    name = node_name.strip()
    if not _NODE_NAME.fullmatch(name):
        raise ValueError(
            "Node name must be 1–64 letters, digits, spaces or . _ ( ) # & ' - "
            "and start with a letter or digit."
        )
    return {"enrollment_code": code, "display_label": name}


_SUMMARIES = {
    "unlinked": "Not linked. Cloud services are off; everything local keeps working.",
    "awaiting_claim": "Linking started; waiting for the cloud to accept the claim.",
    "awaiting_local_confirmation": "Linking did not finish. Press Link to finish it.",
    "linked": "Linked to NeuroWatch Cloud.",
}

# Cloud setup is an installer/deployment concern. The operator at the wall can
# neither read nor set the backend's environment, so its variable names never
# reach the UI; they would only be copied into a support ticket verbatim.
_NOT_SET_UP = (
    "NeuroWatch Cloud is not set up on this computer. Ask whoever installed "
    "NeuroWatch to finish cloud setup; local viewing, recording and playback "
    "keep working without it."
)
_NOT_CONFIGURED = "Cloud is not configured on this computer."
_EXPIRED = (
    "The enrollment code expired before linking finished. Create a new enrollment "
    "code in the web portal and link again."
)
_INTERNAL_MARKERS = ("NW_CLOUD",)
# The portal answers 404 for any enrollment code it will not accept: expired
# (codes last five minutes), already used or cancelled, or mistyped.
_CODE_REFUSED = (
    "The web portal did not accept this enrollment code. It may have expired "
    "(codes last five minutes), been used already, or been mistyped. Create a "
    "new enrollment code in the web portal and link again."
)


def operator_error(message: str) -> str:
    """Return a backend error message that is safe to show to an operator.

    Backend cloud errors name the environment variables an installer has to
    set. Those are replaced wholesale by plain user text; anything else is
    passed through so real failures stay diagnosable.
    """
    upper = message.upper()
    if any(marker in upper for marker in _INTERNAL_MARKERS):
        return _NOT_SET_UP
    if "HTTP 404" in upper:
        return _CODE_REFUSED
    return message


@dataclass(frozen=True)
class CloudLinkStatus:
    phase: str
    tenant_id: str | None = None
    node_id: str | None = None
    last_check_in: str | None = None
    cloud_endpoint: str | None = None
    cloud_configured: bool = True
    pending_expired: bool = False
    error: str | None = None

    @classmethod
    def from_result(cls, result: dict[str, Any]) -> CloudLinkStatus:
        def text(key: str) -> str | None:
            value = result.get(key)
            return value if isinstance(value, str) else None

        def flag(key: str, default: bool) -> bool:
            value = result.get(key)
            return value if isinstance(value, bool) else default

        return cls(
            phase=text("phase") or "unknown",
            tenant_id=text("tenant_id"),
            node_id=text("node_id"),
            last_check_in=text("last_check_in"),
            cloud_endpoint=text("cloud_endpoint"),
            # A backend from before the flag existed reports only the URL.
            cloud_configured=flag("cloud_configured", True),
            pending_expired=flag("pending_expired", False),
            error=text("error"),
        )

    @property
    def can_begin(self) -> bool:
        if not self.cloud_configured:
            return False
        # An expired pending enrollment cannot be approved, but a fresh code can
        # replace it without unlinking first.
        return self.phase == "unlinked" or self.pending_expired

    @property
    def can_confirm(self) -> bool:
        """A claimed link can be finished without a new code (``cloud.confirm``)."""
        return (
            self.cloud_configured
            and not self.pending_expired
            and self.phase == "awaiting_local_confirmation"
        )

    @property
    def can_unlink(self) -> bool:
        return self.phase == "linked"

    @property
    def summary(self) -> str:
        if not self.cloud_configured:
            return _NOT_SET_UP
        if self.pending_expired:
            return _EXPIRED
        base = _SUMMARIES.get(self.phase, f"Unknown link state ({self.phase}).")
        if self.phase == "linked":
            check_in = self.last_check_in or "not yet"
            return f"{base} Last check-in: {check_in}."
        return base

    @property
    def cloud_description(self) -> str:
        """A hint when this computer cannot link at all; otherwise nothing
        (the cloud's address is not shown)."""
        return "" if self.cloud_configured else _NOT_CONFIGURED


@dataclass(frozen=True)
class CloudIndicator:
    """Bottom-bar cloud light: ``offline`` (red), ``connected`` (green) or
    ``active`` (blinking green: live video or recorded playback is being sent
    to NeuroWatch Cloud)."""

    state: str
    tooltip: str


def cloud_indicator(status: dict[str, Any] | None) -> CloudIndicator:
    """Map a ``cloud.status`` result (``None`` if it could not be read)."""
    if not isinstance(status, dict) or status.get("phase") != "linked":
        return CloudIndicator("offline", "NeuroWatch Cloud: not linked")
    if status.get("cloud_reachable") is not True:
        return CloudIndicator("offline", "NeuroWatch Cloud: linked, but can't reach the cloud")
    media = status.get("media_publish")
    sessions = media.get("sessions") if isinstance(media, dict) else None
    publishing = (
        sum(1 for state in sessions.values() if state == "publishing")
        if isinstance(sessions, dict)
        else 0
    )
    if publishing:
        noun = "stream" if publishing == 1 else "streams"
        return CloudIndicator("active", f"NeuroWatch Cloud: connected, sending {publishing} {noun}")
    return CloudIndicator("connected", "NeuroWatch Cloud: connected")
