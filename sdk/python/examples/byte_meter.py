"""A transport example: count real compressed bytes; no simulated detections."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from neurowatch_analytics import HttpRequest, HttpResponse, MediaUnit, Plugin, Session, serve


class ByteMeter(Plugin):
    def __init__(self) -> None:
        self.totals: dict[int, int] = {}

    def manifest(self) -> dict:
        return {
            "protocol": 1,
            "id": "example.byte-meter",
            "name": "Compressed byte meter",
            "version": "1.0",
            "vendor": "Example",
            "permissions": ["video", "web_ui"],
            "event_kinds": [],
            "media": {"video": {"stream": "sub", "codecs": ["h264", "h265"]}},
            "config_schema": {"type": "object", "properties": {}},
            "web_ui": {"entry": "/", "title": "Byte meter"},
        }

    async def unit(self, unit: MediaUnit, session: Session) -> None:
        self.totals[unit.binding] = self.totals.get(unit.binding, 0) + len(unit.data)

    async def detach(self, binding: int) -> None:
        self.totals.pop(binding, None)

    async def web(self, request: HttpRequest) -> HttpResponse:
        if request.path != "/":
            return HttpResponse(b"Not found", "text/plain", 404)
        # Session-local transport totals contain no names or cross-camera data.
        return HttpResponse(
            b"<title>Byte meter</title><h1>Compressed byte meter</h1>"
            b"<p>This example counts actual camera bytes in its daemon. "
            b"It emits no analytics events.</p>"
            b"<nw-camera-picker></nw-camera-picker><nw-live-tile></nw-live-tile>"
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", required=True)
    parser.add_argument("--identity", type=Path, required=True)
    options = parser.parse_args()
    print("Byte meter starting", file=sys.stderr)
    serve(ByteMeter(), address=options.listen, identity=options.identity)
