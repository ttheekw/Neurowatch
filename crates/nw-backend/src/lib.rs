//! Neurowatch backend: camera ingest and recording, desktop view decoders,
//! configuration, IPC, shared-memory frame rings, playback, export, discovery
//! and statistics.
//!
//! The `nw-backend` binary runs [`runtime::main_with`] with no extension.
//! Other builds can add one (see [`extension`]).

pub mod app_state;
pub mod assignment;
pub mod config;
pub mod db;
pub mod discovery;
pub mod export;
pub mod extension;
pub mod ipc;
pub mod pipeline;
pub mod playback;
pub mod recording;
pub mod resource_limits;
pub mod runtime;
pub mod shm;
pub mod stats;
pub mod storage_drives;
pub mod taps;
pub mod viewer;
