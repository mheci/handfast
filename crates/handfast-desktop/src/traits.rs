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
///
/// The default sink is the simple path used by the `systemvolume` plugin. The
/// per-sink methods back the `sinkList` exchange: `sinks` enumerates the
/// outputs, and `set_sink` applies a request addressed to one of them.
#[async_trait]
pub trait Volume: Send + Sync {
    /// Read the current output volume.
    async fn get(&self) -> Result<VolumeState>;

    /// Set output volume and mute state.
    async fn set(&self, percent: u8, muted: bool) -> Result<()>;

    /// Enumerate the controllable output sinks.
    ///
    /// The default implementation reports the single default sink, so an
    /// adapter that only implements `get`/`set` still speaks `sinkList`.
    ///
    /// # Errors
    ///
    /// Fails when the backend cannot read the default sink.
    async fn sinks(&self) -> Result<Vec<AudioSink>> {
        let state = self.get().await?;
        Ok(vec![AudioSink {
            name: "default".to_string(),
            description: "System volume".to_string(),
            volume: i64::from(state.percent),
            max_volume: 100,
            muted: state.muted,
            enabled: true,
        }])
    }

    /// Apply a volume and/or mute change to one sink.
    ///
    /// `volume` and `muted` are independent: either may be absent. The default
    /// implementation routes to `set` when the sink is reported by `sinks`.
    ///
    /// # Errors
    ///
    /// Fails when the sink is unknown or the backend rejects the change.
    async fn set_sink(&self, name: &str, volume: Option<i64>, muted: Option<bool>) -> Result<()> {
        let sinks = self.sinks().await?;
        let Some(sink) = sinks.iter().find(|sink| sink.name == name) else {
            return Err(crate::error::DesktopError::Backend(format!(
                "unknown audio sink {name}"
            )));
        };
        let percent = match volume {
            Some(volume) => {
                if sink.max_volume <= 0 {
                    0
                } else {
                    ((volume.clamp(0, sink.max_volume) * 100) / sink.max_volume) as u8
                }
            }
            None => {
                if sink.max_volume <= 0 {
                    0
                } else {
                    ((sink.volume.clamp(0, sink.max_volume) * 100) / sink.max_volume) as u8
                }
            }
        };
        self.set(percent, muted.unwrap_or(sink.muted)).await
    }
}

/// Host battery state.
///
/// Reported to the peer by the `battery` plugin. `charge` is -1 when the host
/// has no battery, matching the wire field `currentCharge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryStatus {
    /// Charge percentage 0-100, or -1 when there is no battery.
    pub charge: i32,
    /// Whether the host is charging.
    pub charging: bool,
    /// Low-battery threshold event: 0 none, 1 low.
    pub threshold: u8,
}

impl Default for BatteryStatus {
    /// A plausible desk-side default used by the mock backend: no battery,
    /// not charging, no threshold event.
    fn default() -> Self {
        Self {
            charge: -1,
            charging: false,
            threshold: 0,
        }
    }
}

/// Host battery reporting.
#[async_trait]
pub trait Battery: Send + Sync {
    /// Read the current host battery state.
    async fn status(&self) -> Result<BatteryStatus>;
}

/// A fullscreen presentation view for the presenter plugin.
///
/// Entering presents a marker on screen and moving it; [`Self::stop`] closes
/// the view. Coordinates are normalised 0.0-1.0 on each axis.
#[async_trait]
pub trait Presenter: Send + Sync {
    /// Show the view if needed and move the marker to `(x, y)`.
    async fn present(&self, x: f32, y: f32) -> Result<()>;

    /// Close the view.
    async fn stop(&self) -> Result<()>;
}

/// One controllable audio output sink.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioSink {
    /// Stable backend name, sent on the wire.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Current volume in backend units.
    pub volume: i64,
    /// Backend maximum volume.
    pub max_volume: i64,
    /// Whether the sink is muted.
    pub muted: bool,
    /// Whether the sink is the default output.
    pub enabled: bool,
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
