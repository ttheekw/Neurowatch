"""Static contract tests for the Docker images and their release jobs; nothing is built."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest

from test_release_packaging import RELEASE_WORKFLOW, _jobs

ROOT = Path(__file__).resolve().parents[1]
DOCKERFILE = ROOT / "docker/Dockerfile"
ENTRYPOINT = ROOT / "docker/entrypoint.sh"
RUN_SCRIPT = ROOT / "scripts/docker.sh"
SMOKE_SCRIPT = ROOT / "scripts/docker_smoke.sh"


def _stages(text: str) -> dict[str, str]:
    parts = re.split(r"^FROM (\S+) AS (\S+)$", text, flags=re.MULTILINE)
    return {name: f"FROM {base}\n{body}" for base, name, body in zip(*[iter(parts[1:])] * 3)}


@pytest.fixture(scope="module")
def workflow() -> str:
    return RELEASE_WORKFLOW.read_text()


def test_images_install_the_release_package_with_headless_as_the_default_target() -> None:
    text = DOCKERFILE.read_text()
    stages = _stages(text)
    assert list(stages) == ["base", "gui", "headless"]  # headless last: plain `docker build`
    # The arm64 packages need glibc 2.39.
    assert stages["base"].startswith("FROM ubuntu:24.04")
    assert "/tmp/neurowatch.deb" in stages["base"] and "tini" in stages["base"]
    assert stages["gui"].startswith("FROM base") and stages["headless"].startswith("FROM base")
    assert 'CMD ["headless"]' in stages["headless"] and 'CMD ["gui"]' in stages["gui"]
    assert 'ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/neurowatch-docker"]' in text
    assert 'CMD ["neurowatch-docker", "health"]' in text
    assert "org.opencontainers.image.source" in text  # links the GHCR package to the repo


def test_images_run_as_a_user_with_state_in_volumes() -> None:
    text = DOCKERFILE.read_text()
    for stage in _stages(text).values():
        users = re.findall(r"^USER (\S+)$", stage, re.MULTILINE)
        assert not users or users[-1] == "neurowatch"
    for setting in (
        "HOME=/data",
        "XDG_CONFIG_HOME=/data/config",
        "XDG_DATA_HOME=/data/share",
        "XDG_STATE_HOME=/data/state",
        "NW_RECORDINGS=/recordings",
        "NW_SERVICE=external",  # the container runs the backend, not the service manager
        "LANG=C.UTF-8",
        # --gpus all alone brings NVDEC and OpenGL (the toolkit's default leaves them out).
        "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility,graphics",
    ):
        assert setting in text
    assert 'VOLUME ["/data", "/recordings"]' in text


def test_gui_image_uses_the_systems_x11_and_cxx_libraries() -> None:
    # The bundle's (Ubuntu 22.04) libX11 and libstdc++ each make Ubuntu 24.04's
    # Mesa fail GLX/EGL, and Qt aborts with "Could not initialize GLX".
    gui = _stages(DOCKERFILE.read_text())["gui"]
    removal = gui[gui.index("_internal") :]
    for library in ("libX11.so.6", "libxcb*.so.*", "libstdc++.so.6", "libgcc_s.so.1"):
        assert library in removal
    assert "ldconfig -p" in removal  # only where the system has its own
    assert "QT_X11_NO_MITSHM=1" in gui


def test_entrypoint_runs_the_backend_with_the_bundled_gstreamer() -> None:
    text = ENTRYPOINT.read_text()
    for variable in (
        "LD_LIBRARY_PATH=$GST/lib",
        "GST_PLUGIN_SYSTEM_PATH_1_0=$GST/lib/gstreamer-1.0",
        "GST_PLUGIN_SCANNER_1_0=$GST/libexec/gstreamer-1.0/gst-plugin-scanner",
        "GST_REGISTRY_1_0=",
    ):
        assert variable in text
    assert 'exec env "${BACKEND_ENV[@]}" NW_HEADLESS=1 "$BASE/bin/nw-backend"' in text
    assert 'env "${BACKEND_ENV[@]}" NW_HEADLESS=0 "$BASE/bin/nw-backend" &' in text
    # The UI starts no service of its own and registers nothing.
    assert "neurowatch --no-autostart" in text
    assert 'wait -n "$backend" "$ui"' in text
    assert "--group-add $(stat -c %g" in text  # warns about an inaccessible render node


@pytest.mark.parametrize("script", [ENTRYPOINT, RUN_SCRIPT, SMOKE_SCRIPT])
def test_docker_scripts_are_valid_bash(script: Path) -> None:
    assert subprocess.run(["bash", "-n", str(script)]).returncode == 0


@pytest.mark.parametrize(
    "args",
    [
        ("build", "all", "--tag", "x"),
        ("build", "--deb", "a.deb", "--release", "v1.0.0"),
        ("headless", "--wayland"),
        ("nonsense",),
    ],
)
def test_run_script_rejects_invalid_arguments(args: tuple[str, ...]) -> None:
    result = subprocess.run(["bash", str(RUN_SCRIPT), *args], capture_output=True, text=True)
    assert result.returncode == 1 and "error:" in result.stderr


def test_run_script_gives_containers_what_they_need() -> None:
    text = RUN_SCRIPT.read_text()
    for option in (
        "--network host",
        '--user "$(id -u):$(id -g)"',
        "--stop-timeout 30",
        "--restart unless-stopped",
        "--device /dev/dri",
        "--group-add",
        '--shm-size "$shm"',
        "/tmp/.X11-unix:/tmp/.X11-unix:ro",
        "PULSE_SERVER=unix:/run/host/pulse/native",
        "QT_QPA_PLATFORM=wayland",
    ):
        assert option in text, option
    assert 'docker exec "${tty[@]}" --user "$(id -u):$(id -g)"' in text
    launcher = (ROOT / "neurowatch.sh").read_text()
    assert 'docker) shift; exec "$ROOT/scripts/docker.sh" "$@" ;;' in launcher


def test_readme_gives_docker_run_commands_for_both_images_and_gpus() -> None:
    readme = (ROOT / "README.md").read_text()
    section = readme[readme.index("## Running in Docker") : readme.index("## Troubleshooting")]
    assert "ghcr.io/ttheekw/neurowatch:latest" in section and "ghcr.io/ttheekw/neurowatch:gui" in section
    assert section.count("docker run -d --name") == 2
    assert '--device /dev/dri --group-add "$(stat -c %g /dev/dri/renderD128)"' in section
    assert "`--gpus all`" in section


def test_release_builds_smoke_tests_and_publishes_both_images(workflow: str) -> None:
    jobs = _jobs(workflow)
    docker = jobs["docker"]
    assert "needs: [version, package-linux]" in docker
    assert "variant: [headless, gui]" in docker
    targets = dict(re.findall(r"- arch: (\S+)\n\s+runner: (\S+)", docker))
    assert targets == {"x86_64": "ubuntu-24.04", "arm64": "ubuntu-24.04-arm"}
    assert "name: release-linux-${{ matrix.arch }}-free" in docker  # free edition only
    assert "scripts/docker.sh build" in docker and "scripts/docker_smoke.sh" in docker
    assert "Xvfb :99" in docker and "if: matrix.variant == 'gui'" in docker
    assert "NW_EXPECT_EDITION: free" in docker
    push = docker[docker.index("name: Push the image") :]
    assert "startsWith(github.ref, 'refs/tags/v')" in push.split("run:", 1)[0]
    # The release waits for the image checks; the public tags follow it.
    assert "install-linux, docker]" in jobs["publish"]
    publish = jobs["publish-docker"]
    assert "needs: [version, publish]" in publish
    assert re.search(r"^    if: .*startsWith\(github\.ref, 'refs/tags/v'\)", publish, re.MULTILINE)
    for tag in (':latest"', ':headless"', ':gui"', ':$version"', ':$version-gui"'):
        assert tag in publish, tag
    writers = [name for name, job in jobs.items() if "packages: write" in job]
    assert sorted(writers) == ["docker", "publish-docker"]
