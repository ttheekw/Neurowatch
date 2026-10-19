"""Alert notification settings as the desktop edits them (``alerts.get`` /
``alerts.set``): channels (where alerts go) and rules (which events send
them). The recorder validates again and keeps secrets: a secret it already
holds comes back as ``KEPT_SECRET`` and is sent back unchanged to keep it.
No Qt here.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any

KEPT_SECRET = "••••••••"
DEFAULT_NTFY_SERVER = "https://ntfy.sh"
DEFAULT_COOLDOWN_SEC = 60

#: Event kinds a rule can watch, in the order the editor lists them.
KIND_CHOICES: tuple[tuple[str, str], ...] = (
    ("motion", "Motion"),
    ("intrusion", "Intrusion"),
    ("line_crossing", "Line crossed"),
    ("tamper", "Camera tampering"),
    ("digital_input", "Alarm input"),
    ("camera_event", "Other camera events"),
    ("camera_offline", "Camera offline"),
    ("recording_failed", "Recording failed"),
    ("disk_low", "Recording drive almost full"),
    ("storage_missing", "Recording storage missing"),
    ("bookmark", "Bookmark added"),
    ("backend_started", "Recorder started"),
)
KIND_LABELS = dict(KIND_CHOICES)

#: Channel kinds: (kind, name in the editor, one-line explanation).
CHANNEL_KINDS: tuple[tuple[str, str, str], ...] = (
    ("desktop", "This computer", "A notification on this computer while NeuroWatch runs here."),
    (
        "ntfy",
        "ntfy push",
        "Push notifications to the ntfy app on your phone (ntfy.sh or your own server).",
    ),
    ("telegram", "Telegram", "Messages from your Telegram bot to a chat or channel."),
    (
        "webhook",
        "Webhook",
        "An HTTP POST with the alert as JSON (Home Assistant, Node-RED, scripts).",
    ),
)
CHANNEL_LABELS = {kind: name for kind, name, _ in CHANNEL_KINDS}
DAY_NAMES = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")


@dataclass
class Channel:
    id: str
    name: str
    kind: str
    enabled: bool = True
    attach_picture: bool = False
    #: Kind-specific fields: url/secret, server/topic/token, bot_token/chat_id.
    fields: dict[str, str] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Channel:
        common = {"id", "name", "kind", "enabled", "attach_picture"}
        return cls(
            id=str(raw.get("id", "")),
            name=str(raw.get("name", "")),
            kind=str(raw.get("kind", "desktop")),
            enabled=bool(raw.get("enabled", True)),
            attach_picture=bool(raw.get("attach_picture", False)),
            fields={k: str(v) for k, v in raw.items() if k not in common and v is not None},
        )

    def to_dict(self) -> dict[str, Any]:
        raw: dict[str, Any] = {
            "id": self.id,
            "name": self.name.strip(),
            "kind": self.kind,
            "enabled": self.enabled,
            "attach_picture": self.attach_picture,
        }
        for name in FIELDS.get(self.kind, ()):
            value = self.fields.get(name, "").strip()
            if value or name in REQUIRED.get(self.kind, ()):
                raw[name] = value
        return raw


#: Fields each channel kind sends, and which must be filled in.
FIELDS: dict[str, tuple[str, ...]] = {
    "desktop": (),
    "webhook": ("url", "secret"),
    "ntfy": ("server", "topic", "token"),
    "telegram": ("bot_token", "chat_id"),
}
REQUIRED: dict[str, tuple[str, ...]] = {
    "webhook": ("url",),
    "ntfy": ("topic",),
    "telegram": ("bot_token", "chat_id"),
}
SECRETS = frozenset({"secret", "token", "bot_token"})


@dataclass
class Window:
    days: list[int]
    start: str
    end: str

    def text(self) -> str:
        days = sorted(set(self.days))
        if days == list(range(7)):
            when = "daily"
        elif days == [0, 1, 2, 3, 4]:
            when = "weekdays"
        elif days == [5, 6]:
            when = "weekends"
        else:
            when = ", ".join(DAY_NAMES[d] for d in days if 0 <= d < 7)
        return f"{self.start}–{self.end} {when}"


@dataclass
class Rule:
    id: str
    name: str
    kinds: list[str]
    channels: list[str]
    cameras: list[str] = field(default_factory=list)
    schedule: list[Window] = field(default_factory=list)
    cooldown_sec: int = DEFAULT_COOLDOWN_SEC
    enabled: bool = True

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Rule:
        return cls(
            id=str(raw.get("id", "")),
            name=str(raw.get("name", "")),
            kinds=[str(k) for k in raw.get("kinds", [])],
            channels=[str(c) for c in raw.get("channels", [])],
            cameras=[str(c) for c in raw.get("cameras", [])],
            schedule=[
                Window(
                    [int(d) for d in w.get("days", [])],
                    str(w.get("start", "")),
                    str(w.get("end", "")),
                )
                for w in raw.get("schedule", [])
                if isinstance(w, dict)
            ],
            cooldown_sec=int(raw.get("cooldown_sec", DEFAULT_COOLDOWN_SEC)),
            enabled=bool(raw.get("enabled", True)),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name.strip(),
            "enabled": self.enabled,
            "kinds": list(self.kinds),
            "cameras": list(self.cameras),
            "schedule": [
                {"days": sorted(set(w.days)), "start": w.start, "end": w.end} for w in self.schedule
            ],
            "cooldown_sec": self.cooldown_sec,
            "channels": list(self.channels),
        }


@dataclass
class AlertSettings:
    channels: list[Channel] = field(default_factory=list)
    rules: list[Rule] = field(default_factory=list)

    @classmethod
    def from_dict(cls, raw: Any) -> AlertSettings:
        if not isinstance(raw, dict):
            return cls()
        return cls(
            channels=[Channel.from_dict(c) for c in raw.get("channels", []) if isinstance(c, dict)],
            rules=[Rule.from_dict(r) for r in raw.get("rules", []) if isinstance(r, dict)],
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "channels": [c.to_dict() for c in self.channels],
            "rules": [r.to_dict() for r in self.rules],
        }

    def remove_channel(self, channel_id: str) -> list[str]:
        """Remove a channel and its uses; names of rules left with no channel."""
        self.channels = [c for c in self.channels if c.id != channel_id]
        orphaned = []
        for rule in self.rules:
            if channel_id in rule.channels:
                rule.channels.remove(channel_id)
                if not rule.channels:
                    orphaned.append(rule.name)
        return orphaned


def new_id(name: str, taken: set[str]) -> str:
    """A unique id from a name: lowercase letters, digits and dashes."""
    base = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")[:32] or "item"
    candidate, n = base, 2
    while candidate in taken:
        candidate = f"{base}-{n}"
        n += 1
    return candidate


def channel_problem(channel: Channel) -> str | None:
    """What is missing before this channel can be saved (the recorder checks the rest)."""
    if not channel.name.strip():
        return "Give the channel a name."
    labels = {
        "url": "the webhook address",
        "topic": "the ntfy topic",
        "bot_token": "the bot token",
        "chat_id": "the chat id",
    }
    for name in REQUIRED.get(channel.kind, ()):
        if not channel.fields.get(name, "").strip():
            return f"Enter {labels.get(name, name)}."
    if channel.kind == "webhook" and not re.match(
        r"^https?://", channel.fields.get("url", "").strip()
    ):
        return "The webhook address starts with http:// or https://."
    if channel.kind == "ntfy" and not re.fullmatch(
        r"[A-Za-z0-9_-]{1,64}", channel.fields.get("topic", "").strip()
    ):
        return "An ntfy topic is 1–64 letters, digits, - or _."
    return None


def rule_problem(rule: Rule) -> str | None:
    if not rule.name.strip():
        return "Give the rule a name."
    if not rule.kinds:
        return "Choose at least one kind of event."
    if not rule.channels:
        return "Choose at least one channel to send to."
    for window in rule.schedule:
        if not window.days:
            return "Each time window needs at least one day."
        if window.start == window.end:
            return "A time window needs different start and end times."
    return None


def channel_summary(channel: Channel) -> str:
    kind = CHANNEL_LABELS.get(channel.kind, channel.kind)
    detail = ""
    if channel.kind == "ntfy":
        server = channel.fields.get("server", "").strip() or DEFAULT_NTFY_SERVER
        detail = (
            f"{re.sub(r'^https?://', '', server).rstrip('/')}/{channel.fields.get('topic', '')}"
        )
    elif channel.kind == "telegram":
        detail = f"chat {channel.fields.get('chat_id', '')}"
    elif channel.kind == "webhook":
        detail = re.sub(r"^https?://", "", channel.fields.get("url", ""))[:48]
    parts = [kind] + ([detail] if detail else [])
    if channel.attach_picture and channel.kind != "desktop":
        parts.append("with picture")
    if not channel.enabled:
        parts.append("off")
    return " · ".join(parts)


def cooldown_text(seconds: int) -> str:
    if seconds <= 0:
        return "every event"
    if seconds % 3600 == 0:
        return f"at most every {seconds // 3600} h"
    if seconds % 60 == 0:
        return f"at most every {seconds // 60} min"
    return f"at most every {seconds} s"


def rule_summary(rule: Rule, camera_names: dict[str, str], channel_names: dict[str, str]) -> str:
    """One line: what, where, when → to whom."""
    what = ", ".join(KIND_LABELS.get(k, k) for k in rule.kinds) or "No events"
    where = (
        ", ".join(camera_names.get(c, c) for c in rule.cameras) if rule.cameras else "every camera"
    )
    when = "; ".join(w.text() for w in rule.schedule) if rule.schedule else "any time"
    to = ", ".join(channel_names.get(c, c) for c in rule.channels) or "nobody"
    return f"{what} · {where} · {when} · {cooldown_text(rule.cooldown_sec)} → {to}"
