"""Static contract tests for release packaging; nothing is built or published."""

from __future__ import annotations

import re
import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github/workflows"
RELEASE_WORKFLOW = WORKFLOWS / "release.yml"
LINUX_SCRIPT = ROOT / "scripts/package_release_linux.sh"
WINDOWS_SCRIPT = ROOT / "scripts/package_windows.ps1"
QUALIFICATION_DOC = ROOT / "docs/release-qualification.md"


def _block(text: str, key: str, indent: int) -> str:
    """Return the lines nested under ``key:`` at ``indent`` spaces (YAML is not a dependency)."""
    lines = text.splitlines()
    prefix = " " * indent + key + ":"
    for index, line in enumerate(lines):
        if line == prefix or line.startswith(prefix + " "):
            body = [line]
            for child in lines[index + 1 :]:
                stripped = child.strip()
                if stripped and not stripped.startswith("#"):
                    if len(child) - len(child.lstrip(" ")) <= indent:
                        break
                body.append(child)
            return "\n".join(body)
    raise AssertionError(f"{key!r} not found at indent {indent}")


def _jobs(text: str) -> dict[str, str]:
    jobs = _block(text, "jobs", 0)
    names = re.findall(r"^  ([A-Za-z0-9_-]+):$", jobs, re.MULTILINE)
    return {name: _block(jobs, name, 2) for name in names}


@pytest.fixture(scope="module")
def workflow() -> str:
    return RELEASE_WORKFLOW.read_text()


def _directly_invoked_shell_scripts(text: str) -> set[str]:
    scripts: set[str] = set()
    for match in re.finditer(r"(?m)^(\s*)run:\s*\|\s*$", text):
        indent = len(match.group(1))
        for line in text[match.end() :].splitlines():
            stripped = line.strip()
            if stripped and len(line) - len(line.lstrip(" ")) <= indent:
                break
            command = stripped.removeprefix("./")
            if command.startswith("scripts/") and command.split()[0].endswith(".sh"):
                scripts.add(command.split()[0])
    return scripts


