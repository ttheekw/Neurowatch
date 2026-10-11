//! nw-core — Shared types, traits, and error definitions for Neurowatch.
//!
//! This crate contains NO business logic. It defines the interfaces
//! that all other crates depend on.

pub mod camera;
pub mod camera_discovery;
pub mod control_plane;
pub mod encoded;
pub mod error;
pub mod event;
pub mod frame;
pub mod ipc;
pub mod stats;
pub mod traits;

pub use camera::CameraConfig;
pub use error::NwError;
