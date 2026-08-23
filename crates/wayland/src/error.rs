//! Error and result types for the Handfast Wayland bridge.
//!
//! Every fallible public function in this crate returns
//! [`handfast_wayland::Error`](crate::Error) /
//! [`handfast_wayland::Result`](crate::Result) so the daemon can degrade
//! gracefully (log + continue) instead of aborting. No constructor in this
//! crate panics.

/// The error type of the Wayland bridge.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem / socket I/O failure (temp keymap files, thread spawn, …).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The operation is not supported on this platform or session type
    /// (e.g. virtual input on Windows, or without a Wayland compositor).
    #[error("unsupported on this platform/session: {0}")]
    Unsupported(String),

    /// The compositor refused the operation or does not advertise the
    /// required protocol global (identified by its interface name).
    #[error("compositor refused or lacks protocol: {0}")]
    ProtocolMissing(String),

    /// Anything else: connection failures, protocol errors, portal errors.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias used by every fallible public function in this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, Result};

    /// Error display strings are stable contract surface; pin them.
    #[test]
    fn displays_are_stable() -> Result<()> {
        assert_eq!(
            Error::Unsupported("no wayland".to_string()).to_string(),
            "unsupported on this platform/session: no wayland"
        );
        assert_eq!(
            Error::ProtocolMissing("zwp_virtual_keyboard_manager_v1".to_string()).to_string(),
            "compositor refused or lacks protocol: zwp_virtual_keyboard_manager_v1"
        );
        Ok(())
    }
}
