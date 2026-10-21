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


def test_release_builds_include_the_cloud_with_its_address(workflow: str) -> None:
    # The cloud is part of this repository: no private checkout. Its address
    # is fixed in the backend; a build takes no cloud setting.
    assert "NEUROWATCH_CLOUD_TOKEN" not in workflow
    assert "NEUROWATCH_CLOUD_URL" not in workflow and "NW_CLOUD_ENDPOINT" not in workflow
    link = (ROOT / "crates/nw-backend/src/cloud/link/mod.rs").read_text()
    assert 'pub const NEUROWATCH_CLOUD: &str = "https://neurowatch.app";' in link
    linux = _jobs(workflow)["package-linux"]
    assert "llvm.sh 21" in linux and "CXX=clang++-21" in linux


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


def test_linux_release_builder_installs_the_qt_xcb_dependencies(workflow: str) -> None:
    linux = _jobs(workflow)["package-linux"]
    install = linux.split("- name: Install build dependencies", 1)[1].split("- name:", 1)[0]
    for package in (
        "libxkbcommon-x11-0",
        "libxcb-cursor0",
        "libxcb-image0",
        "libxcb-icccm4",
        "libxcb-keysyms1",
        "libxcb-shape0",
        "libxcb-util1",
        "libxcb-render-util0",
        "libxcb-xkb1",
    ):
        assert package in install, f"Qt X11 dependency missing from the release builder: {package}"


@pytest.mark.parametrize(
    ("array", "dependencies"),
    [
        (
            "DEB_DEPENDS",
            {
                "libxkbcommon-x11-0",
                "libxcb-cursor0",
                "libxcb-image0",
                "libxcb-icccm4",
                "libxcb-keysyms1",
                "libxcb-shape0",
                "libxcb-util1",
                "libxcb-render-util0",
                "libxcb-xkb1",
            },
        ),
        (
            "RPM_REQUIRES",
            {
                "libxkbcommon-x11",
                "xcb-util-cursor",
                "xcb-util-image",
                "xcb-util-wm",
                "xcb-util-keysyms",
                "xcb-util-renderutil",
                "xcb-util",
                "libxcb",
            },
        ),
        (
            "ARCH_DEPENDS",
            {
                "libxkbcommon-x11",
                "xcb-util-cursor",
                "xcb-util-image",
                "xcb-util-wm",
                "xcb-util-keysyms",
                "xcb-util-renderutil",
                "xcb-util",
                "libxcb",
            },
        ),
    ],
)
def test_native_packages_declare_qt_xcb_runtime_libraries(
    array: str, dependencies: set[str]
) -> None:
    match = re.search(rf"^{array}=\((.*?)^\)", LINUX_SCRIPT.read_text(), re.MULTILINE | re.DOTALL)
    assert match is not None
    assert dependencies <= set(match[1].split())


def test_linux_script_builds_from_root_pyproject_with_pyinstaller() -> None:
    text = LINUX_SCRIPT.read_text()
    assert "--locked --no-dev --no-emit-project" in text
    assert "pip install --require-hashes" in text
    assert 'pip install --no-deps "$ROOT"' in text
    assert "--collect-data neurowatch" in text
    assert "--copy-metadata neurowatch" in text
    assert "cargo build" in text and '-p "${NW_BACKEND_PACKAGE:-nw-backend}"' in text
    assert 'BACKEND_MANIFEST="${NW_BACKEND_MANIFEST:-$ROOT/Cargo.toml}"' in text
    # The cloud's UI is bundled; its entry point is in the neurowatch metadata.
    # The cloud's UI modules, some imported only when used.
    assert "--collect-submodules neurowatch.cloud" in text
    assert "NW_UI_EXTENSION" not in text and "NW_EXTRA_LICENSE_FILE" not in text
    assert "packages are built natively" in text


