//! Linux desktop backends.
//!
//! Each backend is independent, so a host missing one capability still gets the
//! others.

pub mod clipboard;
pub mod mpris;
pub mod notifications;

use std::sync::Arc;

use crate::traits::{Clipboard, Media, Notifier};

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
