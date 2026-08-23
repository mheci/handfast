//! Protocol-wide error type.
//!
//! Mirrors the shape of `handfast_core::error`: I/O, JSON, certificate and
//! catch-all variants, plus the shared [`Result`] alias used throughout the
//! daemon.

/// Errors produced while framing packets, parsing payloads or handling device
/// credentials.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Socket or filesystem I/O failure (e.g. short reads surface here via
    /// `read_exact`).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization/deserialization failure of a packet or its body.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Certificate generation, loading or validation failure.
    #[error("certificate error: {0}")]
    Cert(String),
    /// Protocol-level violation without a tighter variant (for example a frame
    /// whose declared length exceeds [`crate::MAX_PACKET_LEN`]).
    #[error("{0}")]
    Other(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
