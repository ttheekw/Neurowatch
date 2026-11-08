//! The protocol between a NeuroWatch node and an analytics plugin daemon.
//!
//! The plugin listens; the node connects. One connection (a session) carries
//! length-prefixed [frames](frame): JSON-RPC 2.0 messages both ways (control,
//! events, status) and binary [media units](frame::MediaUnit) from the node.
//! Design: `docs/plans/2026-10-27-analytics-plugins.md`.
//!
//! A session:
//!
//! 1. node → `hello` → plugin answers with its [`Manifest`](messages::Manifest);
//! 2. node → `attach` per bound camera → `accepted` media or an error;
//! 3. node → `stream_format`, then media units; `gap` when units were lost;
//! 4. plugin → `event.start` / `event.update` / `event.end`, `overlay`, `status`;
//! 5. node → `configure`, `camera_hint`, `detach` as things change.
pub mod address;
pub mod auth;
pub mod decoded;
pub mod frame;
pub mod messages;
pub mod rpc;
pub mod web;

/// This crate's protocol version, sent in `hello` and the manifest.
pub const PROTOCOL_VERSION: u32 = 1;

/// Method names.
pub mod method {
    // Node → plugin requests.
    pub const HELLO: &str = "hello";
    pub const ATTACH: &str = "attach";
    pub const CONFIGURE: &str = "configure";
    pub const VALIDATE_CONFIG: &str = "validate_config";
    pub const DETACH: &str = "detach";
    pub const PING: &str = "ping";
    pub const AUTHENTICATE: &str = "authenticate";
    // Node → plugin notifications.
    pub const STREAM_FORMAT: &str = "stream_format";
    pub const GAP: &str = "gap";
    pub const CAMERA_HINT: &str = "camera_hint";
    pub const DECODED_AVAILABLE: &str = "decoded_available";
    pub const DECODED_WITHDRAWN: &str = "decoded_withdrawn";
    // Plugin → node notifications.
    pub const EVENT_START: &str = "event.start";
    pub const EVENT_UPDATE: &str = "event.update";
    pub const EVENT_END: &str = "event.end";
    pub const OVERLAY: &str = "overlay";
    pub const STATUS: &str = "status";
}

/// JSON-RPC error codes of this protocol (beyond the standard ones).
pub mod error_code {
    pub const INVALID_PARAMS: i64 = -32602;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INTERNAL: i64 = -32603;
    /// `hello` with a protocol version the plugin does not speak.
    pub const UNSUPPORTED_PROTOCOL: i64 = -33001;
    /// `attach` refused (`data.code` says why: see [`crate::messages::Refusal`]).
    pub const REJECTED: i64 = -33002;
    /// A configuration the plugin does not accept (`data.errors`).
    pub const INVALID_CONFIG: i64 = -33003;
    /// A binding the plugin does not know.
    pub const UNKNOWN_BINDING: i64 = -33004;
}
