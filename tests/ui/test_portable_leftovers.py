from __future__ import annotations

import sys
from importlib.metadata import distributions
from pathlib import Path

from neurowatch.service import leftovers


def _bundle(root: Path, *, portable: bool = True) -> Path:
    """A 0.1.4 portable copy unpacked over 0.1.2: both versions' files are there."""
    internal = root / "app/neurowatch-ui/_internal"
    for version in ("0.1.2", "0.1.4"):
        meta = internal / f"neurowatch-{version}.dist-info"
        meta.mkdir(parents=True)
        metadata = f"Metadata-Version: 2.1\nName: neurowatch\nVersion: {version}\n"
        (meta / "METADATA").write_text(metadata)
    (root / "app/neurowatch-ui/neurowatch-ui").write_text("")
    (root / "bin").mkdir()
    (root / "bin/nw-backend").write_text("")
    (root / "bin/libold.so").write_text("")
    (root / "gstreamer/lib/gstreamer-1.0/old").mkdir(parents=True)
    (root / "gstreamer/lib/gstreamer-1.0/old/libgstold.so").write_text("")
    (root / "gstreamer/lib/kept-empty").mkdir()
    (root / "notes.txt").write_text("the user's own file, outside the program folders")
    (root / leftovers.MANIFEST).write_text(
        "\n".join(
            [
                "app/",
                "app/neurowatch-ui/",
                "app/neurowatch-ui/neurowatch-ui",
                "app/neurowatch-ui/_internal/",
                "app/neurowatch-ui/_internal/neurowatch-0.1.4.dist-info/",
                "app/neurowatch-ui/_internal/neurowatch-0.1.4.dist-info/METADATA",
                "bin/",
                "bin/nw-backend",
                "gstreamer/",
                "gstreamer/lib/",
                "gstreamer/lib/gstreamer-1.0/",
                "gstreamer/lib/kept-empty/",
            ]
        )
        + "\n"
    )
    if portable:
        (root / leftovers.PORTABLE_MARKER).write_text("")
    return root


def test_a_portable_copy_removes_what_the_old_version_left(tmp_path: Path) -> None:
    root = _bundle(tmp_path)
    internal = root / "app/neurowatch-ui/_internal"
    found = sorted(d.version for d in distributions(name="neurowatch", path=[str(internal)]))
    assert found == ["0.1.2", "0.1.4"]  # the window showed 0.1.2

    assert leftovers.clean_portable_copy(root) == 3

    found = [d.version for d in distributions(name="neurowatch", path=[str(internal)])]
    assert found == ["0.1.4"]
    assert not (internal / "neurowatch-0.1.2.dist-info").exists()  # its emptied folder too
    assert not (root / "bin/libold.so").exists()
    assert not (root / "gstreamer/lib/gstreamer-1.0/old").exists()
    assert (root / "bin/nw-backend").exists()
    assert (root / "gstreamer/lib/kept-empty").is_dir()  # listed, though empty
    assert (root / "notes.txt").exists()  # outside the program folders
    assert leftovers.clean_portable_copy(root) == 0  # nothing more on the next start


def test_an_installed_copy_leaves_it_to_its_installer(tmp_path: Path) -> None:
    root = _bundle(tmp_path, portable=False)
    assert leftovers.clean_portable_copy(root) == 0
    assert (root / "bin/libold.so").exists()


def test_a_windows_file_list_matches_whatever_the_case_and_separators(tmp_path: Path) -> None:
    (tmp_path / "app/neurowatch-ui").mkdir(parents=True)
    (tmp_path / "app/neurowatch-ui/Qt6Core.dll").write_text("")
    (tmp_path / "app/neurowatch-ui/old.dll").write_text("")
    (tmp_path / leftovers.MANIFEST).write_text(
        "app\\\r\napp\\neurowatch-ui\\\r\napp\\NEUROWATCH-UI\\qt6core.DLL\r\n"
    )
    assert leftovers.remove_leftovers(tmp_path, windows=True) == 1
    assert (tmp_path / "app/neurowatch-ui/Qt6Core.dll").exists()
    assert not (tmp_path / "app/neurowatch-ui/old.dll").exists()


def test_the_bundle_is_found_from_the_frozen_executable(tmp_path: Path) -> None:
    exe = tmp_path / "neurowatch/app/neurowatch-ui/neurowatch-ui.exe"
    assert leftovers.bundle_root(str(exe)) == (tmp_path / "neurowatch").resolve()
    assert leftovers.bundle_root(str(tmp_path / "python3")) is None
    # A source checkout is not frozen: nothing to clean.
    assert not getattr(sys, "frozen", False)
    assert leftovers.bundle_root() is None
    assert leftovers.clean_portable_copy() == 0
