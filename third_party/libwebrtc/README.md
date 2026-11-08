# libwebrtc (vendored, patched)

LiveKit's safe Rust bindings from `livekit/rust-sdks` revision
`5a656c4bc4bc5c4f7fad17ab69044db3b487dfeb`, Apache-2.0 (LICENSE).

NeuroWatch adds `rtp_timestamp` to `native::packet_trailer::PublishTimingEvent`,
copied from the matching field in the vendored `webrtc-sys` timing observer.
It reports the timestamp after WebRTC's random sender offset, which lets a
browser's `requestVideoFrameCallback` identify the exact picture. The observer
has packet trailers disabled: camera access units remain byte-for-byte intact.

Only the manifest and `src/` are copied. Workspace-inherited manifest fields
are expanded using the upstream workspace's versions. No native codecs or
additional native code are added by this Rust wrapper.