def test_linux_packages_install_launcher_desktop_file_and_backend() -> None:
    text = LINUX_SCRIPT.read_text()
    assert 'INSTALL_PREFIX="/opt/neurowatch"' in text
    assert 'ln -s "$INSTALL_PREFIX/neurowatch" "$STAGE/usr/bin/neurowatch"' in text
    assert "/usr/share/applications/neurowatch.desktop" in text
    assert "[Desktop Entry]" in text and "Exec=neurowatch" in text
    install = 'install -m 0755 "$BACKEND_TARGET/release/nw-backend" "$BUNDLE_DIR/bin/nw-backend"'
    assert install in text


def test_deb_prefers_the_real_alsa_library() -> None:
    # Ubuntu 24.04+ / Debian 13 satisfy a bare "libasound2" with a stub
    # (liboss4-salsa-asound2) that GStreamer's ALSA plugin cannot load.
    text = LINUX_SCRIPT.read_text()
    assert "'libasound2t64 | libasound2'" in text
    assert "'libasound2 | libasound2t64'" not in text


def test_linux_packages_carry_the_logo_and_software_centre_description() -> None:
    """Every Linux package (deb, rpm, Arch) installs the NeuroWatch logo and the
    AppStream file software centres show when the package is double-clicked."""
    text = LINUX_SCRIPT.read_text()
    assert "Icon=neurowatch\n" in text and "Name=NeuroWatch\n" in text
    linux = ROOT / "scripts/installer/linux"
    sizes = (48, 64, 128, 256)
    for size in sizes:
        assert (linux / f"icons/neurowatch-{size}.png").read_bytes()[:4] == b"\x89PNG"
    assert "for size in 48 64 128 256; do" in text
    assert '"$STAGE$ICON_DIR/scalable/apps/neurowatch.svg"' in text
    assert 'APP_ID="io.github.ttheekw.neurowatch"' in text
    metainfo = (linux / "neurowatch.metainfo.xml").read_text()
    assert "<id>io.github.ttheekw.neurowatch</id>" in metainfo
    assert '<launchable type="desktop-id">neurowatch.desktop</launchable>' in metainfo
    assert '<icon type="stock">neurowatch</icon>' in metainfo
    # The build fills in its own licence and release.
    assert "<project_license>$PKG_LICENSE</project_license>" in text
    assert '<release version=\\"$VERSION\\" date=\\"$RELEASE_DATE\\"/>' in text
    # The rpm lists its files; deb and Arch package the whole stage.
    rpm = text[text.index("%files") : text.index("SPEC\n", text.index("%files"))]
    for size in sizes:
        assert f"$ICON_DIR/{size}x{size}/apps/neurowatch.png" in rpm
    assert "$ICON_DIR/scalable/apps/neurowatch.svg" in rpm and "$METAINFO" in rpm


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


def test_windows_installer_code_section_has_no_ini_style_comments() -> None:
    # Inside [Code] (Pascal), a line starting with ";" is a syntax error
    # ("'BEGIN' expected"); comments there are // or { }.
    script = (ROOT / "scripts/installer/windows.iss").read_text()
    code = script[script.index("[Code]") :]
    assert not [line for line in code.splitlines() if line.lstrip().startswith(";")]


def test_windows_exe_and_installer_carry_the_app_logo() -> None:
    icon = ROOT / "ui/neurowatch/resources/icons/neurowatch.ico"
    assert icon.read_bytes()[:4] == b"\x00\x00\x01\x00", "an ICO file"
    script = WINDOWS_SCRIPT.read_text()
    assert "$AppIcon = Join-Path $Root 'ui/neurowatch/resources/icons/neurowatch.ico'" in script
    assert "'--icon', $AppIcon," in script  # the .exe: shortcuts, Add/Remove Programs
    assert '"/DAppIcon=$AppIcon",' in script
    installer = (ROOT / "scripts/installer/windows.iss").read_text()
    assert "SetupIconFile={#AppIcon}" in installer
    assert "UninstallDisplayIcon={app}\\app\\neurowatch-ui\\neurowatch-ui.exe" in installer
    # The taskbar groups the running app under its shortcut (main.WINDOWS_APP_ID).
    shortcuts = [
        line for line in installer.splitlines() if 'neurowatch-ui.exe"; WorkingDir' in line
    ]
    assert shortcuts and all('AppUserModelID: "NeuroWatch.NeuroWatch"' in s for s in shortcuts)
    main = (ROOT / "ui/neurowatch/main.py").read_text()
    assert 'WINDOWS_APP_ID = "NeuroWatch.NeuroWatch"' in main


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


