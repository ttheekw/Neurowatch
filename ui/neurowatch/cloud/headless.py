"""NeuroWatch Cloud in the headless commands.

Called by ``neurowatch.headless``:

- a ``Cloud: …`` status line, reprinted by the monitor when it changes;
- ``--link CODE [--name NAME]`` for ``--headless``: link on startup;
- ``neurowatch cloud (status | link CODE [--name NAME] | unlink --yes)``.

This module must not import Qt, so it also runs on servers without a display.
"""

from __future__ import annotations

import argparse
import os
from collections.abc import Callable
from typing import Any

from neurowatch import headless
from neurowatch.cloud.cloud_link import (
    CloudLinkStatus,
    begin_params,
    cloud_indicator,
    parse_enrollment_code,
)
from neurowatch.headless import Rpc

_STATUS_TIMEOUT_S = 5.0
_CONFIRM_TIMEOUT_S = 45.0  # backend allows 10 s connect + 20 s request
_BEGIN_TIMEOUT_S = 2 * _CONFIRM_TIMEOUT_S  # cloud.begin also confirms the claim
# The command line is used by whoever installs and runs the node, so it may
# name the setting to change; the desktop UI deliberately does not.
_NOT_CONFIGURED = (
    "NeuroWatch Cloud is not available in this backend (a development build's "
    "NW_CLOUD_ENDPOINT may be unusable; details in the backend log)."
)
_CODE_REFUSED = (
    "The web portal did not accept this enrollment code. It may have expired "
    "(codes last five minutes), been used already, or been mistyped. Create a new "
    "enrollment code in the web portal and try again."
)


def cloud_line(cloud: dict[str, Any] | None) -> str:
    if cloud is None:
        return "Cloud: status unavailable from this backend"
    status = CloudLinkStatus.from_result(cloud)
    if not status.cloud_configured:
        return "Cloud: not configured for this backend"
    if status.pending_expired:
        return "Cloud: link expired before it finished; create a new enrollment code"
    if status.phase == "unlinked":
        return "Cloud: not linked (link with: neurowatch cloud link CODE --name NAME)"
    if status.phase == "awaiting_claim":
        return "Cloud: linking, waiting for the cloud to accept the claim"
    if status.phase == "awaiting_local_confirmation":
        return "Cloud: linking did not finish; run the link command again to finish it"
    if status.phase != "linked":
        return f"Cloud: {status.phase}"
    indicator = cloud_indicator(cloud)
    if indicator.state == "offline":
        return f"Cloud: linked, but {_unreachable_reason(cloud)}"
    text = indicator.tooltip.removeprefix("NeuroWatch Cloud: ")
    return f"Cloud: {text}"


def _unreachable_reason(cloud: dict[str, Any]) -> str:
    problem = cloud.get("cloud_problem")
    endpoint = str(cloud.get("cloud_endpoint") or "")
    if problem == "link_refused":
        return (
            "the cloud refused this node (it may have been removed in the web portal); "
            "unlink and link again"
        )
    if problem == "endpoint_unusable":
        if endpoint.startswith(("http://127.", "http://localhost", "http://[::1]")):
            return (
                "this local development address is only allowed with "
                "NW_CLOUD_DEV_ALLOW_LOOPBACK_HTTP=1: start with scripts/dev_cloud_headless.sh"
            )
        return "the saved cloud address is not usable (details in the backend log)"
    return "cannot reach it"


def _optional_status(rpc: Rpc) -> dict[str, Any] | None:
    try:
        cloud = rpc("cloud.status", {}, _STATUS_TIMEOUT_S)
    except Exception:
        return None
    return cloud if isinstance(cloud, dict) else None


def _error_text(exc: Exception) -> str:
    message = str(exc)
    if "NW_CLOUD" in message.upper() or "NOT LINKED TO A CLOUD" in message.upper():
        return _NOT_CONFIGURED
    if "HTTP 404" in message.upper():
        return _CODE_REFUSED
    return message


def _code_problem(text: str) -> str:
    """What is wrong with a would-be enrollment code, without echoing its secret part."""
    parts = text.strip().split(".")
    if len(parts) != 3:
        shown = text.strip()[:12] or "nothing"
        return f"got {len(parts)} part(s) starting {shown!r}, expected 3 separated by dots"
    return "its first two parts must be IDs like 1f0e8c2a-…, and the last part a token"


