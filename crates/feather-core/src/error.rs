//! Domain errors returned by the library.

use thiserror::Error;

#[derive(Debug, Error)]
/// An error returned by Feather's configuration, drivers, daemon, or IPC layer.
pub enum FeatherError {
    /// The configuration file could not be read, parsed, or validated.
    #[error("configuration error: {0}")]
    Config(String),

    /// A hardware driver could not discover, read, write, or release a device.
    #[error("driver error: {0}")]
    Driver(String),

    /// The daemon could not start or complete an operation.
    #[error("daemon error: {0}")]
    Daemon(String),

    /// A local IPC message did not follow the expected protocol.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// An operating-system I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization or deserialization failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// The result type shared by Feather crates.
pub type Result<T> = std::result::Result<T, FeatherError>;
