//! The 31-plugin KDE Connect roster and the implemented plugins.
//!
//! Only plugins with [`PluginStatus::Implemented`] are created by [`registry`]
//! and advertised in identity capabilities. The full roster is available from
//! [`metas`] for the parity matrix, the CLI and future work.

#![deny(missing_docs)]

pub mod battery;
pub mod clipboard;
pub mod ping;
pub mod roster;

use handfast_runtime::{PluginMeta, Registry};

pub use battery::{BatteryFactory, BatteryPlugin};
pub use clipboard::{ClipboardFactory, ClipboardPlugin};
pub use ping::{PingFactory, PingPlugin};

/// Build the registry of implemented plugins.
#[must_use]
pub fn registry() -> Registry {
    Registry::new(vec![
        Box::new(ping::PingFactory),
        Box::new(battery::BatteryFactory),
        Box::new(clipboard::ClipboardFactory),
    ])
}

/// The full KDE Connect plugin roster, including planned plugins.
#[must_use]
pub fn metas() -> &'static [&'static PluginMeta] {
    roster::ALL
}
