# NeuroWatch Python analytics SDK

Requires Python 3.11+. The Python package uses the standard library; the companion Rust helper
provides the same authenticated transport as the Rust SDK, including QUIC, TLS-over-TCP fallback and Windows pipes.

```sh
cargo install --path crates/nw-analytics-sdk --bin nw-analytics-bridge
python -m pip install ./sdk/python
python sdk/python/examples/byte_meter.py --listen tcp:127.0.0.1:7690 --identity /private/path/meter.key
```

Implement `Plugin.manifest`, `attach`, `unit` and optional authorization, notification and HTTP
callbacks. Call `serve(plugin, address=..., identity=...)`; preserve the identity file. Decode
compressed Annex-B units in your own process using an Apache-compatible decoder. Python does not
currently consume local raw-picture rings; native Rust SDK clients can use those offers.

The helper handles socket ownership, TLS identity, challenges and node signatures before passing
an authorization credential into Python. It starts one Python stdio session per node. stdout is
reserved for protocol output; write diagnostics to stderr. Do not echo credentials in errors.
Raise `RefusalError` for a typed refusal and optional HTTPS help URL/retry delay.

`MediaUnit` keeps exact Python integer sequence numbers/timestamps. Each binding has a bounded
worker queue; slow callbacks do not block control. A global 32 MiB budget includes in-flight
units, loss skips to a keyframe and generates a gap notification. Offload CPU inference from the
async event loop with a thread/process or native library. The SDK does not invent detection data.

For a page, declare `web_ui` permission and entry metadata, then return `HttpResponse` from
`Plugin.web(HttpRequest)`. Signed user/camera contexts are verified by the helper and response
chunks are scoped to that identity. Return self-contained HTML or use the injected JavaScript
SDK for resources; arbitrary browser networking is blocked. Limit domain data by
`request.user['cameras']`, never assume every camera belongs to the viewer.

Run `PYTHONPATH=sdk/python python -m unittest discover -s sdk/python/tests -v`. Rust integration tests in
`crates/nw-analytics-sdk/tests/python_bridge.rs` drive a real Python process over TCP and QUIC.
