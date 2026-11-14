//! Shared library surface for the Handfast daemon.
//!
//! The daemon binary owns composition. This crate exposes the local control
//! channel and the state directory default so `handfastd` and `hfctl` resolve
//! the same endpoint.

pub mod control;

use std::path::PathBuf;

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
