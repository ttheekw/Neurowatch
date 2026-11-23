> Generated from the canonical pages at [neurowatch.app/docs](https://neurowatch.app/docs). Do not edit these files by hand: change the pages' source and regenerate them.

# How NeuroWatch works

Technical documentation for engineers: which process does what, how video moves through the recorder, and why each design choice was made. The diagrams are Mermaid, which GitHub draws. There are no benchmark figures on purpose: the load depends on your cameras, codecs and hardware, and Diagnostics shows the real numbers for your own recorder.

## Start here

- [Overview](overview.md): How the NeuroWatch camera recorder is built: a Rust recorder service, a Qt desktop app and analytics plugins as separate processes, and the rules behind them.
- [Architecture](architecture.md): Inside the NeuroWatch recorder service and an analytics plugin: the modules, who owns which thread, how the service starts and how it restarts itself.

## Video pipeline

- [Stream engine and buffers](stream-engine.md): How NeuroWatch opens each camera stream once, numbers every picture, caches the current GOP and gives each reader a bounded queue with its own loss rule.
- [Decoding and hardware acceleration](hardware-decoding.md): When NeuroWatch decodes video, how it ranks VA-API, NVDEC, Direct3D, Quick Sync and V4L2 decoders, and how it falls back to the CPU without losing the camera.
- [Desktop rendering](desktop-rendering.md): How pictures reach the NeuroWatch video wall: shared-memory rings, NV12 converted on the GPU, a reader that sets its rate, and latest-frame-only painting.
- [Recording, playback and export](recording.md): How NeuroWatch records cameras as segmented Matroska without decoding, indexes them in SQLite, protects missing drives, archives to a NAS and exports MP4 clips.
- [Browser viewing](browser-viewing.md): What the NeuroWatch recorder does for browser viewers: the camera's own H.264 or H.265 passed through, a fast start from the GOP cache, and when it converts.

## Analytics and events

- [Analytics plugins](plugins.md): How NeuroWatch analytics plugins work: separate processes over sockets, TLS or QUIC, signed keys, compressed or shared pictures, drawings tied to pictures.
- [Events and integrations](events.md): How NeuroWatch handles events: an EventHub that never waits on the database, ONVIF push and pull, plugin events, pictures, alerts and MQTT.

## Engineering

- [Engineering in Rust](rust.md): Why the NeuroWatch recorder is written in Rust, the rules its code follows for locks, errors and streaming threads, how it is tested, and its licence.
