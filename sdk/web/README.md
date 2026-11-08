# Plugin browser SDK

`plugin-sdk.js` is injected by the host into an opaque sandbox iframe. A plugin page uses
`window.NeuroWatch` after `await NeuroWatch.ready`; it does not import recorder credentials.
See the working motion dashboard in `crates/nw-motion/web/index.html`.

The SDK provides `request`, scoped `snapshot`, `live.open`, plugin-only `events.list` and
`playback.open`, and admin-only `binding.get` / `binding.save`. `init` returns signed-in user,
role, bound-and-allowed cameras and theme. SDK fetch/asset helpers proxy plugin resources;
external network access, forms, arbitrary frames and generic recorder RPC are blocked.
The SDK reports the page's height (`resize`, watched with a `ResizeObserver`), and the portal sizes
the frame to it, so a dashboard scrolls with the portal's page instead of inside a fixed box. The
theme carries `--nw-surface`, `--nw-raised`, `--nw-line`, `--nw-text`, `--nw-muted`, `--nw-accent` and
`--nw-scheme` (`light`/`dark`), following the portal's appearance when the page opens.

Components: `<nw-camera-picker>`, `<nw-live-tile>`, `<nw-zone-editor>`, `<nw-line-editor>` and
`<nw-event-list>`. Set their `camera` after selection. Live opens the portal's normal camera
panel (LiveKit) beside the plugin page.
Zone/line editors use normalized points over an actual camera snapshot and save the plugin's
own binding config. Event actions validate both camera and plugin ownership before playback.

`host.js` is canonical parent-side code shared with the portal; its nonce/source/origin checks,
operation allowlist, body/chunk bounds and camera scope are security boundaries. Use
`scripts/sync_plugin_web_sdk.py --check` to verify the portal copies.

Run `node --test sdk/web/tests/*.test.mjs` from the repository root.


`annotation-renderer.js` is the canonical host-side vector drawing renderer for portal live and
playback views. It maps normalized primitives into the fitted video rectangle and renders text
at CSS size on a device-pixel-ratio-sized canvas. Keep this layer separate from the decoded
video bitmap. Reference text is 18 px at 720p with a 12 px small-tile minimum; dimensions and
line widths follow display size. Long text is elided instead of horizontally compressed. A
polygon's `label` is not painted: the host lists it in the key under the picture.
