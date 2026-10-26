//! Neurowatch backend: camera ingest and recording, desktop view decoders,
//! configuration, IPC, shared-memory frame rings, playback, export, discovery,
//! statistics, and NeuroWatch Cloud ([`cloud`]). The `nw-backend` binary
//! runs [`runtime::main`].

pub mod accounts;
pub mod alerts;
pub mod app_state;
pub mod assignment;
mod camera_credentials;
pub mod cloud;
pub mod config;
pub mod db;
pub mod debug_monitor;
pub mod discovery;
pub mod edition;
pub mod events;
pub mod export;
pub mod ipc;
pub mod logging;
pub mod motion;
pub mod onvif_events;
pub mod pipeline;
pub mod playback;
pub mod ptz;
pub mod recording;
pub mod resource_limits;
pub mod runtime;
pub mod shm;
pub mod snapshot;
pub mod stats;
pub mod storage_drives;
pub mod storage_guard;
pub mod streams;
pub mod supervise;
pub mod talk;
pub mod taps;
pub mod viewer;
