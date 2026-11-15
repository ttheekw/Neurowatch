"""MQTT and Home Assistant settings (``mqtt.get|set|status|test``).

No Qt: the Settings tab uses it, and tests read it directly.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Any

#: Stands for the stored password in ``mqtt.get``; sent back, it keeps it.
KEPT_SECRET = "••••••••"
DEFAULT_PORT = 1883
DEFAULT_TLS_PORT = 8883
DEFAULT_PREFIX = "neurowatch"
DEFAULT_DISCOVERY_PREFIX = "homeassistant"


@dataclass
class MqttSettings:
    enabled: bool = False
    host: str = ""
    port: int = DEFAULT_PORT
    tls: bool = False
    username: str = ""
    password: str = ""
    topic_prefix: str = DEFAULT_PREFIX
    discovery: bool = True
    discovery_prefix: str = DEFAULT_DISCOVERY_PREFIX

    @classmethod
    def from_json(cls, raw: object) -> MqttSettings:
        data = raw if isinstance(raw, dict) else {}
        defaults = cls()

        def text(name: str) -> str:
            value = data.get(name)
            return value if isinstance(value, str) else str(getattr(defaults, name))

        port = data.get("port")
        return cls(
            enabled=bool(data.get("enabled", False)),
            host=text("host"),
            port=port if isinstance(port, int) and 0 < port < 65536 else DEFAULT_PORT,
            tls=bool(data.get("tls", False)),
            username=text("username"),
            password=text("password"),
            topic_prefix=text("topic_prefix") or DEFAULT_PREFIX,
            discovery=bool(data.get("discovery", True)),
            discovery_prefix=text("discovery_prefix") or DEFAULT_DISCOVERY_PREFIX,
        )

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    def problem(self) -> str | None:
        """What is wrong before sending, in words; ``None`` when it can be saved."""
        if self.enabled and not self.host.strip():
            return "Enter the broker's address."
        host = self.host.strip()
        if "://" in host or (":" in host and not host.startswith("[")):
            return "Enter the broker's address without mqtt:// or a port; the port has its own box."
        for name, prefix in (
            ("Topic prefix", self.topic_prefix),
            ("Discovery prefix", self.discovery_prefix),
        ):
            if (
                not prefix
                or any(c in prefix for c in "+# ")
                or prefix.startswith("/")
                or prefix.endswith("/")
            ):
                return f"{name}: use letters, digits, _, - and inner /."
        return None


@dataclass(frozen=True)
class MqttStatus:
    state: str = "off"
    detail: str | None = None
    broker: str | None = None

    @classmethod
    def from_json(cls, raw: object) -> MqttStatus:
        data = raw if isinstance(raw, dict) else {}
        state = data.get("state")
        detail = data.get("detail")
        broker = data.get("broker")
        return cls(
            state=state if isinstance(state, str) else "off",
            detail=detail if isinstance(detail, str) else None,
            broker=broker if isinstance(broker, str) else None,
        )

    @property
    def text(self) -> str:
        where = f" to {self.broker}" if self.broker else ""
        if self.state == "connected":
            return f"Connected{where}."
        if self.state == "connecting":
            return f"Connecting{where}…"
        if self.state == "error":
            return f"Not connected{where}: {self.detail or 'unknown problem'}."
        return "Off. Turn it on and enter your broker to send events."

    @property
    def tone(self) -> str:
        return {"connected": "success", "connecting": "info", "error": "error"}.get(
            self.state, "muted"
        )


def topics_help(prefix: str) -> str:
    """The topics, for people writing their own automations."""
    base = prefix or DEFAULT_PREFIX
    return (
        f"{base}/<camera>/motion, …/person, …/vehicle, …/animal and …/online: ON or OFF. "
        f"{base}/<camera>/picture: the latest event picture. {base}/problem: the recorder needs "
        f"attention. {base}/events: every event as JSON. {base}/status: online or offline."
    )
