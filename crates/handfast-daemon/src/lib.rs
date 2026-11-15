//! Shared library surface for the Handfast daemon.
//!
//! [`run`] is the daemon composition root.

pub mod control;
#[cfg(feature = "gui")]
pub mod gui;
pub mod service;
pub mod update;

mod daemon;
mod identity;
mod limits;
mod pairing;
mod redact;
mod session;

pub use daemon::{parse_tcp_port, run, DaemonArgs, DEFAULT_TCP_PORT, SHUTDOWN_GRACE};

use std::path::PathBuf;

/// What `handfast` does when it is started without a subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSubcommand {
    /// Open the GUI window. The `gui` feature is on.
    Gui,
    /// Print help to stderr and exit 2. The `gui` feature is off.
    Help,
}

/// The daemon is never the default, so a typo cannot start a process that
/// holds the store lock.
#[must_use]
pub const fn no_subcommand() -> NoSubcommand {
    if cfg!(feature = "gui") {
        NoSubcommand::Gui
    } else {
        NoSubcommand::Help
    }
}

/// Default state directory.
///
/// `%LOCALAPPDATA%\handfast` on Windows, then `$XDG_DATA_HOME/handfast`, then
/// `$HOME/.local/share/handfast`, then a relative fallback.
#[must_use]
pub fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(value) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(value).join("handfast");
        }
    }
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(value).join("handfast");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/handfast");
    }
    PathBuf::from("handfast-data")
}

#[cfg(test)]
mod tests {
    use super::{no_subcommand, NoSubcommand};

    #[cfg(feature = "gui")]
    #[test]
    fn no_subcommand_opens_the_gui_when_the_feature_is_on() {
        assert_eq!(no_subcommand(), NoSubcommand::Gui);
    }

    #[cfg(not(feature = "gui"))]
    #[test]
    fn no_subcommand_prints_help_when_the_feature_is_off() {
        assert_eq!(no_subcommand(), NoSubcommand::Help);
    }
}
