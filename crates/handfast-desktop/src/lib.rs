//! Desktop adapter contracts for Handfast.
//!
//! Plugins talk to these traits. Linux backends implement them with D-Bus,
//! Wayland, PipeWire and libei. Tests use [`MockDesktop`].

#![deny(missing_docs)]

pub mod backends;
pub mod error;
pub mod files;
pub mod mock;
pub mod model;
pub mod owner;
pub mod traits;
#[cfg(windows)]
pub mod winacl;

use std::sync::Arc;

pub use error::{DesktopError, Result};
pub use files::{
    default_incoming_dir, sanitize_filename, FileLimits, Files, FilesError, IncomingFile,
    IncomingRequest, LocalFiles, OutgoingFile, SweepReport,
};
pub use mock::MockDesktop;
pub use model::{
    CallEvent, ContactRecord, KeyEvent, MediaAction, Modifiers, Notification, PlaybackState,
    PointerAction, PointerButton, PointerEvent, Urgency, VirtualDisplay, VolumeState,
};
pub use traits::{
    AudioSink, Battery, BatteryStatus, Clipboard, Contacts, Display, Input, Lock, Media, Notifier,
    Presenter, Screensaver, Shell, Telephony, Volume,
};

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
    /// Desktop session locking is available.
    pub lock: bool,
    /// URL and file opening is available.
    pub shell: bool,
    /// File receive and send is available.
    pub files: bool,
    /// Host battery reporting is available.
    pub battery: bool,
    /// A fullscreen presentation view is available.
    pub presenter: bool,
    /// Telephony events can be read from a local modem stack.
    pub telephony: bool,
    /// An address book can be read.
    pub contacts: bool,
    /// A virtual display can be created.
    pub display: bool,
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
    /// Session lock adapter.
    pub lock: Option<Arc<dyn Lock>>,
    /// Shell integration adapter.
    pub shell: Option<Arc<dyn Shell>>,
    /// File receive and send adapter.
    pub files: Option<Arc<dyn Files>>,
    /// Host battery reporting adapter.
    pub battery: Option<Arc<dyn Battery>>,
    /// Presentation view adapter.
    pub presenter: Option<Arc<dyn Presenter>>,
    /// Telephony adapter.
    pub telephony: Option<Arc<dyn Telephony>>,
    /// Address book adapter.
    pub contacts: Option<Arc<dyn Contacts>>,
    /// Virtual display adapter.
    pub display: Option<Arc<dyn Display>>,
}

impl std::fmt::Debug for Desktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Desktop")
            .field("capabilities", &self.capabilities())
            .finish()
    }
}

impl Desktop {
    /// Install a notification adapter.
    #[must_use]
    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Install an MPRIS media adapter.
    #[must_use]
    pub fn with_media(mut self, media: Arc<dyn Media>) -> Self {
        self.media = Some(media);
        self
    }

    /// Install a system volume adapter.
    #[must_use]
    pub fn with_volume(mut self, volume: Arc<dyn Volume>) -> Self {
        self.volume = Some(volume);
        self
    }

    /// Install a telephony adapter.
    #[must_use]
    pub fn with_telephony(mut self, telephony: Arc<dyn Telephony>) -> Self {
        self.telephony = Some(telephony);
        self
    }

    /// Install an address book adapter.
    #[must_use]
    pub fn with_contacts(mut self, contacts: Arc<dyn Contacts>) -> Self {
        self.contacts = Some(contacts);
        self
    }

    /// Install a virtual display adapter.
    #[must_use]
    pub fn with_display(mut self, display: Arc<dyn Display>) -> Self {
        self.display = Some(display);
        self
    }

    /// Install a shell adapter.
    #[must_use]
    pub fn with_shell(mut self, shell: Arc<dyn Shell>) -> Self {
        self.shell = Some(shell);
        self
    }

    /// Install a screensaver adapter.
    #[must_use]
    pub fn with_screensaver(mut self, screensaver: Arc<dyn Screensaver>) -> Self {
        self.screensaver = Some(screensaver);
        self
    }

    /// Install a battery adapter.
    #[must_use]
    pub fn with_battery(mut self, battery: Arc<dyn Battery>) -> Self {
        self.battery = Some(battery);
        self
    }

    /// Install a presenter adapter.
    #[must_use]
    pub fn with_presenter(mut self, presenter: Arc<dyn Presenter>) -> Self {
        self.presenter = Some(presenter);
        self
    }

    /// Install a clipboard adapter.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Arc<dyn Clipboard>) -> Self {
        self.clipboard = Some(clipboard);
        self
    }

    /// Install a session lock adapter.
    #[must_use]
    pub fn with_lock(mut self, lock: Arc<dyn Lock>) -> Self {
        self.lock = Some(lock);
        self
    }

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
            lock: self.lock.is_some(),
            shell: self.shell.is_some(),
            files: self.files.is_some(),
            battery: self.battery.is_some(),
            presenter: self.presenter.is_some(),
            telephony: self.telephony.is_some(),
            contacts: self.contacts.is_some(),
            display: self.display.is_some(),
        }
    }
}
