//! Desktop adapter contracts for Handfast.
//!
//! Plugins talk to these traits. Linux backends implement them with D-Bus,
//! Wayland, PipeWire and libei. Tests use [`MockDesktop`].

#![deny(missing_docs)]

pub mod error;
pub mod mock;
pub mod model;
pub mod traits;

use std::sync::Arc;

pub use error::{DesktopError, Result};
pub use mock::MockDesktop;
pub use model::{
    KeyEvent, MediaAction, Modifiers, Notification, PlaybackState, PointerEvent, VolumeState,
};
pub use traits::{Clipboard, Input, Media, Notifier, Screensaver, Shell, Volume};

/// Presence of desktop adapters on the current host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DesktopCapabilities {
    /// Clipboard read/write is available.
    pub clipboard: bool,
    /// Desktop notifications are available.
    pub notifications: bool,
    /// MPRIS media control is available.
    pub media: bool,
    /// System volume control is available.
    pub volume: bool,
    /// Synthetic input injection is available.
    pub input: bool,
    /// Screensaver inhibition is available.
    pub screensaver: bool,
    /// URL and file opening is available.
    pub shell: bool,
}

/// A composed desktop backend.
#[derive(Clone, Default)]
pub struct Desktop {
    /// Clipboard adapter.
    pub clipboard: Option<Arc<dyn Clipboard>>,
    /// Notification adapter.
    pub notifier: Option<Arc<dyn Notifier>>,
    /// MPRIS adapter.
    pub media: Option<Arc<dyn Media>>,
    /// System volume adapter.
    pub volume: Option<Arc<dyn Volume>>,
    /// Input injection adapter.
    pub input: Option<Arc<dyn Input>>,
    /// Screensaver adapter.
    pub screensaver: Option<Arc<dyn Screensaver>>,
    /// Shell integration adapter.
    pub shell: Option<Arc<dyn Shell>>,
}

impl std::fmt::Debug for Desktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Desktop")
            .field("capabilities", &self.capabilities())
            .finish()
    }
}

impl Desktop {
    /// Report which adapters are present.
    #[must_use]
    pub fn capabilities(&self) -> DesktopCapabilities {
        DesktopCapabilities {
            clipboard: self.clipboard.is_some(),
            notifications: self.notifier.is_some(),
            media: self.media.is_some(),
            volume: self.volume.is_some(),
            input: self.input.is_some(),
            screensaver: self.screensaver.is_some(),
            shell: self.shell.is_some(),
        }
    }
}
