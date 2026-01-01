//! Error types for Niri IPC operations

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur when communicating with the niri compositor
#[derive(Debug, Error)]
pub enum NiriError {
    /// The NIRI_SOCKET environment variable is not set
    #[error("NIRI_SOCKET environment variable not set - is niri running?")]
    SocketNotSet,

    /// The socket path does not exist
    #[error("Niri socket not found at {path}")]
    SocketNotFound { path: PathBuf },

    /// Failed to connect to the niri socket
    #[error("Failed to connect to niri socket at {path}: {source}")]
    ConnectionFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Failed to send request to niri
    #[error("Failed to send request to niri: {0}")]
    SendFailed(#[source] std::io::Error),

    /// Failed to receive response from niri
    #[error("Failed to receive response from niri: {0}")]
    ReceiveFailed(#[source] std::io::Error),

    /// Failed to serialize request to JSON
    #[error("Failed to serialize request: {0}")]
    SerializeFailed(#[source] serde_json::Error),

    /// Failed to deserialize response from JSON
    #[error("Failed to deserialize response: {0}")]
    DeserializeFailed(#[source] serde_json::Error),

    /// Niri returned an error response
    #[error("Niri returned error: {message}")]
    NiriError { message: String },

    /// Connection was closed unexpectedly
    #[error("Connection to niri closed unexpectedly")]
    ConnectionClosed,

    /// Maximum retry attempts exceeded
    #[error("Failed to connect to niri after {attempts} attempts")]
    MaxRetriesExceeded { attempts: u32 },
}
