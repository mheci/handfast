//! Unified error type shared across handfast-core subsystems.

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by handfast-core operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem or other standard I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite backend failure. rusqlite errors are stringified so this type
    /// stays decoupled from the database driver version.
    #[error("sqlite: {0}")]
    Sqlite(String),
    /// JSON serialization or deserialization failure.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Any other failure, described by the payload.
    #[error("{0}")]
    Other(String),
}
