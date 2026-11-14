//! Desktop value types shared by adapters and plugins.

use serde::{Deserialize, Serialize};

/// Notification urgency, as understood by desktop notification daemons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    /// Low urgency.
    Low,
    /// Normal urgency.
    #[default]
    Normal,
    /// Critical urgency.
    Critical,
}

/// A desktop notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Application display name.
    pub app_name: String,
    /// Notification title.
    pub title: String,
    /// Notification body.
    pub text: String,
    /// Backend notification ID, when known.
    pub id: Option<String>,
    /// Reply ID for repliable notifications.
    pub reply_id: Option<String>,
    /// Notification urgency.
    #[serde(default)]
    pub urgency: Urgency,
}

impl Notification {
    /// Build a notification with no backend IDs.
    #[must_use]
    pub fn new(
        app_name: impl Into<String>,
        title: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            app_name: app_name.into(),
            title: title.into(),
            text: text.into(),
            id: None,
            reply_id: None,
            urgency: Urgency::Normal,
        }
    }
}

/// System or player volume state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeState {
    /// Volume percentage, 0-100.
    pub percent: u8,
    /// Whether output is muted.
    pub muted: bool,
}

impl Default for VolumeState {
    fn default() -> Self {
        Self {
            percent: 50,
            muted: false,
        }
    }
}

/// Keyboard modifiers for an injected key event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    /// Alt is held.
    pub alt: bool,
    /// Control is held.
    pub ctrl: bool,
    /// Shift is held.
    pub shift: bool,
    /// Super/Meta is held.
    pub sup: bool,
}

/// A synthetic keyboard event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyEvent {
    /// A printable character or string.
    Text {
        /// Text to type.
        text: String,
        /// Held modifiers.
        modifiers: Modifiers,
    },
    /// A non-printable key identified by the KDE Connect special-key code.
    Special {
        /// Special key code, 0-32.
        code: u8,
        /// Held modifiers.
        modifiers: Modifiers,
    },
}

/// A relative pointer movement or scroll.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointerEvent {
    /// Relative X delta.
    pub dx: f64,
    /// Relative Y delta.
    pub dy: f64,
    /// Whether this is a scroll event rather than movement.
    pub scroll: bool,
}

/// Transport action for an MPRIS player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaAction {
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Toggle playback.
    PlayPause,
    /// Stop playback.
    Stop,
    /// Next track.
    Next,
    /// Previous track.
    Previous,
}

/// Snapshot of one media player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackState {
    /// Player identity.
    pub player: String,
    /// Whether the player is playing.
    pub playing: bool,
    /// Track title.
    pub title: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Position in milliseconds.
    pub position_ms: i64,
    /// Track length in milliseconds.
    pub length_ms: i64,
    /// Player volume, 0-100.
    pub volume: Option<u8>,
}
