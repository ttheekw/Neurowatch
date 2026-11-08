"""Framed-stdio fixture for Rust/Python transport interoperability tests."""

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from neurowatch_analytics import HttpResponse, Plugin, Session


class Fixture(Plugin):
    def manifest(self):
        return {
            "id": "nw.sdk-fixture",
            "name": "SDK fixture",
            "vendor": "NeuroWatch",
            "version": "1",
            "permissions": ["video", "events", "web_ui"],
            "media": {"video": {"stream": "main", "codecs": ["h264"]}},
            "web_ui": {"entry": "/", "title": "Fixture"},
        }

    async def unit(self, unit, session):
        await session.notify(
            "test.unit", {"seq": unit.seq, "time_us": unit.time_us, "data": unit.data.hex()}
        )

    async def web(self, request):
        return HttpResponse(b"<h1>Protocol fixture</h1>")


if __name__ == "__main__":
    asyncio.run(Session(Fixture(), sys.stdin.buffer, sys.stdout.buffer).run())
