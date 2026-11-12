//! Desktop adapter errors.

use thiserror::Error;

/// Errors returned by desktop adapters.
#[derive(Debug, Error)]
pub enum DesktopError {
    /// The host or session does not provide this capability.
    #[error("desktop capability unavailable: {0}")]
    Unsupported(&'static str),

    /// The backend rejected the operation.
    #[error("desktop backend error: {0}")]
    Backend(String),
}

/// Result alias for desktop operations.
pub type Result<T> = std::result::Result<T, DesktopError>;
