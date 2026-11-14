//! Linux desktop backends.
//!
//! Each backend is independent, so a host missing one capability still gets the
//! others.

pub mod clipboard;
pub mod mpris;
pub mod notifications;
pub mod screensaver;
pub mod shell;
pub mod volume;

use std::sync::Arc;

use crate::traits::{Clipboard, Media, Notifier, Screensaver, Shell, Volume};

/// Build the host clipboard backend.
#[must_use]
pub fn clipboard() -> Arc<dyn Clipboard> {
    Arc::new(clipboard::CommandClipboard::detect())
}

/// Build the host notification backend.
#[must_use]
pub fn notifier() -> Arc<dyn Notifier> {
    Arc::new(notifications::CommandNotifier::new())
}

/// Build the host MPRIS media backend.
#[must_use]
pub fn media() -> Arc<dyn Media> {
    Arc::new(mpris::PlayerctlMedia::new())
}

/// Build the host system volume backend.
#[must_use]
pub fn volume() -> Arc<dyn Volume> {
    Arc::new(volume::WpctlVolume::new())
}

/// Build the host URL opening backend.
#[must_use]
pub fn shell() -> Arc<dyn Shell> {
    Arc::new(shell::XdgOpenShell::new())
}

/// Build the host screensaver inhibition backend.
#[must_use]
pub fn screensaver() -> Arc<dyn Screensaver> {
    Arc::new(screensaver::SystemdInhibitor::new())
}
