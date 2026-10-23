# Building and publishing releases

The [release workflow](../.github/workflows/release.yml) produces these downloads:

| Platform | Artifacts |
|---|---|
| Linux x86_64 | `.deb`, `.rpm`, Arch `.pkg.tar.zst`, portable `.tar.gz` |
| Linux arm64 | `.deb`, `.rpm`, portable `.tar.gz` (no Arch package: Arch Linux is x86_64 only) |
| Windows x86_64 | Inno Setup `-setup.exe`, portable `.zip` |
| All | `SHA256SUMS` |
| Docker (x86_64, arm64) | `ghcr.io/<owner>/neurowatch`: headless (`:latest`, `:<version>`) and gui (`:gui`, `:<version>-gui`) images from the free `.deb` ([NeuroWatch in Docker](docker.md)) |

Every package is built twice: **free** (runs up to 16 cameras) and
**unlimited** (the backend's `unlimited` Cargo feature; `--edition` /
`-Edition` of the packaging scripts). Only the free packages are attached to
the GitHub Release. The `publish-downloads` job uploads the unlimited ones to
the private downloads bucket, under `releases/<tag>/unlimited/`, then
`releases/<tag>/manifest.json` (`scripts/release_manifest.py`: both editions;
free entries link to the GitHub Release). The downloads page
(`https://neurowatch.app/downloads`, the web portal) reads the bucket and
gives unlimited packages only to buyers, as short-lived signed links. The
smoke checks assert each package's edition (`NW_EXPECT_EDITION`).

Unlimited packages are never uploaded as public GitHub Actions artifacts.
The build jobs stage them in the same private bucket under
`staging/<run id>/<run attempt>/unlimited/`; the publishing job reads them
there after the build and install checks pass. Keep the bucket private,
give the workflow key read/write access to both `staging/` and `releases/`,
and set a bucket lifecycle rule to expire `staging/` objects after 7 days.
Free packages remain available as workflow artifacts and GitHub Release assets.

The README's download links open the downloads page
(`?version=latest&platform=…&arch=…&kind=…`). Each free download is also
published under a version-free name (for example
`neurowatch-windows-x86_64-setup.exe`), so older
`https://github.com/ttheekw/Neurowatch/releases/latest/download/<name>` links
keep working.

| Repository secret | Value |
|---|---|
| `DOWNLOADS_S3_BUCKET` | Private bucket the downloads page reads (the portal's `NEUROWATCH_DOWNLOADS_S3_BUCKET`) |
| `DOWNLOADS_S3_ENDPOINT` | S3-compatible endpoint, e.g. Cloudflare R2's `https://<account>.r2.cloudflarestorage.com` or Wasabi's `https://s3.<region>.wasabisys.com` (host only, no bucket in the path); empty for AWS |
| `DOWNLOADS_S3_REGION` | Region (`auto` for R2, the default; anything else needs its real region, e.g. `ap-southeast-1`) |
| `DOWNLOADS_S3_ACCESS_KEY_ID`, `DOWNLOADS_S3_SECRET_ACCESS_KEY` | A key that may write to the bucket |

The official repository fails an Unlimited build if the bucket or key pair is
missing. Forks warn and retain no Unlimited artifacts without private storage.

Check the values before setting the secrets: `scripts/check_downloads_bucket.sh`
asks for any that are not in the environment, flags stray spaces, `s3://`
prefixes and a region that does not match the endpoint, checks that the bucket
exists, and lists, writes, reads back and deletes a test object under
`staging/` with the key (`--read-only` for the portal's key). It prints the
values to copy into the secrets. It needs only bash and curl.

Before accepting purchases, deploy the webapp's downloads page and configure
its read-only bucket credentials, live payments.lk key and signing secret
(`../Neurowatch-webapp/docs/integrations/downloads.md`). Publish a release and
verify that Free downloads work signed out and Unlimited downloads require a
confirmed purchase. The advertised desktop offer is USD 50 once, no software
camera limit (capacity depends on hardware), and a support response within
24 hours at `hello@neurowatch.app`. Optional Cloud plans are separate.

Ordinary branch pushes run CI. A `vMAJOR.MINOR.PATCH` tag, optionally followed
by `-rc.1`, runs the release build. No self-hosted runner is required. GitHub
Actions must be enabled, with repository policy allowing the publish job's
`contents: write` permission. Only that job receives write permission.

## First run without publishing

Commit and push the packaging changes. In **Actions → Release → Run workflow**,
select the branch and enter a version such as `v0.1.0-rc.1`. A manual run uploads
Free workflow artifacts and stages Unlimited packages privately; it never
creates or modifies a GitHub Release.

## Official builds and NeuroWatch Cloud

Every build includes the NeuroWatch Cloud client: it is part of the
`nw-backend` binary and of the `neurowatch` UI package. Linux builds install
clang 21, which the cloud's libwebrtc needs.

Every release build links to NeuroWatch Cloud at `https://neurowatch.app`
and nowhere else (`NEUROWATCH_CLOUD` in `crates/nw-backend/src/cloud/link/mod.rs`);
there is no build or run-time setting for it. Only development (debug)
builds read `NW_CLOUD_ENDPOINT`, to link to a development portal.

## Prereleases and stable releases

Release CI runs its install and media checks automatically on every build;
nothing else is required to publish. Manual testing on real hardware is
optional for a prerelease and recommended before a stable release:

| Release | Tag | Manual testing |
|---|---|---|
| Prerelease (beta, release candidate) | `v0.1.0-beta.1`, `v0.1.0-rc.1` | Optional. Users test it; say so in the release notes, e.g. "Not yet tested on real hardware: please report problems in Issues." |
| Stable | `v0.1.0` | Recommended: the [release qualification record](release-qualification.md), on the downloaded artifacts of this exact commit |

Every published release, betas included, is marked **Latest** (not
pre-release): GitHub's "latest" link, which the README's download buttons
use, skips pre-releases. The version and release title still say beta.

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
   the GitHub Release with the downloads attached (and their version-free
   copies), marked Latest.

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
- Both Docker images (headless, gui) are built from the free `.deb` on both
  architectures and run with `scripts/docker_smoke.sh`: healthy backend as a
  non-root user on mounted volumes, the expected mode and edition, the gui
  window on Xvfb, and a clean `docker stop`. The release waits for them;
  `publish-docker` tags the multi-architecture images after it.
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
`scripts/package_windows.ps1 -Version v0.1.0` in an MSVC developer shell. On
Linux set `CXX=clang++-21` first.
