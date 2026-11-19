//! Adapter traits implemented by Linux desktop backends.

use async_trait::async_trait;

use crate::error::Result;
use crate::model::{
    CallEvent, ContactRecord, KeyEvent, MediaAction, Notification, PlaybackState, PointerEvent,
    VirtualDisplay, VolumeState,
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

    /// Move the pointer to an absolute position, expressed as a fraction of
    /// the screen in 0.0-1.0.
    ///
    /// Separate from [`Input::pointer`] because relative movement cannot
    /// express a tablet sample: a digitizer event names a position on a
    /// surface, not a delta. Backends that cannot address the pointer
    /// absolutely return `Unsupported`, which the digitizer plugin reports
    /// once instead of failing every sample.
    ///
    /// `pressed` holds the primary button down, for a continuous stroke.
    ///
    /// # Errors
    /// Fails when the backend cannot position the pointer.
    async fn pointer_absolute(&self, _x: f64, _y: f64, _pressed: bool) -> Result<()> {
        Err(crate::error::DesktopError::Unsupported(
            "this backend cannot position the pointer absolutely",
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

/// Telephony state changes from the host's phone stack.
///
/// On a desktop this is usually absent: a Linux host has no modem. The trait
/// exists so a Linux host with ModemManager, or a future mobile build, can
/// report calls without the plugin knowing how they were detected.
#[async_trait]
pub trait Telephony: Send + Sync {
    /// Events emitted since the last call, oldest first.
    ///
    /// Polled once per plugin tick rather than pushed, so the backend never
    /// has to hold a reference into the plugin.
    async fn poll(&self) -> Result<Vec<CallEvent>>;
}

/// Address book storage.
#[async_trait]
pub trait Contacts: Send + Sync {
    /// Uid and modification time for every contact, for the change handshake.
    async fn list(&self) -> Result<Vec<(String, i64)>>;

    /// Fetch the vCards for the given uids. Unknown uids are skipped.
    async fn vcards(&self, uids: &[String]) -> Result<Vec<ContactRecord>>;
}

/// Virtual display creation and removal.
#[async_trait]
pub trait Display: Send + Sync {
    /// Create or update the virtual display.
    async fn enable(&self, display: VirtualDisplay) -> Result<()>;

    /// Remove the virtual display.
    async fn disable(&self) -> Result<()>;

    /// Whether a virtual display currently exists.
    async fn is_enabled(&self) -> Result<bool>;
}
