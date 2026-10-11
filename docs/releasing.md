# Building and publishing releases

The [release workflow](../.github/workflows/release.yml) produces these downloads:

| Platform | Artifacts |
|---|---|
| Linux x86_64 | `.deb`, `.rpm`, Arch `.pkg.tar.zst`, portable `.tar.gz` |
| Linux arm64 | `.deb`, `.rpm`, portable `.tar.gz` (no Arch package: Arch Linux is x86_64 only) |
| Windows x86_64 | Inno Setup `-setup.exe`, portable `.zip` |
| All | `SHA256SUMS` |

Ordinary branch pushes run CI. A `vMAJOR.MINOR.PATCH` tag, optionally followed
by `-rc.1`, runs the release build. No self-hosted runner is required. GitHub
Actions must be enabled, with repository policy allowing the publish job's
`contents: write` permission. Only that job receives write permission.

## First run without publishing

Commit and push the packaging changes. In **Actions → Release → Run workflow**,
select the branch and enter a version such as `v0.1.0-rc.1`. A manual run uploads
workflow artifacts and never creates or modifies a GitHub Release.

## Official builds and NeuroWatch Cloud

The downloads on the Releases page are the official builds: the open-source app
plus the proprietary NeuroWatch Cloud extension, from the private
`Neurowatch-cloud` repository. The release workflow adds it when the repository
has:

| Setting | Kind | Value |
|---|---|---|
| `NEUROWATCH_CLOUD_TOKEN` | Actions secret | A fine-grained token with read-only **Contents** access to the private cloud repository |
| `NEUROWATCH_CLOUD_URL` | Actions variable | The NeuroWatch Cloud address built into official builds, e.g. `https://cloud.example.com` |
| `NEUROWATCH_CLOUD_REPOSITORY` | Actions variable (optional) | The cloud repository, if not `ttheekw/Neurowatch-cloud` |

The packaging scripts then build the backend from the cloud workspace
(`NW_BACKEND_MANIFEST`, `NW_BACKEND_PACKAGE`), bundle the UI extension
(`NW_UI_EXTENSION_DIR`, `_DIST`, `_MODULE`), build in the cloud address
(`NW_CLOUD_ENDPOINT`, read at compile time; release builds ignore it at run
time) and ship the cloud's terms as `LICENSE-EXTENSIONS`. Linux official builds
also install clang 21, which the cloud's libwebrtc needs.

A tagged release checks out the cloud repository's tag of the same version, so
tag the cloud repository first (for example `v0.1.0` in both); a manual build
uses its default branch. Without the secret, as in forks, the workflow builds
the open-source app with no cloud. Nothing proprietary is stored in this repository; the cloud code is
fetched only on the build machine, during the release run.

## Prereleases and stable releases

Release CI runs its install and media checks automatically on every build;
nothing else is required to publish. Manual testing on real hardware is
optional for a prerelease and recommended before a stable release:

| Release | Tag | Manual testing |
|---|---|---|
| Prerelease (beta, release candidate) | `v0.1.0-beta.1`, `v0.1.0-rc.1` | Optional. Users test it; say so in the release notes, e.g. "Not yet tested on real hardware: please report problems in Issues." |
| Stable | `v0.1.0` | Recommended: the [release qualification record](release-qualification.md), on the downloaded artifacts of this exact commit |

GitHub marks a tag with a suffix as a prerelease.

The pipeline builds GStreamer and the Rust backend, so allow time for a full
native build on all three hosted runners. The media source downloads are pinned
by SHA-256; Python runtime dependencies come from `uv.lock` and Cargo uses
`--locked`. Updating the lockfiles is a separate deliberate change.

## Publish a version

1. Update the application version in the workspace `Cargo.toml` and
   `pyproject.toml` and `ui/neurowatch/__init__.py` and refresh the corresponding
   lockfiles. GitHub writes the release notes from the merged pull requests;
   edit them on the release page if needed. For a stable release, complete the
   qualification record from the non-publishing workflow on this exact commit
   if you can. The tag sets the artifact filenames; it does not rewrite the
   application version.
2. Merge the release commit, then tag that exact commit:

   ```bash
   git tag -a v0.1.0 -m "NeuroWatch 0.1.0"
   git push origin v0.1.0
   ```

3. The workflow builds and checks all packages, generates checksums and creates
   the GitHub Release with the downloads attached. A tag with a suffix such as
   `v0.1.0-rc.1` creates a prerelease.

