"""Loopback-only motion qualification source. No discovery or user state.

Started by the opt-in Rust media tests; stdout is only the allocated port.
Uses the same optional local GstRtspServer runtime as mock-ip-camera.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

runtime = Path(__file__).resolve().parents[2] / "mock-ip-camera/.cache/runtime/usr/lib"
for directory in runtime.glob("*/girepository-1.0"):
    os.environ["GI_TYPELIB_PATH"] = str(directory)
    # The wrapper starts a new interpreter to apply the loader path before GI loads.
    library_path = str(directory.parent)
    if library_path not in os.environ.get("LD_LIBRARY_PATH", "").split(":"):
        os.environ["LD_LIBRARY_PATH"] = library_path + ":" + os.environ.get("LD_LIBRARY_PATH", "")
        os.execv(sys.executable, [sys.executable, *sys.argv])

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstRtspServer", "1.0")
from gi.repository import GLib, Gst, GstRtspServer

Gst.init(None)
server = GstRtspServer.RTSPServer()
server.set_address("127.0.0.1")
server.set_service("0")
factory = GstRtspServer.RTSPMediaFactory()
codec = sys.argv[1] if len(sys.argv) > 1 else "h264"
encoder = (
    "x265enc tune=zerolatency speed-preset=ultrafast key-int-max=15 "
    "option-string=pools=1:frame-threads=1 ! h265parse ! rtph265pay"
    if codec == "h265"
    else "x264enc tune=zerolatency speed-preset=ultrafast key-int-max=15 threads=1 "
    "! h264parse ! rtph264pay"
)
factory.set_launch(
    "( videotestsrc is-live=true pattern=ball ! "
    "video/x-raw,width=320,height=180,framerate=15/1 ! videoconvert ! "
    + encoder + " name=pay0 pt=96 config-interval=1 )"
)
factory.set_shared(True)
server.get_mount_points().add_factory("/motion", factory)
assert server.attach(None)
print(server.get_bound_port(), flush=True)
GLib.MainLoop().run()
