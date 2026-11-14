//! The 31-plugin KDE Connect roster and the implemented plugins.
//!
//! Only plugins marked `PluginStatus::Implemented` are created by [`registry`]
//! and advertised in identity capabilities. The full roster is available from
//! [`metas`] for the parity matrix, the CLI and future work.

#![deny(missing_docs)]

pub mod battery;
pub mod clipboard;
pub mod connectivity_report;
pub mod findthisdevice;
pub mod mpriscontrol;
pub mod notifications;
pub mod pausemusic;
pub mod ping;
pub mod roster;
pub mod share;

use handfast_runtime::{PluginMeta, Registry};

pub use battery::{BatteryFactory, BatteryPlugin};
pub use clipboard::{ClipboardFactory, ClipboardPlugin};
pub use connectivity_report::{ConnectivityReportFactory, ConnectivityReportPlugin};
pub use findthisdevice::{FindThisDeviceFactory, FindThisDevicePlugin};
pub use mpriscontrol::{MprisControlFactory, MprisControlPlugin};
pub use notifications::{NotificationsFactory, NotificationsPlugin};
pub use pausemusic::{PauseMusicFactory, PauseMusicPlugin};
pub use ping::{PingFactory, PingPlugin};
pub use share::{ShareFactory, SharePlugin};

/// Build the registry of implemented plugins.
#[must_use]
pub fn registry() -> Registry {
    Registry::new(vec![
        Box::new(ping::PingFactory),
        Box::new(battery::BatteryFactory),
        Box::new(clipboard::ClipboardFactory),
        Box::new(connectivity_report::ConnectivityReportFactory),
        Box::new(pausemusic::PauseMusicFactory),
        Box::new(share::ShareFactory),
        Box::new(mpriscontrol::MprisControlFactory),
        Box::new(notifications::NotificationsFactory),
        Box::new(findthisdevice::FindThisDeviceFactory),
    ])
}

/// The full KDE Connect plugin roster, including planned plugins.
#[must_use]
pub fn metas() -> &'static [&'static PluginMeta] {
    roster::ALL
}

#[cfg(test)]
mod tests {
    use handfast_desktop::MockDesktop;
    use handfast_proto::{types, DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, DeviceHost};

    use super::registry;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Desktop,
        )
    }

    #[test]
    fn share_is_advertised_only_with_files_adapter() {
        let registry = registry();

        let without_files = MockDesktop::new();
        let capabilities = registry.capabilities(&without_files.desktop()).unwrap();
        assert!(!capabilities.accepts(types::SHARE_REQUEST));
        assert!(!capabilities.accepts(types::SHARE_REQUEST_UPDATE));
        assert!(!DeviceHost::new(device(), &registry, without_files.desktop()).is_enabled("share"));

        let with_files = MockDesktop::with_files().unwrap();
        let capabilities = registry.capabilities(&with_files.desktop()).unwrap();
        assert!(capabilities.accepts(types::SHARE_REQUEST));
        assert!(capabilities.accepts(types::SHARE_REQUEST_UPDATE));
        assert!(DeviceHost::new(device(), &registry, with_files.desktop()).is_enabled("share"));
    }
}
