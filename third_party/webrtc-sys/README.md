# webrtc-sys (vendored, patched)

LiveKit's `webrtc-sys` crate from [livekit/rust-sdks](https://github.com/livekit/rust-sdks)
at commit `5a656c4bc4bc5c4f7fad17ab69044db3b487dfeb` (the revision `nw-backend`
pins for `livekit`), Apache-2.0 (`LICENSE`, `NOTICE.md`). The workspace
`Cargo.toml` substitutes it for the git crate with `[patch]`.

## Why

NeuroWatch never encodes video for browsers: the camera's own H.264/H.265
access units go to LiveKit through the SDK's pass-through ("pre-encoded")
encoder, which hands them to RTP unchanged. Upstream ties the pass-through
formats to real encoders, so on a desktop, which has no H.265 encoder, an
H.265 camera could not be forwarded at all.

## The changes

Search for "NeuroWatch change". Two in `src/video_encoder_factory.cpp`,
`InternalFactory`:

1. `GetSupportedFormats`: upstream advertises a pass-through format only for
   codecs some real encoder supports, so H.265 was never offered. This copy
   always advertises the pass-through formats.
2. `GetImplementations`: libwebrtc keeps only the negotiated send codecs some
   listed implementation supports, and upstream lists no pass-through
   formats, so a negotiated H.265 track had no send codec and sent nothing.
   This copy lists them last, tagged with the `preencoded` backend, so a codec
   only they support starts on the pass-through encoder directly. H.264 keeps
   matching the software implementation first, as before.

And one in `src/passthrough_video_encoder.cpp`, `GetEncoderInfo`:

3. `has_trusted_rate_controller = true`. Otherwise libwebrtc's frame dropper
   drops frames to stay under its bandwidth estimate, about 1 Mbit/s at the
   start while a viewer's catch-up GOP goes out, as if the next frame were
   encoded afresh. With pass-through every later frame references the dropped
   one, so a browser showed corruption until the next keyframe (H.264 and
   H.265 alike; obvious at 1080p).

With them goes one setting in `crates/nw-backend/src/cloud/media_publish.rs`
(`connect_room`): `single_peer_connection = false`. In single-connection mode
the SDK opens with receive-only video sections listing what libwebrtc can
decode (no H.265), and LiveKit's server takes a session's video codecs from
its first video section.

Only `build.rs`, `include/`, `src/` and the licence files are kept (not
upstream's tests or libwebrtc build scripts). `webrtc-sys-build` still comes
from the same git revision. The manifest replaces upstream workspace inheritance.

## Licensing and release packaging

The LiveKit bindings are Apache-2.0; the notices also include MIT code.
These licences permit commercial use and modified redistribution when their
licences, copyright notices and required attributions are retained. The two
modified C++ files mark the changes inline; `build.rs` is also marked.

Upstream's `src/nvidia/NvCodec/` is deliberately omitted: its NVIDIA SDK
sample implementations and utilities refer to a separate proprietary EULA,
despite the crate's Apache-2.0 metadata. `build.rs` disables the optional
CUDA and Jetson SDK codec paths so host SDK installations cannot silently
add these separately licensed implementations. NeuroWatch forwards encoded
camera video and needs neither path. Desktop hardware decoding uses GStreamer.

`build.rs` exports `THIRD_PARTY_NOTICES.md` in Cargo's build output, combining
this crate's `LICENSE` and `NOTICE.md` with the exact native libwebrtc SDK's
`LICENSE.md`. The release scripts copy it into every bundle using
`scripts/collect_backend_notices.py`; a missing notice fails the build.
The native SDK and its dependencies retain their own licences. Codec patent
permissions are separate from these source-code licences.

References: [Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0),
[WebRTC licence](https://webrtc.googlesource.com/src/+/refs/heads/main/LICENSE),
[NVIDIA Video Codec SDK terms](https://developer.nvidia.com/nvidia-video-codec-sdk-license-agreement).

## Updating the SDK

When `livekit` moves to another revision: copy that revision's `webrtc-sys`
files here excluding `src/nvidia/NvCodec/`, review any new third-party licences,
re-apply the changes, set `version` and the `webrtc-sys-build`
revision in `Cargo.toml` to match, and run the LiveKit tests in
`crates/nw-backend/src/cloud/media_publish.rs` (`--ignored`): the H.265 one
against a server with `video/h265` in `room.enabled_codecs`, and
`h265_is_refused_by_a_server_that_does_not_accept_it` against one without.
Drop this crate once upstream offers a way to advertise pass-through formats.
