//! Reusable analytics transport and picture-sharing primitives. No recorder,
//! camera, UI or GStreamer dependencies; a daemon owns its decoding/analysis.
pub use nw_analytics_protocol as protocol;
pub mod cache;
pub mod identity;
#[cfg(unix)]
pub mod local;
pub mod locality;
pub mod pictures;
#[cfg(windows)]
pub mod pipe;
pub mod quic;
pub mod ring;
pub mod secrets;
pub mod tls;
pub mod web;
