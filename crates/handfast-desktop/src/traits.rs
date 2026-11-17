//! Adapter traits implemented by desktop backends.

use async_trait::async_trait;

use crate::error::{DesktopError, Result};
use crate::model::{
    ButtonAction, CommandSpec, KeyEvent, MediaAction, Notification, PenEvent, PlaybackState,
    PointerButton, PointerEvent, PowerState, VolumeState,
};

/// Text clipboard access.
#[async_trait]
pub trait Clipboard: Send + Sync {
    /// Read the current clipboard text, if any.
    async fn read_text(&self) -> Result<Option<String>>;

    /// Replace the clipboard text.
    async fn write_text(&self, text: &str) -> Result<()>;
}

/// Desktop notification delivery.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Show a notification.
    async fn notify(&self, notification: &Notification) -> Result<()>;

    /// Close a notification by backend ID.
    async fn close(&self, id: &str) -> Result<()>;
}

/// MPRIS media player control.
#[async_trait]
pub trait Media: Send + Sync {
    /// List available player identities.
    async fn players(&self) -> Result<Vec<String>>;

    /// Read a player snapshot.
    async fn playback(&self, player: &str) -> Result<Option<PlaybackState>>;

    /// Send a transport action.
    async fn control(&self, player: &str, action: MediaAction) -> Result<()>;

    /// Set player volume, 0-100.
    async fn set_volume(&self, player: &str, percent: u8) -> Result<()>;
}

/// System output volume control.
#[async_trait]
pub trait Volume: Send + Sync {
    /// Read the current output volume.
    async fn get(&self) -> Result<VolumeState>;

    /// Set output volume and mute state.
    async fn set(&self, percent: u8, muted: bool) -> Result<()>;
}

/// Synthetic pointer and keyboard injection.
#[async_trait]
pub trait Input: Send + Sync {
    /// Inject a relative pointer movement or scroll.
    async fn pointer(&self, event: PointerEvent) -> Result<()>;

    /// Inject a pointer button event.
    async fn button(&self, button: PointerButton, action: ButtonAction) -> Result<()>;

    /// Inject a key press and release.
    async fn key(&self, event: KeyEvent) -> Result<()>;

    /// Move the pointer to an absolute pixel position on the main display.
    ///
    /// The default implementation reports [`DesktopError::Unsupported`].
    async fn absolute_pointer(&self, x: i32, y: i32) -> Result<()> {
        let _ = (x, y);
        Err(DesktopError::Unsupported(
            "absolute pointer positioning is not supported on this desktop",
        ))
    }

    /// The main display size in pixels, as `(width, height)`. Its top-left
    /// pixel is the origin of [`Input::absolute_pointer`].
    ///
    /// The default implementation reports [`DesktopError::Unsupported`].
    async fn main_display(&self) -> Result<(i32, i32)> {
        Err(DesktopError::Unsupported(
            "the main display size is not available on this desktop",
        ))
    }

    /// Open a digitizer session that maps a `width` x `height` device
    /// coordinate space, with `resolution_x` and `resolution_y` in pixels per
    /// millimetre, onto the desktop.
    ///
    /// The default implementation reports [`DesktopError::Unsupported`], so
    /// backends without absolute pen injection keep working.
    async fn start_pen_session(
        &self,
        width: i32,
        height: i32,
        resolution_x: i32,
        resolution_y: i32,
    ) -> Result<()> {
        let _ = (width, height, resolution_x, resolution_y);
        Err(DesktopError::Unsupported(
            "absolute pen injection is not supported on this desktop",
        ))
    }

    /// Close the digitizer session opened by [`Input::start_pen_session`].
    ///
    /// No-op backends report [`DesktopError::Unsupported`]; implementations
    /// must tolerate an end without a matching start.
    async fn end_pen_session(&self) -> Result<()> {
        Err(DesktopError::Unsupported(
            "absolute pen injection is not supported on this desktop",
        ))
    }

    /// Inject one absolute pen event from an active digitizer session.
    ///
    /// Pressure is best effort. Backends that cannot inject pressure must
    /// ignore it rather than fake it.
    async fn pen(&self, event: PenEvent) -> Result<()> {
        let _ = event;
        Err(DesktopError::Unsupported(
            "absolute pen injection is not supported on this desktop",
        ))
    }
}

/// Screensaver inhibition.
#[async_trait]
pub trait Screensaver: Send + Sync {
    /// Inhibit the screensaver while the device is connected.
    async fn inhibit(&self, reason: &str) -> Result<()>;

    /// Release a previous inhibition.
    async fn release(&self) -> Result<()>;
}

/// Desktop session locking.
///
/// Lock only. Unlocking is not exposed, so a remote peer can never take over an
/// unattended session.
#[async_trait]
pub trait Lock: Send + Sync {
    /// Lock the current desktop session.
    async fn lock(&self) -> Result<()>;
}

/// Opening URLs and files through the desktop session.
#[async_trait]
pub trait Shell: Send + Sync {
    /// Open a URL with the default handler.
    async fn open_url(&self, url: &str) -> Result<()>;
}

/// Host battery state.
#[async_trait]
pub trait Power: Send + Sync {
    /// Read the host battery, or `None` when the host has no battery.
    async fn state(&self) -> Result<Option<PowerState>>;
}

/// Locally configured commands that a peer may list and run.
#[async_trait]
pub trait Commands: Send + Sync {
    /// List the configured commands.
    async fn list(&self) -> Result<Vec<CommandSpec>>;

    /// Read one command by key.
    async fn get(&self, key: &str) -> Result<Option<CommandSpec>>;

    /// Add one command with a caller-generated key.
    async fn add(&self, key: &str, name: &str, argv: &[String]) -> Result<()>;

    /// Remove one command. Returns whether it existed.
    async fn remove(&self, key: &str) -> Result<bool>;
}