def test_the_bundled_nettle_3_reaches_the_package() -> None:
    # librtmp's nettle 3 is built into lib/nettle3, off the default library
    # path; the package must carry that folder, or RTMP fails on Arch.
    build = (ROOT / "scripts/build_release_gstreamer.sh").read_text()
    package = (ROOT / "scripts/package_release_linux.sh").read_text()
    assert 'cp -L "$dep" "$PREFIX/lib/nettle3/$(basename "$dep")"' in build
    assert 'cp -a "$WORK_DIR/gstreamer-sdk/lib/nettle3" "$BUNDLE_DIR/gstreamer/lib/"' in package


def test_smoke_check_requires_a_working_cloud_in_official_builds() -> None:
    # A fresh install once switched the cloud off, and Windows (TCP) never
    # served it: every bundle must answer cloud.status with the user's token,
    # with the cloud on and linking to NeuroWatch Cloud.
    smoke = (ROOT / "scripts/smoke_release.py").read_text()
    assert "LICENSE-EXTENSIONS" not in smoke
    assert "check_cloud(port, data_dirs)" in smoke
    assert 'NEUROWATCH_CLOUD = "https://neurowatch.app"' in smoke
    assert 'rpc(port, "cloud.status", auth=ipc_token(port, data_dirs))' in smoke
    assert 'assert status.get("cloud_configured") is True, status' in smoke


def test_readme_download_links_open_the_downloads_page_at_a_published_package(
    workflow: str,
) -> None:
    # Every download link opens https://neurowatch.app/downloads (free and
    # Unlimited) at a package the manifest describes; none bypasses it.
    import importlib.util

    spec = importlib.util.spec_from_file_location(
        "release_manifest", ROOT / "scripts/release_manifest.py"
    )
    assert spec and spec.loader
    manifest = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(manifest)
    packages = {(platform, arch, kind) for _, platform, arch, kind, _ in manifest._PACKAGES}

    readme = (ROOT / "README.md").read_text()
    assert "releases/latest/download/" not in readme
    linked = set(
        re.findall(
            r"https://neurowatch\.app/downloads\?version=latest"
            r"&platform=(\w+)&arch=(\w+)&kind=(\w+)",
            readme,
        )
    )
    assert linked and linked <= packages, linked - packages
    assert ("windows", "x86_64", "installer") in linked
    assert ("linux", "x86_64", "deb") in linked
    # Version-free copies stay on GitHub for older links; the release stays Latest.
    job = workflow[workflow.index("  publish:") :]
    assert '--notes "' in job and "neurowatch.app/downloads?version=$RELEASE_TAG" in job
    assert "--latest" in job and "--prerelease" not in job
    assert job.index("Add version-free download names") < job.index("Generate SHA256SUMS")


