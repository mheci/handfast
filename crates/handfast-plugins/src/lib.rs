//! The 31-plugin KDE Connect roster and the implemented plugins.
//!
//! Only plugins marked `PluginStatus::Implemented` are created by [`registry`]
//! and advertised in identity capabilities. The full roster is available from
//! [`metas`] for the parity matrix, the CLI and future work.

#![deny(missing_docs)]

pub mod battery;
pub mod clipboard;
pub mod connectivity_report;
pub mod contacts;
pub mod findmyphone;
pub mod findthisdevice;
pub mod lockdevice;
pub mod mousepad;
pub mod mpriscontrol;
pub mod mprisremote;
pub mod notifications;
pub mod pausemusic;
pub mod ping;
pub mod presenter;
pub mod remotecommands;
pub mod remotecontrol;
pub mod remotesystemvolume;
pub mod roster;
pub mod runcommand;
pub mod screensaver_inhibit;
pub mod share;
pub mod systemvolume;
pub mod telephony;

use handfast_runtime::{PluginMeta, Registry};

pub use battery::{BatteryFactory, BatteryPlugin};
pub use clipboard::{ClipboardFactory, ClipboardPlugin};
pub use connectivity_report::{ConnectivityReportFactory, ConnectivityReportPlugin};
pub use contacts::{ContactsFactory, ContactsPlugin};
pub use findmyphone::{findmyphone_packet, FindMyPhoneFactory, FindMyPhonePlugin};
pub use findthisdevice::{FindThisDeviceFactory, FindThisDevicePlugin};
pub use lockdevice::{LockDeviceFactory, LockDevicePlugin};
pub use mousepad::{MousepadFactory, MousepadPlugin};
pub use mpriscontrol::{MprisControlFactory, MprisControlPlugin};
pub use mprisremote::{valid_player_name, MprisRemoteFactory, MprisRemotePlugin};
pub use notifications::{NotificationsFactory, NotificationsPlugin};
pub use pausemusic::{PauseMusicFactory, PauseMusicPlugin};
pub use ping::{PingFactory, PingPlugin};
pub use presenter::{PresenterFactory, PresenterPlugin};
pub use remotecommands::{valid_command_key, RemoteCommandsFactory, RemoteCommandsPlugin};
pub use remotecontrol::{
    click_request, key_request, move_request, scroll_request, special_key_request, RemoteClick,
    RemoteControlFactory, RemoteControlPlugin, RemoteInputModifiers, RemoteKeyboardFactory,
    RemoteKeyboardPlugin,
};
pub use remotesystemvolume::{
    mute_request, valid_sink_name, volume_request, RemoteSystemVolumeFactory,
    RemoteSystemVolumePlugin,
};
pub use runcommand::{RunCommandFactory, RunCommandPlugin};
pub use screensaver_inhibit::{ScreensaverInhibitFactory, ScreensaverInhibitPlugin};
pub use share::{ShareFactory, SharePlugin};
pub use systemvolume::{SystemVolumeFactory, SystemVolumePlugin};
pub use telephony::{TelephonyFactory, TelephonyPlugin};

/// Build the registry of implemented plugins.
#[must_use]
pub fn registry() -> Registry {
    Registry::new(vec![
        Box::new(ping::PingFactory),
        Box::new(battery::BatteryFactory),
        Box::new(clipboard::ClipboardFactory),
        Box::new(connectivity_report::ConnectivityReportFactory),
        Box::new(contacts::ContactsFactory),
        Box::new(pausemusic::PauseMusicFactory),
        Box::new(share::ShareFactory),
        Box::new(mpriscontrol::MprisControlFactory),
        Box::new(mprisremote::MprisRemoteFactory),
        Box::new(notifications::NotificationsFactory),
        Box::new(findthisdevice::FindThisDeviceFactory),
        Box::new(findmyphone::FindMyPhoneFactory),
        Box::new(systemvolume::SystemVolumeFactory),
        Box::new(remotesystemvolume::RemoteSystemVolumeFactory),
        Box::new(runcommand::RunCommandFactory),
        Box::new(remotecommands::RemoteCommandsFactory),
        Box::new(remotecontrol::RemoteControlFactory),
        Box::new(remotecontrol::RemoteKeyboardFactory),
        Box::new(screensaver_inhibit::ScreensaverInhibitFactory),
        Box::new(lockdevice::LockDeviceFactory),
        Box::new(presenter::PresenterFactory),
        Box::new(mousepad::MousepadFactory),
        Box::new(telephony::TelephonyFactory),
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
    fn remote_system_volume_needs_no_desktop_adapter() {
        let registry = registry();
        let desktop = MockDesktop::new();
        let capabilities = registry.capabilities(&desktop.desktop()).unwrap();

        assert!(capabilities.accepts(types::SYSTEMVOLUME));
        assert!(capabilities.accepts(types::SYSTEMVOLUME_REQUEST));
        assert!(DeviceHost::new(device(), &registry, desktop.desktop())
            .is_enabled("remotesystemvolume"));
    }

    #[test]
    fn remote_commands_need_no_desktop_adapter() {
        let registry = registry();
        let desktop = MockDesktop::new();
        let capabilities = registry.capabilities(&desktop.desktop()).unwrap();

        assert!(capabilities.accepts(types::RUNCOMMAND));
        assert!(capabilities.outgoing().contains(types::RUNCOMMAND_REQUEST));
        assert!(
            DeviceHost::new(device(), &registry, desktop.desktop()).is_enabled("remotecommands")
        );
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