Alternatively, publish a release for a `v*` tag through GitHub's Releases UI.
The `release: published` event builds and attaches the same files to that
release. In that flow the release is visible while the builds run; the tag-push
flow publishes only after the checks pass. Branch pushes never publish.

For failures, inspect the failed Actions job. Do not move an already distributed
version tag. Existing asset names are not overwritten; remove a failed draft or
use a new version for changed binaries. Prefer the tag-push flow instead of
manually publishing while its run is in progress.

## What the checks cover

- Both Linux architectures build natively. x86_64 uses Ubuntu 22.04 (glibc 2.35);
  arm64 uses Ubuntu 24.04 (glibc 2.39). Packages declare these minimums and the
  matching GLib floor. A local build on a newer OS can raise the actual floor.
  [System requirements](system-requirements.md) lists the resulting minimum
  version for each distribution and download.
- Portable Linux bundles must start the packaged UI's service command, start a
  backend with zero cameras, answer IPC, load the required media elements,
  decode generated JPEG/H.264/H.265 streams, and stop cleanly.
- Clean containers install and remove the actual packages: Ubuntu 22.04/24.04,
  Debian 12/13, Fedora 43 and Arch (x86_64); Ubuntu 24.04, Debian 13 and
  Fedora 43 (arm64). Every generated Linux package therefore has a native
  install, packaged-service/media, removal and residue check.
- Windows silently installs into a path with spaces, exercises the installed
  service and media runtime with the build SDK removed from `PATH` and temporarily
  renamed on the disposable runner, and uninstalls.
- Checks use temporary configuration and IPC locations, no cameras, and
  `--no-autostart`. Linux data/cache locations are also temporary. Windows checks
  run only on the disposable Actions account because Rust uses Windows Known
  Folders for its database directory.

These are installation/runtime checks, not GPU, monitor, camera or long-running
recording qualification. For a stable release, testing real Windows and Linux
desktops, VA/NVIDIA decoding, page cycling, recording through UI closure,
playback/audio, upgrades and uninstall is recommended (see the table above).
Windows ARM, macOS, Alpine, RHEL and openSUSE are not qualified by this
workflow.

## Bundled media runtime

Linux builds matched GStreamer core/base/good/bad/libav **1.24.13**, including the
fixed `va` plugin, and a minimal LGPL FFmpeg **6.1.5**. See
[`scripts/build_release_gstreamer.sh`](../scripts/build_release_gstreamer.sh) and
[`scripts/packaging/media-sources.sha256`](../scripts/packaging/media-sources.sha256).
The private checkout VA workaround is not used in release builds.

Windows uses the upstream MSVC **1.24.13** runtime/development installers and
ships the runtime DLLs, plugins, GIO TLS modules, scanner and app-local MSVC runtime. The Inno
Setup compiler is available on the Windows hosted runner. Installed builds
select `<bundle>/gstreamer` in the service manager, including when started by
the tray, and keep plugin caches in the user's writable cache directory.

Linux keeps host GLib/GIO (including TLS), graphics drivers, VA driver loading,
audio clients, and other declared system dependencies. It bundles JPEG and
RTMP libraries where distribution package/SONAME differences would otherwise
break compatibility. Notices are included under `gstreamer/share/licenses`.
Keep the source versions, checksums, build recipe, dependency notices and any
required corresponding-source distribution together when updating the runtime.
This pin addresses the diagnosed leak; it is not a claim that 1.24 is the newest
GStreamer branch. Review upstream security updates as part of release maintenance.

## Signing and local builders

The Windows installer is unsigned until an Authenticode signing identity and
CI signing step are configured. Production Windows distribution should sign
the application and installer before generating checksums. No signing secret
is included in this repository. SHA-256 checksums detect file changes; they do
not replace a publisher signature.

For local Linux builds, install the build prerequisites listed in the workflow,
then use `scripts/package_release_linux.sh --version v0.1.0 --arch x86_64
--format portable,deb,rpm,arch` (`arch` is x86_64 only). Build inside the oldest target OS. Windows
requires Python 3.12, Rust/MSVC, the GStreamer MSVC SDK and Inno Setup 6; run
`scripts/package_windows.ps1 -Version v0.1.0` in an MSVC developer shell. To build an
official package locally, set the variables listed under
[Official builds](#official-builds-and-neurowatch-cloud) first (on Linux also
`CXX=clang++-21`).
