//! Adapter traits implemented by Linux desktop backends.

use async_trait::async_trait;

use crate::error::Result;
use crate::model::{KeyEvent, MediaAction, Notification, PlaybackState, PointerEvent, VolumeState};

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

    /// Inject a key press and release.
    async fn key(&self, event: KeyEvent) -> Result<()>;
}

/// Screensaver inhibition.
#[async_trait]
pub trait Screensaver: Send + Sync {
    /// Inhibit the screensaver while the device is connected.
    async fn inhibit(&self, reason: &str) -> Result<()>;

    /// Release a previous inhibition.
    async fn release(&self) -> Result<()>;
}

/// Opening URLs and files through the desktop session.
#[async_trait]
pub trait Shell: Send + Sync {
    /// Open a URL with the default handler.
    async fn open_url(&self, url: &str) -> Result<()>;
}
