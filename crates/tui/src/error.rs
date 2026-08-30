//! Error and result types for the hfctl control client.
//!
//! Every module in this crate returns [`Result`] carrying this typed error;
//! only `main` widens failures into an `anyhow::Error` so the binary edge can
//! print one flat diagnostic and exit with status 1.

use std::fmt;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong while driving the daemon from hfctl.
#[derive(Debug)]
pub enum Error {
    /// Transport or framing failure reported by the IPC client.
    Ipc(handfast_ipc::Error),
    /// Terminal setup/teardown or stdout write failure.
    Io(std::io::Error),
    /// The daemon answered with a structured application-level error.
    Daemon {
        /// Machine-readable code assigned by the daemon.
        code: i32,
        /// Human-readable explanation from the daemon.
        message: String,
    },
    /// A reply payload could not be rendered as pretty JSON text.
    Json(serde_json::Error),
    /// The per-connection server-event stream was unavailable.
    EventStreamUnavailable,
}

impl Error {
    /// Build a plain local error (usage/validation problems detected in hfctl
    /// itself, before anything reaches the daemon).
    #[must_use]
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Io(std::io::Error::other(message.into()))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipc(err) => write!(f, "ipc: {err}"),
            Self::Io(err) => write!(f, "io: {err}"),
            Self::Daemon { code, message } => {
                write!(f, "daemon error [{code}]: {message}")
            }
            Self::Json(err) => write!(f, "json: {err}"),
            Self::EventStreamUnavailable => write!(f, "server event stream unavailable"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ipc(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Daemon { .. } | Self::EventStreamUnavailable => None,
        }
    }
}

impl From<handfast_ipc::Error> for Error {
    fn from(err: handfast_ipc::Error) -> Self {
        Self::Ipc(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

/// Flatten a protocol envelope into its payload, raising [`Error::Daemon`]
/// when the daemon replied with a structured failure.
pub(crate) fn expect_ok(response: handfast_ipc::Response) -> Result<serde_json::Value> {
    match response {
        handfast_ipc::Response::Ok { result } => Ok(result),
        handfast_ipc::Response::Err { code, message } => Err(Error::Daemon { code, message }),
    }
}
