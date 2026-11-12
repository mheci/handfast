//! Network errors.

use handfast_proto::ProtoError;
use thiserror::Error;

/// Errors from discovery, links and pairing.
#[derive(Debug, Error)]
pub enum NetError {
    /// IO failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Protocol parsing failed.
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),

    /// JSON decoding failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Discovery setup or packet handling failed.
    #[error("discovery: {0}")]
    Discovery(String),

    /// Pairing state machine rejected a transition.
    #[error("pairing: {0}")]
    Pairing(String),
}

/// Result alias for network operations.
pub type Result<T> = std::result::Result<T, NetError>;