def default_name() -> str:
    return (os.uname().nodename.split(".")[0] or "NeuroWatch")[:64]


def link(rpc: Rpc, code_text: str, name: str, say: Callable[[str], None]) -> int:
    """Link to NeuroWatch Cloud. Returns a process exit code."""
    try:
        code = parse_enrollment_code(code_text)
    except ValueError:
        say(
            "Cloud link: that is not an enrollment code "
            f"({_code_problem(code_text)}). In the web portal, open Nodes and choose "
            "Create enrollment, copy the whole code it shows (valid 5 minutes), and pass "
            "it in quotes: neurowatch --headless --link '<code>' --name 'My node'"
        )
        return 2
    try:
        params = begin_params(code, name)
    except ValueError as exc:
        say(f"Cloud link: {exc}")
        return 2
    try:
        status = CloudLinkStatus.from_result(rpc("cloud.status", {}, _STATUS_TIMEOUT_S))
    except Exception as exc:
        say(f"Cloud link: cannot read the cloud status: {_error_text(exc)}")
        return 1
    if not status.cloud_configured:
        say(f"Cloud link: {_NOT_CONFIGURED}")
        return 1
    if status.phase == "linked":
        say("Cloud link: already linked; nothing to do (unlink first to link elsewhere)")
        return 0
    if not status.can_begin and not status.can_confirm:
        say(f"Cloud link: cannot start now ({status.summary})")
        return 1
    try:
        if status.can_begin:
            say(f"Cloud link: linking as {params['display_label']!r}…")
            result = rpc("cloud.begin", params, _BEGIN_TIMEOUT_S)
        else:
            # The claim was accepted earlier; finish it without spending the code.
            say("Cloud link: finishing the earlier link…")
            result = rpc("cloud.confirm", {}, _CONFIRM_TIMEOUT_S)
    except Exception as exc:
        say(f"Cloud link failed: {_error_text(exc)}")
        return 1
    say(cloud_line(result if isinstance(result, dict) else None))
    return 0


def unlink(rpc: Rpc, confirmed: bool, say: Callable[[str], None]) -> int:
    if not confirmed:
        say("Unlinking turns off cloud services for this node. Re-run with --yes to confirm.")
        return 2
    try:
        rpc("cloud.unlink", {}, _STATUS_TIMEOUT_S)
    except Exception as exc:
        say(f"Cloud unlink failed: {_error_text(exc)}")
        return 1
    say("Cloud: unlinked; local capture, recording and playback keep working")
    return 0


# ---------------------------------------------------------------------------
# Called by neurowatch.headless
# ---------------------------------------------------------------------------


def status_line(rpc: Rpc) -> str:
    return cloud_line(_optional_status(rpc))


def add_monitor_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--link", metavar="CODE", help="link to NeuroWatch Cloud first")
    parser.add_argument("--name", help="node name shown in the web portal (with --link)")


def before_monitor(args: argparse.Namespace, rpc: Rpc, say: Callable[[str], None]) -> int | None:
    if args.name and not args.link:
        say("--name needs --link")
        return 2
    if args.link:
        result = link(rpc, args.link, args.name or default_name(), say)
        # Bad input stops at once; a cloud failure must not stop local
        # capture and recording, so the node keeps running unlinked.
        if result == 2:
            return result
    return None


def add_commands(subparsers: Any) -> None:
    cloud = subparsers.add_parser("cloud", help="NeuroWatch Cloud: status, link, unlink")
    actions = cloud.add_subparsers(dest="cloud_command", required=True)
    actions.add_parser("status", help="print camera, recording and cloud status once")
    join = actions.add_parser("link", help="link to NeuroWatch Cloud with an enrollment code")
    join.add_argument("code", help="enrollment code from the web portal (Nodes, Create enrollment)")
    join.add_argument("--name", help="node name shown in the web portal (default: host name)")
    leave = actions.add_parser("unlink", help="unlink from NeuroWatch Cloud")
    leave.add_argument("--yes", action="store_true", help="confirm unlinking")


def run_command(args: argparse.Namespace, rpc: Rpc, say: Callable[[str], None]) -> int | None:
    if args.command != "cloud":
        return None
    if args.cloud_command == "status":
        return headless.print_status(rpc, say)
    if args.cloud_command == "link":
        return link(rpc, args.code, args.name or default_name(), say)
    return unlink(rpc, bool(args.yes), say)
