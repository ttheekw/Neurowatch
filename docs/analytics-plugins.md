# Analytics plugins

Analytics execute in separate daemons. The recorder connects, sends the camera's compressed
H.264/H.265 access units, and accepts bounded events, status and drawings. It never starts a
decoder, scaler or colour converter for analytics. Motion detection is the NeuroWatch Motion
plugin (`nw-motion`), a separate install like any other plugin; camera-side ONVIF events remain
independent.

## Install and approve

NeuroWatch does not ship or start `nw-motion` (since 0.1.8; until then its packages bundled a copy
the recorder ran as a child). Install it from its own release and add it on the Plugins page: on
the same computer it listens on its default address (`unix:$XDG_RUNTIME_DIR/neurowatch/plugins/nw-motion.sock`,
Windows `pipe:\\.\pipe\neurowatch-motion`), which Add Plugin → Local socket finds. Configurations from
the bundled era keep their Motion server (`nw-motion-bundled`) and bindings, pointed at that
default address without the old automatic approval (`plugin_host/legacy_motion.rs`), so an
installed Motion connects and is approved once. Older built-in detector settings
(builtin/both/legacy-enabled) still become bindings to it before config deserialization; ONVIF,
event recording, sensitivity, timing, zones and low-CPU settings survive.

`nw-motion` is developed in its own repository, [Neurowatch-Motion](https://github.com/ttheekw/Neurowatch-Motion), whose release workflow
publishes its builds, on the same computer as NeuroWatch or on another machine:
a Windows x86_64 zip and Linux x86_64/arm64 `.deb`, `.rpm`, `.tar.gz` (plus Arch on x86_64),
each installed and checked in clean containers before publishing. They carry NeuroWatch's own
GStreamer build (`scripts/build_release_gstreamer.sh`, pinned by that repository's
`neurowatch.ref`), only the plugins the analysis uses, found through the binary's DT_RPATH and
its own registry. `nw-motion --check` lists any GStreamer element its pipelines need that is
missing (`appsrc`, `appsink`, `h264parse`, `h265parse`, `decodebin`, `videorate`, `videoscale`,
`videoconvert`, `avdec_h264`, `avdec_h265`); its release checks run it on each package.

Add another server through Plugins, inspect its identity fingerprint and requested permissions,
and approve that key. The recorder sends no media, bindings or licence credential before this.
Key changes require fresh approval. Permissions are `video`, `events`, `overlays`, `decoded_frames`, `web_ui`.
Vendor credentials are write-only, private/DPAPI-protected, scoped to the server and approved
key, omitted from config/status, and removed when the server or approved identity is replaced.
A plugin's licence failure is an ordinary refusal with optional retry time and HTTPS help URL.

Addresses:

- `unix:/absolute/private/plugin.sock` on Unix.
- `pipe:\\.\pipe\plugin-name` on Windows (local clients only).
- `tcp:127.0.0.1:7690` for loopback development.
- `tls:analytics.example:7690` for TLS 1.3 over TCP when UDP is blocked.
- `quic:analytics.example:7690` or `quic:[::1]:7690` for encrypted remote delivery.

Both encrypted transports use TLS 1.3 and ALPN `nw-analytics/1`, a reliable bidirectional control stream and independent
unidirectional GOP streams on QUIC; TLS/TCP uses the same reliable framed byte stream as local sessions. Its certificate uses the same Ed25519 key as the plugin identity.
The manifest signature binds both challenges and the TLS exporter. Node authorization binds
both identities, both challenges, exporter and credential digest. Pins apply to persistent keys,
not short-lived certificates. Unknown keys can be inspected without sending private data.

## Adding a plugin

The Add plugin dialog has **Local** and **URL** choices on desktop and in the portal. Local
asks the recorder for running plugin sockets in its plugin directories (Windows: local plugin
pipes). Refresh rescans; plugins already configured are marked **Already added**. URL accepts
`quic://host:port` or `tls://host:port`, with the canonical colon-only forms also accepted.
The vendor credential prompt uses the application's frameless dialog chrome and masked input.

Admin RPC `plugins.discover` returns local candidates, their verified manifest and `added_as`;
`plugins.inspect {address}` reads one signed greeting. Probing never supplies a locality
capability, node authorization, vendor credential or camera. Work is bounded to four concurrent
probes, 32 candidates, a short read deadline and an eight-second local scan budget.
`plugins.add {id?,address}` reinspects, checks the current config under its write lock and saves
one server atomically. It rejects an existing address, manifest id or approved identity, including
a plugin running at another address. Newly added servers persist the verified `plugin_id` and
still require explicit key/permission approval. `plugins.set` validates duplicate normalized
addresses, known manifest ids and approved keys and preserves learned identity metadata.
Existing manually configured entries are not silently removed or merged.

## Protocol and SDKs

`crates/nw-analytics-protocol` contains the types and framing. Each frame starts with a little-endian
u32 length, then kind 1 (JSON-RPC, ≤1 MiB) or kind 2 (40-byte media header plus ≤16 MiB Annex-B
payload). Identity is full-width `seq` and UTC capture `time_us`, allocated before hub fan-out.
`hello`, authorization and approval precede `attach`; `stream_format` precedes media. Configuration,
validation, detach, camera-moving hints and liveness travel on control. Plugins return events,
status and normalized drawings. Refused attachments retry after their delay or 30 seconds.

Rust authors use `nw-analytics-sdk`; the reference implementation is
[Neurowatch-Motion](https://github.com/ttheekw/Neurowatch-Motion). Python
authors use [the Python SDK](../sdk/python/README.md) and its `nw-analytics-bridge` transport helper.
The helper owns sockets, TLS and signing; Python owns decoding and application logic. SDKs do not
require recorder internals. Keep the persistent identity file across restarts.

`latest` abandons obsolete queued GOPs and resumes at a keyframe with a gap/discontinuity.
`reliable` preserves order within its bounded delay/byte budget; it also reports loss when the
budget is exceeded. Per-session media outboxes are bounded at 256 frames / 32 MiB. QUIC uses
32 MiB budgets and cancellable old GOP streams. Pending UI queries are capped at 64 and expire;
page forwarding has an eight-request channel. Hostile input cannot block recorder fan-out.

Events must use declared known kinds. The host validates finite coordinates, confidence, rate,
open-event count and frame references. Drawings are limited to 256 items, 64 points, eight text
lines and 64 characters per line, with a 25/s per-binding/layer limit and 0.1–10 second TTL.
Plugins choose style tokens, never arbitrary host colours: `accent`, `success`, `warning`, `danger`,
`muted`, and `series1`–`series8` to tell categories apart. Losing a session ends its open events
and clears its drawings; reconnect uses 2/5/10/30 second backoff.

## Existing desktop pictures

Only verified local Unix/pipe peers can receive raw offers. A fresh secret in a read-only capability
proves locality. The host copies existing system-memory RGB24/NV12 planes, or an existing Y plane,
into a private bounded latest-picture ring at the rate the plugin asks for (`max_fps`, at most 30 fps). It never decodes, rescales or converts. GPU
pool buffers are never retained externally. Raw rings are bounded at 32 MiB / 4096 pixels per axis.

A plugin declares accepted formats, rate and optional minimum dimensions. Nonzero dimensions
permit an already-visible larger stream from the same camera and credentials. Stable publication
warms for one second and source changes withdraw/reoffer the capability. The SDK caches compressed
units by reference (16 MiB / 1024 units), switching back to a keyframe after one stale second.
Low-CPU motion keeps compressed keyframes only. Python currently consumes compressed units; native
Rust SDK clients can use raw offers.

## Plugin pages

Declare `web_ui {entry,title}` and request `web_ui` permission. Handle authenticated `web.request`
and `web.read` through the SDK web service. Responses are ≤4 MiB, read in 48 KiB chunks, and cached
for at most five minutes within an 8 MiB / 16-response budget. Viewer requests are GET/HEAD only.
The signed user context contains user, role, allowed bound cameras, plugin key, expiry and nonce;
services must verify it against the session's authenticated node identity.

Plugin pages are shown in the NeuroWatch Cloud portal only; the desktop app has no browser link
and runs no local web server (its Plugins tab has built-in Settings, Live and Output views for
every plugin). The portal shows a page in an opaque-origin iframe sandbox with the SDK. Page
requests are sealed end to end through the existing control plane and fetched from the plugin by
the recorder; authenticated requester identity is attached by the portal server outside the
browser-controlled envelope. No cookies, recorder tokens or vendor credentials reach a plugin
frame. Video uses the ordinary LiveKit panel beside the page, so dashboard traffic never tunnels
video through the page RPC. Permissions are rechecked on every request.

The [JavaScript SDK](../sdk/web/README.md) exposes only user/theme, bound cameras, scoped snapshots,
normal host live view, own-plugin events/playback and admin-only own-binding configuration. It
includes camera, live, zone, line and event components. Frames use `sandbox="allow-scripts"`
without same-origin rights, block network/form/frame navigation with CSP, and communicate through
source/origin/nonce-checked postMessage. Use SDK `fetch`/`asset` for additional resources; arbitrary
external resource loading is unavailable. Keep HTML self-contained when possible.

## History and export

Latest drawings use `plugins.overlays`; recording-time drawings live in a separately bounded
`overlays.db` with camera-filtered cursor paging through `plugins.history`. Desktop and browser
playback align with recorded capture times. Browser live frames carry an `nw.frames.v1` mapping
from the final RTP timestamp to stream/capture identity; no private trailer modifies camera video.
Schema drawing widgets: `nw:zones`, `nw:lanes` (with `x-nw-lines`), `nw:lines` and `nw:areas` are drawn together on the camera's picture in the settings form, on the desktop and in the portal. An array whose `items` have an `enum` is a checkbox group. A settings field with an `enum` may add `x-nw-labels` (`{value: label}`), the names forms show instead of the values. A plugin that sends no events should not ask for the `events` permission: desktop and portal then leave out its Output views. A binding's `figures` in `status` (`[{label, value, icon?, style?}]`; `icon` is one NeuroWatch has,
`car`, `bus`, `lorry`, `motorbike`, `person`, `van`, `three_wheeler`, `bicycle` or `total`, and `style`
the colour of what it counts in the plugin's drawings; at most 16, each at most 40 characters, most
important first) are a few numbers about the camera, such as today's counts: viewers show them
in their own panel (under the picture; on the desktop's video wall, a click-through panel over
that camera's tile on a half-transparent background), so report numbers this way rather than as drawn text. The portal
uses the mapping to show the very picture each live result describes. Results travel only
through the node, which checks each viewer's camera scope; plugins never talk to browsers directly.
Local desktop SHM appends an identity trailer while retaining compatibility with older readers.

Exports optionally stream a `.analytics.jsonl` sidecar independently of camera video. The sidecar
contains capture-time drawings, is capped at 64 MiB, and includes a footer that reports truncation.
The portal offers its separate download. Overlay retention follows its own bounded history policy;
older footage can remain after analytics history expires.


## Resolution-independent drawings

Plugin boxes and points are normalized to the complete processing picture (0–1), not pixel
coordinates. Convert processing pixels with x/width and y/height before sending them. The host
maps those values into the actual displayed video rectangle, including letterboxing, zoom and
pan where supported. Changing between camera resolutions does not require rescaling the event
or drawing data. Streams with a genuinely different crop/field of view need their own mapping.

All viewers paint vectors at display resolution. Desktop uses Qt's device-pixel-aware painter;
portal live/playback and the local dashboard share `sdk/web/annotation-renderer.js`. Local
browser video and drawings use separate canvases so a low-resolution video frame never becomes
the text's raster backing store. Fonts, strokes, padding and margins follow displayed picture
size (18 px type at 720p, minimum 12 px for small tiles). A polygon's `label` is not painted: viewers list it, with its colour, in the key shown with the figures, together with figures that carry a `style` (a figure and a marking with the same label are one entry). A box's `label` belongs to its object and stays on it. Long labels are elided, never squeezed; text is painted after every geometry layer so debug
fills cannot wash out glyphs. Corner panels stack without overlap and use a dark contrast background. Renderer-only sizing
leaves camera units, recording and exported annotation geometry unchanged.


Live Main/Sub alignment uses two node-owned clocks. `time_us` remains the stream's
presentation/recording timeline. Each hub also records `observed_us`, the UTC node
receipt time of that compressed unit. The node resolves both from the plugin's
validated sequence; plugins cannot invent either clock. Live drawing snapshots and
browser RTP maps include the observation clock. Desktop rings append an optional
8-byte observation timestamp per slot after the existing 16-byte identity table,
leaving every older slot and identity offset intact. The trusted local browser
preview sends matching sequence/observation metadata before each media unit;
plugin protocol media headers remain unchanged.

Live views compare observation clocks when both sides provide them, so Main and
Sub need not share a presentation-clock origin. A result holds only for its TTL
and never paints onto an earlier cached picture. Legacy identities and recorded
history keep presentation-time matching. Normalized geometry maps into the viewed
picture rectangle; source resolution does not enter that transform.

The desktop NV12 renderer brackets raw GL drawing with QPainter native-paint
boundaries and restores four-byte pixel-upload alignment before painting labels.
Qt alpha glyph rows are padded to four bytes; leaving the tightly packed video
alignment at one corrupts them into diagonal stripes. Raster/offscreen rendering
cannot exercise this failure. Run `tests/ui/test_gpu_annotations.py` on an OpenGL
display (a private Xvfb with Mesa suffices); ordinary offscreen runs skip it.
