//! NeuroWatch motion detection, as an analytics plugin daemon.
//!
//! NeuroWatch connects to the daemon (`nw-analytics-protocol`), binds
//! cameras and sends their compressed video; the daemon decodes it, runs the
//! detector and answers with motion events. Design:
//! `docs/plans/2026-10-27-analytics-plugins.md` in the NeuroWatch repository.
//!
//! The analysis itself ([`detector`], [`activity`], [`settings`]) is pure:
//! pictures and times in, samples and event actions out.
pub mod access;
pub mod activity;
pub mod decoding;
pub mod detector;
pub mod drawing;
pub mod gstreamer_runtime;
pub mod manifest;
pub mod server;
pub mod settings;
pub mod worker;
