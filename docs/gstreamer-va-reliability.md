# VA decoder reliability and release qualification

GStreamer 1.24.0–1.24.5's `va` plugin can leak DMA-BUF descriptors while
discovering a surface's DRM modifier. Page cycling, playback segment changes and
thumbnail requests can repeatedly create decoders. Exhaustion can abort GLib and
stop the entire backend, including recording.

The upstream fix closes exported descriptors both after successful modifier
discovery and on export setup failures. It shipped in
[GStreamer 1.24.6](https://gstreamer.freedesktop.org/releases/1.24/#1.24.6).

## Deployment

1. Deploy the backend guard. At startup it loads the `va` plugin, checks its
   version and removes affected factories from the process registry. Both
   `decodebin` and explicit factory selection then exclude them. Local desktop
   live view, native overlays and decoded camera-test previews are blocked until
   the runtime is updated; the status and Diagnostics UI report the reason.
   Recording and compressed browser live view continue because neither creates
   a video decoder. Non-live decoding can use another safe decoder after the
   unsafe plugin has been removed.
2. Restore VA acceleration using a maintained, matched GStreamer runtime that
   contains the upstream fix and reports version 1.24.6 or later. That is the
   minimum for this defect, not a recommendation to deploy an old release with
   other unfixed issues. Use trusted distribution packages, or a reproducible
   private runtime built and distributed with the application. Keep libraries,
   plugins and their dependencies consistent; replacing one installed `.so` or
   intercepting an internal function is not the supported deployment path.
3. Check `gst-inspect-1.0 va` in the service's runtime environment. Its plugin
   version matters; `gst-inspect-1.0 --version` only identifies the core. Confirm
   the backend startup log and Diagnostics after a planned restart. Release
   packages now bundle matched GStreamer 1.24.13; source checkouts still need
   their own fixed runtime (see below and [release packaging](releasing.md)). Vendor backports
   retaining a 1.24.0–1.24.5 plugin version remain conservatively excluded.
4. Qualify the actual runtime before advertising hardware acceleration for that
   OS/GPU combination. Keep a rollback package and verify recorded footage
   around the maintenance restart.

Ubuntu 24.04's official repositories currently retain the affected 1.24.2 VA
plugin. For a source checkout on that release, install the verified private
1.24.13 component without changing `/usr`:

```bash
scripts/install_gstreamer_va_runtime.sh
```

The script verifies upstream's published SHA-256 checksum, builds only the VA
plugin plus the support libraries from the same `gst-plugins-bad` release, and
installs them under the ignored `.cache/gstreamer-1.24.13` directory. The
supported launcher and dev service select that component while retaining the
distribution's other plugins. Run the script independently on each source
checkout. Release packages should use a qualified distribution runtime of
1.24.6 or later; this checkout-local install is not copied into Linux packages.

On Linux, startup raises only the backend's open-file soft limit to its existing
hard limit. The before/after values or failure are logged. This does not repair
a leak, guarantee sufficient capacity, or change the system-wide limit. Do not
use periodic backend restarts or permanently running hidden decoders to conceal
resource growth.

## Verification

Fast regression tests cover affected/fixed version boundaries, registry
exclusion for explicit and automatic decoder selection, and file-limit changes
in isolated child processes:

```bash
CXX=clang++-21 cargo test -p nw-backend --bin nw-backend pipeline::hwaccel::tests
CXX=clang++-21 cargo test -p nw-backend --bin nw-backend resource_limits::tests
```

Run the lifecycle qualification test alone on each target GPU/driver/runtime:

```bash
CXX=clang++-21 cargo test -p nw-backend --bin nw-backend \
  pipeline::hwaccel::tests::decoder_recreation_keeps_descriptors_bounded \
  -- --exact --ignored --nocapture
```

It decodes generated H.264 video through `decodebin`, recreates the pipeline
1,000 times after warmup, and fails if open descriptors grow by more than eight.
It prints the actual decoder factories and descriptor counts. Override
`NW_DECODER_SOAK_CYCLES` for longer runs. It does not start the backend, open
camera URLs, or access recordings. It needs `x264enc`, `h264parse`, a working
decoder and, for hardware validation, access to the GPU. A passing software run
does not validate VA-API. Confirm the printed factory is the intended hardware
decoder when qualifying a patched runtime.

Before a production release, also run a 24-hour isolated NVR soak with the
intended camera count, page cycle intervals, codecs, playback seeks and portal
thumbnail requests. Record the package/driver versions, actual decoders, open
descriptor counts, process memory, GPU memory where available, CPU load,
backend restarts and recording continuity. Descriptors and memory should
stabilize after warmup; all recordings must remain playable with no unexplained
gaps. Exercise camera disconnects and reconnections as well. This hardware and
end-to-end qualification is separate from unit tests and is not implied by
their passing.

Longer term, isolating desktop decoders in a separate process would contain
native decoder crashes and protect the recording process from that failure
class. The current backend still shares a process with GStreamer decoders;
this guard addresses the identified leak, not every possible native crash.
