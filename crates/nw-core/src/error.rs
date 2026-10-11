//! Shared error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NwError {
    #[error("Camera not found: {id}")]
    CameraNotFound { id: String },

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Pipeline error: {0}")]
    Pipeline(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