def test_linux_packages_give_users_the_graphics_card_from_boot() -> None:
    # A recorder started at boot (before sign-in) decodes on the CPU unless
    # its user is in `render`; every native package adds them, and removal
    # takes back only what it added.
    script = (ROOT / "scripts/package_release_linux.sh").read_text()
    access = (ROOT / "scripts/installer/linux/gpu-access.sh").read_text()
    assert 'GPU_ACCESS="$(cat "$LINUX_FILES/gpu-access.sh")"' in script
    assert "configure) neurowatch_grant_gpu_access" in script
    assert "remove|purge) neurowatch_revoke_gpu_access" in script
    assert '"$root/DEBIAN/postinst"' in script and '"$root/DEBIAN/postrm"' in script
    assert "%post\n$GPU_ACCESS\nneurowatch_grant_gpu_access" in script
    assert "%postun\n$GPU_ACCESS" in script and '[ "\\$1" -eq 0 ]' in script
    assert "post_install() {" in script and "post_remove() {" in script
    assert ".PKGINFO .INSTALL .MTREE opt usr" in script
    assert 'NEUROWATCH_GPU_GROUPS="render video"' in access
    assert 'usermod -aG "$group" "$user"' in access and 'gpasswd -d "$user" "$group"' in access
    assert "/var/lib/neurowatch/device-group-users" in access
    result = subprocess.run(["sh", "-n", str(ROOT / "scripts/installer/linux/gpu-access.sh")])
    assert result.returncode == 0


def test_every_package_is_built_free_and_unlimited(workflow: str) -> None:
    # Free builds (16 cameras) go to GitHub Releases; unlimited ones only to
    # the private downloads bucket, never to the public release.
    jobs = _jobs(workflow)
    for name in ("package-linux", "package-windows"):
        assert "edition: [free, unlimited]" in jobs[name]
    assert '--edition "${{ matrix.edition }}"' in jobs["package-linux"]
    assert '-Edition "${{ matrix.edition }}"' in jobs["package-windows"]
    assert "pattern: release-*-free" in jobs["publish"]
    assert "release-*-unlimited" not in jobs["publish"]
    downloads = jobs["publish-downloads"]
    assert "pattern: release-*-unlimited" not in downloads
    assert "Gather the privately staged Unlimited packages" in downloads
    for name in ("package-linux", "package-windows"):
        job = jobs[name]
        upload = job[job.index("- uses: actions/upload-artifact@") :]
        assert "if: matrix.edition == 'free'" in upload.split("with:", 1)[0]
        assert "Stage Unlimited packages privately" in job
        assert '"s3://$S3_BUCKET/staging/$GITHUB_RUN_ID/$GITHUB_RUN_ATTEMPT/unlimited/"' in job
        assert "secrets.DOWNLOADS_S3_SECRET_ACCESS_KEY" in job
    assert "secrets.DOWNLOADS_S3_BUCKET" in downloads
    assert "scripts/release_manifest.py" in downloads
    order = [
        downloads.index('"$bucket/$RELEASE_TAG/unlimited/"'),
        downloads.index('"$bucket/$RELEASE_TAG/manifest.json"'),
        downloads.index('dist/index.json "$bucket/index.json"'),
    ]
    assert order == sorted(order), "the index is uploaded last"
    linux = (ROOT / "scripts/package_release_linux.sh").read_text()
    assert "unlimited) BACKEND_FEATURES=(--features nw-backend/unlimited)" in linux
    windows = (ROOT / "scripts/package_windows.ps1").read_text()
    assert "$CargoArgs += @('--features', 'nw-backend/unlimited')" in windows
    assert workflow.count("NW_EXPECT_EDITION: ${{ matrix.edition }}") == 2
    smoke = (ROOT / "scripts/smoke_release.py").read_text()
    assert 'assert status.get("edition") == expected_edition, status' in smoke
    cargo = (ROOT / "crates/nw-backend/Cargo.toml").read_text()
    assert "\nunlimited = []" in cargo


