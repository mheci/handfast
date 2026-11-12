//! Protocol errors.

use thiserror::Error;

/// Errors produced by the wire layer.
#[derive(Debug, Error)]
pub enum ProtoError {
    /// A frame exceeded the protocol limit.
    #[error("frame too large: {size} bytes (max {max})")]
    FrameTooLarge {
        /// Observed frame size.
        size: usize,
        /// Configured maximum.
        max: usize,
    },

    /// A frame was empty.
    #[error("empty frame")]
    EmptyFrame,

    /// JSON serialization or deserialization failed.
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),

    /// A packet type did not match `kdeconnect.<plugin>[.<action>]`.
    #[error("invalid packet type: {0}")]
    InvalidPacketType(String),

    /// The packet body was not a JSON object.
    #[error("packet body must be a JSON object")]
    BodyNotObject,

    /// IO failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// An identity packet was missing or malformed.
    #[error("invalid identity: {0}")]
    Identity(String),

    /// A payload packet lacked transfer information.
    #[error("payload packet is missing payloadTransferInfo.port")]
    MissingPayloadInfo,

    /// A payload size is not usable.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
}

/// Result alias for protocol operations.
pub type Result<T> = std::result::Result<T, ProtoError>;
