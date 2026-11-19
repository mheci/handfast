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

/// A pointer button, using the X11 numbering both KDE Connect clients send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerButton {
    /// Primary button.
    Left,
    /// Middle button, usually paste.
    Middle,
    /// Secondary button.
    Right,
}

/// What a pointer event does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerAction {
    /// Relative movement.
    Move,
    /// Scroll wheel movement.
    Scroll,
    /// Press a button and release it after `count` clicks.
    Click {
        /// Button to click.
        button: PointerButton,
        /// 1 for a single click, 2 for a double click.
        count: u8,
    },
    /// Press a button and keep it pressed, for drag and drop.
    Press(PointerButton),
    /// Release a button pressed earlier.
    Release(PointerButton),
}

/// A relative pointer movement, scroll, click or drag step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointerEvent {
    /// Relative X delta.
    pub dx: f64,
    /// Relative Y delta.
    pub dy: f64,
    /// Whether this is a scroll event rather than movement.
    pub scroll: bool,
    /// What the event does. Defaults to movement for older callers.
    pub action: PointerAction,
}

impl PointerEvent {
    /// A relative movement event.
    #[must_use]
    pub const fn movement(dx: f64, dy: f64) -> Self {
        Self {
            dx,
            dy,
            scroll: false,
            action: PointerAction::Move,
        }
    }

    /// A scroll wheel event.
    #[must_use]
    pub const fn scroll(dx: f64, dy: f64) -> Self {
        Self {
            dx,
            dy,
            scroll: true,
            action: PointerAction::Scroll,
        }
    }

    /// A click of `count` clicks on `button`.
    #[must_use]
    pub const fn click(button: PointerButton, count: u8) -> Self {
        Self {
            dx: 0.0,
            dy: 0.0,
            scroll: false,
            action: PointerAction::Click { button, count },
        }
    }

    /// A button press or release.
    #[must_use]
    pub const fn button(button: PointerButton, pressed: bool) -> Self {
        Self {
            dx: 0.0,
            dy: 0.0,
            scroll: false,
            action: if pressed {
                PointerAction::Press(button)
            } else {
                PointerAction::Release(button)
            },
        }
    }
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

/// One telephony event reported by the host's telephony stack.
///
/// Mirrors the fields of `kdeconnect.telephony` so the plugin only has to
/// serialize this, and the backend only has to fill it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEvent {
    /// Event kind, exactly as the wire spells it: `ringing`, `talking`,
    /// `missedCall` or `sms`.
    pub event: String,
    /// Caller identifier, when the stack knows it.
    pub phone_number: Option<String>,
    /// Contact name resolved from the phone number.
    pub contact_name: Option<String>,
    /// Whether the event is a cancellation of the previous one.
    pub is_cancel: bool,
}

/// One contact address book entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRecord {
    /// Stable identifier, unique within the address book.
    pub uid: String,
    /// Last modification time in milliseconds since the Unix epoch.
    pub timestamp_ms: i64,
    /// The vCard document body.
    pub vcard: String,
}

/// Geometry a virtual display should be created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualDisplay {
    /// Requested width in pixels.
    pub width: u32,
    /// Requested height in pixels.
    pub height: u32,
    /// Fractional scale, as a percentage (100 is 1.0).
    pub scale: u16,
}