@pytest.mark.parametrize("job_name", ["package-linux", "package-windows"])
@pytest.mark.parametrize(
    ("repository", "configured", "expected_code", "uploaded"),
    [
        ("ttheekw/Neurowatch", False, 1, False),
        ("someone/fork", False, 0, False),
        ("ttheekw/Neurowatch", True, 0, True),
    ],
)
def test_unlimited_staging_uses_private_storage_or_refuses_to_publish(
    workflow: str,
    tmp_path: Path,
    job_name: str,
    repository: str,
    configured: bool,
    expected_code: int,
    uploaded: bool,
) -> None:
    import os
    import textwrap

    job = _jobs(workflow)[job_name]
    step = job.split("- name: Stage Unlimited packages privately", 1)[1]
    script = textwrap.dedent(step.split("        run: |\n", 1)[1])
    fake_aws = tmp_path / "aws"
    fake_aws.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$NW_TEST_AWS_LOG"\n')
    fake_aws.chmod(0o755)
    recorded = tmp_path / "aws-arguments"
    result = subprocess.run(
        ["bash", "-c", script],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        env={
            "PATH": f"{tmp_path}{os.pathsep}{os.defpath}",
            "GITHUB_REPOSITORY": repository,
            "GITHUB_RUN_ID": "123",
            "GITHUB_RUN_ATTEMPT": "2",
            "AWS_ACCESS_KEY_ID": "example" if configured else "",
            "AWS_SECRET_ACCESS_KEY": "example" if configured else "",
            "S3_BUCKET": "private-downloads" if configured else "",
            "S3_ENDPOINT": "https://objects.example.test",
            "NW_TEST_AWS_LOG": str(recorded),
        },
    )
    assert result.returncode == expected_code, result.stderr
    assert recorded.exists() is uploaded
    if uploaded:
        arguments = recorded.read_text().splitlines()
        assert arguments[-1] == "s3://private-downloads/staging/123/2/unlimited/"
        assert arguments[:2] == ["--endpoint-url", "https://objects.example.test"]


def test_the_release_manifest_lists_both_editions(tmp_path: Path) -> None:
    import importlib.util
    import json

    spec = importlib.util.spec_from_file_location(
        "release_manifest", ROOT / "scripts/release_manifest.py"
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    free, unlimited = tmp_path / "free", tmp_path / "unlimited"
    for folder in (free, unlimited):
        folder.mkdir()
        for name in (
            "neurowatch-v1.2.3-windows-x86_64-setup.exe",
            "neurowatch_1.2.3_amd64.deb",
            "neurowatch-1.2.3-1.aarch64.rpm",
            "neurowatch-1.2.3-linux-arm64.tar.gz",
            "neurowatch_amd64.deb",  # a version-free README copy: not listed
            "SHA256SUMS",
        ):
            (folder / name).write_bytes(name.encode())
    out = tmp_path / "manifest.json"
    assert (
        module.main(
            [
                "--version",
                "v1.2.3",
                "--repo",
                "ttheekw/Neurowatch",
                "--free",
                str(free),
                "--unlimited",
                str(unlimited),
                "--out",
                str(out),
            ]
        )
        == 0
    )
    data = json.loads(out.read_text())
    assert data["version"] == "v1.2.3" and data["prerelease"] is False
    names = {entry["name"] for entry in data["free"]}
    assert "neurowatch_amd64.deb" not in names and "SHA256SUMS" not in names
    assert len(data["free"]) == len(data["unlimited"]) == 4
    deb = next(e for e in data["free"] if e["kind"] == "deb")
    assert deb["url"] == (
        "https://github.com/ttheekw/Neurowatch/releases/download/v1.2.3/neurowatch_1.2.3_amd64.deb"
    )
    assert (deb["platform"], deb["arch"]) == ("linux", "x86_64")
    rpm = next(e for e in data["unlimited"] if e["kind"] == "rpm")
    assert rpm["key"] == "releases/v1.2.3/unlimited/neurowatch-1.2.3-1.aarch64.rpm"
    assert "url" not in rpm, "unlimited packages are never public links"
    assert len(rpm["sha256"]) == 64

    # The index keeps every version, newest first; republishing replaces.
    older = {"version": "v1.2.2", "published_at": "2020-01-01T00:00:00Z"}
    index = module.merged_index({"versions": [older, {**data, "free": []}]}, data)
    assert [v["version"] for v in index["versions"]] == ["v1.2.3", "v1.2.2"]
    assert index["versions"][0]["free"] == data["free"]
    assert module.merged_index("garbage", data)["versions"] == [data]
