"""Exercise an installed bundle with no cameras, autostart or user state.

Run on the destination OS after installing/extracting the artifact. This checks
the actual packaged UI, its service runtime environment, backend IPC, media
plugins and shutdown. GPU/display/camera qualification is a separate test.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import tempfile
import time
from pathlib import Path


def rpc(port: int, method: str) -> dict:
    with socket.create_connection(("127.0.0.1", port), timeout=2) as connection:
        connection.sendall(
            (
                json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": {}}) + "\n"
            ).encode()
        )
        reply = json.loads(connection.makefile("rb").readline())
    if "error" in reply:
        raise RuntimeError(f"{method}: {reply['error']}")
    return reply["result"]


def smoke(bundle: Path) -> None:
    # Rust's Windows directory provider uses Known Folders rather than the
    # overridden APPDATA variables. Never run against a developer's user data.
    if os.name == "nt" and os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("Run Windows release smoke checks on a disposable GitHub Actions runner")
    suffix = ".exe" if os.name == "nt" else ""
    ui = bundle / "app" / "neurowatch-ui" / f"neurowatch-ui{suffix}"
    gst = bundle / "gstreamer"
    with tempfile.TemporaryDirectory(prefix="neurowatch-release-check-") as temp:
        state = Path(temp)
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("NW_", "GST_", "GSTREAMER_", "LIVEKIT_"))
        }
        for key in (
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
            "LOCALAPPDATA",
            "APPDATA",
        ):
            directory = state / key.lower()
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        # Prevent a Linux developer machine's user manager from being selected.
        env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=/nonexistent-neurowatch-test-bus"
        env["NW_CONFIG"] = str(state / "config.json")
        (state / "config.json").write_text(
            json.dumps(
                {
                    "version": 3,
                    "site_name": "Release smoke check",
                    "cameras": [],
                    "wall": {"rows": 1, "cols": 1, "auto_fit": False, "page_interval_sec": 0},
                    "ui": {"theme": "dark", "fullscreen": False},
                    "recording": {
                        "storage_path": str(state / "recordings"),
                        "segment_minutes": 1,
                        "pre_buffer_sec": 0,
                        "disk_threshold_percent": 0,
                    },
                }
            ),
            encoding="utf-8",
        )
        env["NW_SOURCE_ROOT"] = str(state / "no-source-checkout")
        env["QT_QPA_PLATFORM"] = "offscreen"
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        env["NW_IPC_ENDPOINT"] = f"tcp:127.0.0.1:{port}"
        env.pop("LD_LIBRARY_PATH", None)

        def ui_command(*args: str, expected: int = 0) -> None:
            result = subprocess.run([str(ui), *args], env=env, timeout=90, check=False)
            if result.returncode != expected:
                raise RuntimeError(
                    f"packaged UI {args}: exit {result.returncode}, expected {expected}"
                )

        try:
            ui_command("service", "status", "--json", expected=3)
            ui_command("service", "start", "--desktop", "--no-autostart")
            status = rpc(port, "system.status")
            assert status["cameras"] == 0, status
            assert status["site_name"] == "Release smoke check", status
            assert status["pid"] > 0, status
            print("Packaged backend:", json.dumps(status), flush=True)
            ui_command("service", "status", "--json")

            media_env = env.copy()
            media_env["PATH"] = os.pathsep.join(
                [str(gst / "bin"), str(bundle / "bin"), env["PATH"]]
            )
            media_env["LD_LIBRARY_PATH"] = str(gst / "lib")
            media_env["GST_PLUGIN_SYSTEM_PATH_1_0"] = str(gst / "lib/gstreamer-1.0")
            media_env["GST_PLUGIN_PATH_1_0"] = str(gst / "lib/gstreamer-1.0")
            media_env["GST_PLUGIN_SCANNER_1_0"] = str(
                gst / f"libexec/gstreamer-1.0/gst-plugin-scanner{suffix}"
            )
            media_env["GST_REGISTRY_1_0"] = str(state / "media-registry.bin")
            if os.name == "nt":
                media_env["GIO_MODULE_DIR"] = str(gst / "lib/gio/modules")
            inspect = str(gst / f"bin/gst-inspect-1.0{suffix}")
            for plugin in (
                "rtspsrc",
                "rtmpsrc",
                "souphttpsrc",
                "h264parse",
                "h265parse",
                "jpegparse",
                "jpegdec",
                "rtph264depay",
                "rtph265depay",
                "splitmuxsink",
                "matroskamux",
                "matroskademux",
                "mp4mux",
                "appsink",
                "decodebin",
                "avdec_h264",
                "avdec_h265",
                "avenc_aac",
                "alawdec",
                "mulawdec",
                "opusdec",
                "scaletempo",
                "autoaudiosink",
            ):
                subprocess.run([inspect, "--exists", plugin], env=media_env, check=True, timeout=30)
            if os.name != "nt":
                va = subprocess.check_output([inspect, "va"], env=media_env, text=True, timeout=30)
                assert "1.24.13" in va, va
            subprocess.run(
                [
                    str(gst / f"bin/gst-launch-1.0{suffix}"),
                    "-q",
                    "videotestsrc",
                    "num-buffers=10",
                    "!",
                    "jpegenc",
                    "!",
                    "jpegdec",
                    "!",
                    "fakesink",
                ],
                env=media_env,
                check=True,
                timeout=30,
            )
            for codec in ("h264", "h265"):
                fixture = Path(__file__).resolve().parent / "packaging" / f"smoke.{codec}"
                subprocess.run(
                    [
                        str(gst / f"bin/gst-launch-1.0{suffix}"),
                        "-q",
                        "filesrc",
                        f'location="{fixture.as_posix()}"',
                        "!",
                        f"{codec}parse",
                        "!",
                        f"avdec_{codec}",
                        "!",
                        "fakesink",
                    ],
                    env=media_env,
                    check=True,
                    timeout=30,
                )
        finally:
            try:
                ui_command("service", "stop")
            except Exception:
                # Still ask the isolated backend to exit if the UI command failed.
                try:
                    rpc(port, "system.shutdown")
                    time.sleep(1)
                except OSError:
                    pass
                raise
            finally:
                for log in state.rglob("*.log"):
                    print(log.name, log.read_text(errors="replace")[-12000:])
        ui_command("service", "status", "--json", expected=3)
        print("Release smoke check passed", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    smoke(parser.parse_args().bundle.resolve())