def _git_index_modes(paths: set[str]) -> dict[str, str]:
    result = subprocess.run(
        ["git", "ls-files", "-s", *sorted(paths)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    modes: dict[str, str] = {}
    for line in result.stdout.splitlines():
        mode, _oid, _stage, path = line.split(maxsplit=3)
        modes[path] = mode
    return modes


def _dry_run(*args: str) -> list[str]:
    result = subprocess.run(
        ["bash", str(LINUX_SCRIPT), "--dry-run", "--out-dir", "/out", *args],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    return result.stdout.split()


# --- workflow trigger ----------------------------------------------------------


def test_release_workflow_does_not_build_on_branch_pushes(workflow: str) -> None:
    trigger = _block(workflow, "on", 0)
    assert "branches:" not in trigger and "pull_request:" not in trigger
    assert "- 'v*'" in trigger
    assert "types: [published]" in trigger
    assert "workflow_dispatch:" in trigger


def test_release_workflow_default_token_is_read_only(workflow: str) -> None:
    assert _block(workflow, "permissions", 0).split() == ["permissions:", "contents:", "read"]
    writers = [name for name, job in _jobs(workflow).items() if "contents: write" in job]
    assert writers == ["publish"]


def test_publish_job_is_guarded_to_tag_refs(workflow: str) -> None:
    publish = _jobs(workflow)["publish"]
    assert re.search(r"^    if: .*startsWith\(github\.ref, 'refs/tags/v'\)", publish, re.MULTILINE)
    assert "needs: [version, package-linux, package-windows, install-linux]" in publish
    assert "github.event_name == 'push' || github.event_name == 'release'" in publish


def test_no_workflow_publishes_releases_on_branch_pushes() -> None:
    for path in sorted(WORKFLOWS.glob("*.y*ml")):
        text = path.read_text()
        trigger = _block(text, "on", 0)
        tag_only = [line.strip() for line in trigger.splitlines()] == [
            "on:",
            "push:",
            "tags:",
            "- 'v*'",
        ]
        for name, job in _jobs(text).items():
            if "gh release" not in job and "action-gh-release" not in job:
                continue
            guarded = re.search(r"^    if: .*refs/tags/", job, re.MULTILINE) is not None
            assert tag_only or guarded, f"{path.name}:{name} can publish from a branch push"


def test_workflow_direct_shell_scripts_are_executable_in_git_index(workflow: str) -> None:
    scripts = _directly_invoked_shell_scripts(workflow)
    assert scripts == {"scripts/package_release_linux.sh"}
    assert _git_index_modes(scripts) == dict.fromkeys(scripts, "100755")


# --- target matrix -------------------------------------------------------------


def test_linux_matrix_uses_native_x86_64_and_arm64_runners(workflow: str) -> None:
    job = _jobs(workflow)["package-linux"]
    targets = dict(re.findall(r"- arch: (\S+)\n\s+runner: (\S+)", job))
    formats = dict(re.findall(r"- arch: (\S+)\n\s+runner: \S+\n\s+formats: (\S+)", job))
    assert set(targets) == {"x86_64", "arm64"}
    assert re.fullmatch(r"ubuntu-[0-9.]+", targets["x86_64"])
    assert re.fullmatch(r"ubuntu-[0-9.]+-arm", targets["arm64"])
    # PySide6 manylinux aarch64 wheels require glibc 2.39 (Ubuntu 24.04).
    assert targets["arm64"] == "ubuntu-24.04-arm"
    assert "runs-on: ${{ matrix.runner }}" in job
    assert "scripts/package_release_linux.sh" in job
    assert '--arch "${{ matrix.arch }}"' in job
    assert '--format "${{ matrix.formats }}"' in job
    assert formats == {"x86_64": "portable,deb,rpm,arch", "arm64": "portable,deb,rpm"}
    assert "qemu" not in workflow.lower()


def test_windows_job_builds_x86_64_zip(workflow: str) -> None:
    job = _jobs(workflow)["package-windows"]
    assert re.search(r"runs-on: windows-\d+", job)
    assert "scripts/package_windows.ps1" in job
    assert "msvc-x86_64" in job


def test_official_builds_add_the_cloud_only_with_its_secret(workflow: str) -> None:
    # Forks have no NEUROWATCH_CLOUD_TOKEN and build the open-source app.
    for name in ("package-linux", "package-windows"):
        job = _jobs(workflow)[name]
        assert "OFFICIAL_BUILD: ${{ secrets.NEUROWATCH_CLOUD_TOKEN != '' }}" in job
        for step in (
            "Check out NeuroWatch Cloud (official builds)",
            "Build with NeuroWatch Cloud (official builds)",
        ):
            following = job[job.index(step) :].splitlines()[1].strip()
            assert following == "if: env.OFFICIAL_BUILD == 'true'", f"{name}: {step}"
        assert "NW_BACKEND_PACKAGE=nw-backend-cloud" in job
        assert "NW_UI_EXTENSION_MODULE=neurowatch_cloud" in job


def test_publish_gathers_artifacts_checksums_and_uploads(workflow: str) -> None:
    job = _jobs(workflow)["publish"]
    assert "actions/download-artifact@" in job
    assert "pattern: release-*" in job and "merge-multiple: true" in job
    assert "sha256sum" in job
    assert "gh release create" in job and "--verify-tag" in job
    assert job.index("SHA256SUMS") < job.index("gh release create")
    for name in ("package-linux", "package-windows"):
        assert "name: release-" in _jobs(workflow)[name]


# --- package filenames and metadata --------------------------------------------


def test_linux_artifact_names_for_x86_64() -> None:
    assert _dry_run(
        "--version", "v1.4.0", "--arch", "x86_64", "--format", "deb,rpm,arch,portable"
    ) == [
        "/out/neurowatch_1.4.0-1_amd64.deb",
        "/out/neurowatch-1.4.0-1.x86_64.rpm",
        "/out/neurowatch-1.4.0-1-x86_64.pkg.tar.zst",
        "/out/neurowatch-1.4.0-linux-x86_64.tar.gz",
    ]


def test_linux_artifact_names_for_arm64_prerelease() -> None:
    assert _dry_run(
        "--version", "v2.0.0-rc.1", "--arch", "aarch64", "--format", "deb", "--format", "rpm"
    ) == [
        "/out/neurowatch_2.0.0~rc.1-1_arm64.deb",
        "/out/neurowatch-2.0.0~rc.1-1.aarch64.rpm",
    ]


@pytest.mark.parametrize(
    "args",
    [
        ("--arch", "x86_64", "--format", "deb"),
        ("--version", "1.0.0", "--format", "deb"),
        ("--version", "1.0.0", "--arch", "x86_64"),
        ("--version", "1.0", "--arch", "x86_64", "--format", "deb"),
        ("--version", "1.0.0", "--arch", "riscv64", "--format", "deb"),
        ("--version", "1.0.0", "--arch", "x86_64", "--format", "snap"),
        # Arch Linux is x86_64 only.
        ("--version", "1.0.0", "--arch", "arm64", "--format", "arch"),
    ],
)
def test_linux_script_rejects_invalid_arguments(args: tuple[str, ...]) -> None:
    result = subprocess.run(
        ["bash", str(LINUX_SCRIPT), "--dry-run", *args],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode != 0


def test_linux_script_is_valid_bash() -> None:
    subprocess.run(["bash", "-n", str(LINUX_SCRIPT)], check=True)


def test_linux_script_declares_distro_runtime_dependencies() -> None:
    text = LINUX_SCRIPT.read_text()
    for dep in ("libsoup-3.0-0", "libva2", "libxcb-cursor0"):
        assert dep in text
    for dep in ("libsoup3", "libva", "xcb-util-cursor"):
        assert dep in text
    assert 'bash "$ROOT/scripts/build_release_gstreamer.sh"' in text
    assert '"$BUNDLE_DIR/gstreamer/lib/"' in text
    assert "Depends: $(join_by ', ' \"${DEB_DEPENDS[@]}\")" in text
    assert "Requires:       $(join_by ' ' \"${RPM_REQUIRES[@]}\")" in text
    assert 'echo "depend = $dep"' in text


def test_linux_script_builds_from_root_pyproject_with_pyinstaller() -> None:
    text = LINUX_SCRIPT.read_text()
    assert "--locked --no-dev --no-emit-project" in text
    assert "pip install --require-hashes" in text
    assert 'pip install --no-deps "$ROOT"' in text
    assert "--collect-data neurowatch" in text
    assert "--copy-metadata neurowatch" in text
    assert "cargo build" in text and '-p "${NW_BACKEND_PACKAGE:-nw-backend}"' in text
    assert 'BACKEND_MANIFEST="${NW_BACKEND_MANIFEST:-$ROOT/Cargo.toml}"' in text
    # A UI extension is bundled with its entry-point metadata, so it is found.
    assert '--copy-metadata "$NW_UI_EXTENSION_DIST"' in text
    assert '--collect-submodules "$NW_UI_EXTENSION_MODULE"' in text
    assert "packages are built natively" in text


def test_linux_packages_install_launcher_desktop_file_and_backend() -> None:
    text = LINUX_SCRIPT.read_text()
    assert 'INSTALL_PREFIX="/opt/neurowatch"' in text
    assert 'ln -s "$INSTALL_PREFIX/neurowatch" "$STAGE/usr/bin/neurowatch"' in text
    assert "/usr/share/applications/neurowatch.desktop" in text
    assert "[Desktop Entry]" in text and "Exec=neurowatch" in text
    assert 'install -m 0755 "$BACKEND_TARGET/release/nw-backend" "$BUNDLE_DIR/bin/nw-backend"' in text


def test_linux_launcher_opens_the_ui_and_leaves_the_service_running() -> None:
    # The backend is a background service the UI starts (neurowatch.service):
    # it keeps recording after the UI closes, so the launcher never stops it.
    text = LINUX_SCRIPT.read_text()
    launcher = text[text.index("<<'LAUNCH'") : text.index("\nLAUNCH\n")]
    assert 'exec "$BASE/app/neurowatch-ui/neurowatch-ui" "$@"' in launcher
    assert "kill" not in launcher and 'nw-backend" &' not in launcher


# --- Windows -------------------------------------------------------------------


def test_windows_script_installs_root_project_not_ui_subdir() -> None:
    text = WINDOWS_SCRIPT.read_text()
    assert "'--no-deps', $Root)" in text
    assert "'--require-hashes'" in text
    assert "$Root/ui" not in text
    assert "--collect-data', 'neurowatch'" in text


def test_windows_script_bundles_gstreamer_runtime() -> None:
    text = WINDOWS_SCRIPT.read_text()
    assert "GSTREAMER_1_0_ROOT_MSVC_X86_64" in text
    assert "lib/gstreamer-1.0/*.dll" in text
    assert "gst-plugin-scanner.exe" in text


def test_the_service_runs_the_bundled_gstreamer(tmp_path, monkeypatch) -> None:
    # The tray may start the service at login without the launcher, so the
    # service manager (not the launcher) points the backend at the bundle's
    # GStreamer runtime.
    from neurowatch.service.manager import bundled_gstreamer_env

    (tmp_path / "gstreamer" / "bin").mkdir(parents=True)
    env = bundled_gstreamer_env(tmp_path)
    assert env["GST_PLUGIN_SYSTEM_PATH_1_0"].endswith("gstreamer-1.0")
    assert env["GST_PLUGIN_SCANNER_1_0"].startswith(str(tmp_path / "gstreamer" / "libexec"))


def test_linux_bundle_ignores_pyinstaller_libraries_and_uses_writable_cache(tmp_path, monkeypatch):
    from neurowatch.service import manager

    monkeypatch.setattr(manager, "IS_WINDOWS", False)
    monkeypatch.setenv("LD_LIBRARY_PATH", "/tmp/pyinstaller/_internal")
    monkeypatch.setenv("GST_PLUGIN_PATH_1_0", "/usr/lib/old-gstreamer-plugins")
    cache = tmp_path / "user-cache"
    monkeypatch.setenv("XDG_CACHE_HOME", str(cache))
    bundle = tmp_path / "read-only-install"
    (bundle / "gstreamer").mkdir(parents=True)
    env = manager.bundled_gstreamer_env(bundle)
    assert env["LD_LIBRARY_PATH"] == str(bundle / "gstreamer/lib")
    assert env["GST_PLUGIN_PATH_1_0"] == str(bundle / "gstreamer/lib/gstreamer-1.0")
    assert Path(env["GST_REGISTRY_1_0"]).is_relative_to(cache)


def test_windows_bundle_uses_app_local_runtime_and_user_cache(tmp_path, monkeypatch):
    from neurowatch.service import manager

    monkeypatch.setattr(manager, "IS_WINDOWS", True)
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path / "user-data"))
    (tmp_path / "gstreamer").mkdir()
    env = manager.bundled_gstreamer_env(tmp_path)
    assert "LD_LIBRARY_PATH" not in env
    assert env["PATH"].startswith(str(tmp_path / "gstreamer/bin"))
    assert env["GST_PLUGIN_SCANNER_1_0"].endswith(".exe")
    assert env["GIO_MODULE_DIR"] == str(tmp_path / "gstreamer/lib/gio/modules")
    assert Path(env["GST_REGISTRY_1_0"]).is_relative_to(tmp_path / "user-data")


def test_release_requires_install_checks(workflow):
    jobs = _jobs(workflow)
    assert "scripts/smoke_release.py" in jobs["package-linux"]
    assert "scripts/smoke_windows_installer.ps1" in jobs["package-windows"]
    assert "scripts/smoke_linux_package.sh" in jobs["install-linux"]
    assert "dist/release/*-setup.exe" in jobs["package-windows"]


def test_production_qualification_is_tied_to_exact_artifacts() -> None:
    releasing = (ROOT / "docs/releasing.md").read_text()
    qualification = QUALIFICATION_DOC.read_text()
    assert "release-qualification.md" in releasing
    for requirement in (
        "Commit SHA",
        "SHA256SUMS",
        "24-hour soak",
        "VA-API",
        "NVDEC",
        "Open files or handles",
        "Closing the UI leaves the service running and recording",
        "Upgrade from the previous production version",
    ):
        assert requirement in qualification


def test_every_linux_package_format_has_native_install_coverage(workflow: str) -> None:
    install = _jobs(workflow)["install-linux"]
    entries = {
        (image, package_format, arch, runner)
        for image, package_format, arch, runner in re.findall(
            r"\{image: '([^']+)', format: (\w+), arch: (\w+), runner: ([^}]+)\}",
            install,
        )
    }
    for arch, runner in (("x86_64", "ubuntu-24.04"), ("arm64", "ubuntu-24.04-arm")):
        assert ("fedora:43", "rpm", arch, runner) in entries
        assert any(
            package_format == "deb" and entry_arch == arch and entry_runner == runner
            for _image, package_format, entry_arch, entry_runner in entries
        )
    # Arch Linux is x86_64 only; the official image has no arm64 variant.
    assert ("archlinux:base", "arch", "x86_64", "ubuntu-24.04") in entries
    assert not any(fmt == "arch" and arch == "arm64" for _i, fmt, arch, _r in entries)


def test_windows_installer_stops_service_and_preserves_user_data():
    script = (ROOT / "scripts/installer/windows.iss").read_text()
    assert "PrivilegesRequired=lowest" in script
    assert "RunServiceCommand('stop')" in script
    assert "RunServiceCommand('uninstall')" in script
    assert "[UninstallDelete]" not in script
    assert "{localappdata}\\Programs\\NeuroWatch" in script


def test_windows_launcher_opens_the_ui_and_leaves_the_service_running() -> None:
    text = WINDOWS_SCRIPT.read_text()
    launcher = text[text.index("$Launcher = @'") : text.index("'@\nSet-Content")]
    assert "app\\neurowatch-ui\\neurowatch-ui.exe" in launcher
    assert "nw-backend.exe') -WindowStyle" not in launcher and "Kill()" not in launcher
    assert "neurowatch-$Version-windows-x86_64.zip" in text


@pytest.mark.skipif(shutil.which("pwsh") is None, reason="pwsh not installed")
def test_windows_script_parses() -> None:
    command = (
        "$errors = $null; "
        f"[void][System.Management.Automation.Language.Parser]::ParseFile('{WINDOWS_SCRIPT}', "
        "[ref]$null, [ref]$errors); if ($errors) { $errors; exit 1 }"
    )
    subprocess.run(["pwsh", "-NoProfile", "-Command", command], check=True)
