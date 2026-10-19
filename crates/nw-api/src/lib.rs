//! nw-api — the hosted NeuroWatch control plane.
//!
//! This crate is the cloud counterpart of the node's outbound-only Cloud Link
//! (`crates/nw-backend/src/cloud`). It owns tenants, node enrollments, node
//! identities and a cache of camera *metadata*, and nothing else: there is no
//! column, route, log line or response anywhere in it for an RTSP or ONVIF URL,
//! a camera credential, a recording segment, a local filesystem path or a local
//! IPC endpoint.
//!
//! Browser-facing media delivery is not part of this crate yet; it is the next
//! layer up and needs the node identity and camera inventory established here.

pub mod clock;
pub mod config;
pub mod error;
pub mod identity;
pub mod protocol;
pub mod routes;
pub mod server;
pub mod store;

pub use config::{ApiConfig, ConfigError};
pub use error::{ApiError, ApiResult};
pub use server::{AppState, Server};
pub use store::{Store, StoreError};
