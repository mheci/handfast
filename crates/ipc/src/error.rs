//! Error type for handfast-ipc.

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by the IPC transport and protocol layers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Socket or other I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding/decoding failure inside a frame.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A frame declared or required more than [`crate::MAX_FRAME_BYTES`].
    #[error("frame too large: {size} bytes exceeds limit of {max} bytes")]
    FrameTooLarge {
        /// Offending size in bytes.
        size: usize,
        /// Configured maximum ([`crate::MAX_FRAME_BYTES`]).
        max: usize,
    },
    /// Peer closed the connection or the stream ended mid-frame.
    #[error("connection closed")]
    Closed,
    /// Any other failure, described by the payload.
    #[error("{0}")]
    Other(String),
}
