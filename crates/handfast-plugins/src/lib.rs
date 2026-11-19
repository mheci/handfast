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
pub mod digitizer;
pub mod findmyphone;
pub mod findthisdevice;
pub mod lockdevice;
pub mod mmtelephony;
pub mod mousepad;
pub mod mpriscontrol;
pub mod mprisremote;
pub mod notifications;
pub mod pausemusic;
pub mod ping;
pub mod presenter;
pub mod remotecommands;
pub mod remoteinput;
pub mod remotesystemvolume;
pub mod roster;
pub mod runcommand;
pub mod screensaver_inhibit;
pub mod sendnotifications;
pub mod sftp;
pub mod share;
pub mod shareinputdevices;
pub mod shareinputdevicesremote;
pub mod sms;
pub mod systemvolume;
pub mod telephony;
pub mod virtualmonitor;

use handfast_runtime::{PluginMeta, Registry};

pub use battery::{BatteryFactory, BatteryPlugin};
pub use clipboard::{ClipboardFactory, ClipboardPlugin};
pub use connectivity_report::{ConnectivityReportFactory, ConnectivityReportPlugin};
pub use contacts::{ContactsFactory, ContactsPlugin};
pub use digitizer::{DigitizerFactory, DigitizerPlugin, DigitizerSession};
pub use findmyphone::{FindMyPhoneFactory, FindMyPhonePlugin};
pub use findthisdevice::{FindThisDeviceFactory, FindThisDevicePlugin};
pub use lockdevice::{LockDeviceFactory, LockDevicePlugin};
pub use mousepad::{MousepadFactory, MousepadPlugin};
pub use mpriscontrol::{MprisControlFactory, MprisControlPlugin};
pub use mmtelephony::{MmTelephonyEvent, MmTelephonyFactory, MmTelephonyPlugin};
pub use mprisremote::{MprisRemoteFactory, MprisRemotePlugin, RemoteError};
pub use notifications::{NotificationsFactory, NotificationsPlugin};
pub use pausemusic::{PauseMusicFactory, PauseMusicPlugin};
pub use ping::{PingFactory, PingPlugin};
pub use presenter::{PresenterFactory, PresenterPlugin};
pub use remotecommands::{
    RemoteCommand, RemoteCommandError, RemoteCommandsFactory, RemoteCommandsPlugin,
};
pub use remoteinput::{
    RemoteControlFactory, RemoteControlPlugin, RemoteInputError, RemoteKeyboardFactory,
    RemoteKeyboardPlugin,
};
pub use remotesystemvolume::{
    RemoteSystemVolumeFactory, RemoteSystemVolumePlugin, RemoteVolumeError,
};
pub use runcommand::{
    parse_commands, ConfiguredCommand, RunCommandError, RunCommandFactory, RunCommandPlugin,
};
pub use screensaver_inhibit::{ScreensaverInhibitFactory, ScreensaverInhibitPlugin};
pub use sendnotifications::{
    OutgoingNotification, SendNotificationsFactory, SendNotificationsPlugin,
};
pub use sftp::{DesktopSftpServer, SftpFactory, SftpPlugin, SftpServer, SftpStart};
pub use share::{ShareFactory, SharePlugin};
pub use shareinputdevices::{ShareInputDevicesFactory, ShareInputDevicesPlugin};
pub use shareinputdevicesremote::{
    ShareInputDevicesRemoteFactory, ShareInputDevicesRemotePlugin, ShareState,
};
pub use sms::{Conversation, Message, SmsFactory, SmsPlugin};
pub use systemvolume::{SystemVolumeFactory, SystemVolumePlugin};
pub use telephony::{IncomingCall, TelephonyFactory, TelephonyPlugin};
pub use virtualmonitor::{RequestedDisplay, VirtualMonitorFactory, VirtualMonitorPlugin};

/// Build the registry of implemented plugins.
#[must_use]
pub fn registry() -> Registry {
    registry_with_sftp(None)
}

/// Build the registry with an SFTP serving backend.
///
/// `sftp` is the one plugin whose behaviour depends on a host-wide resource
/// (an `sshd` process and a port), so the backend is passed in rather than
/// discovered by the plugin.
#[must_use]
pub fn registry_with_sftp(server: Option<std::sync::Arc<dyn sftp::SftpServer>>) -> Registry {
    Registry::new(vec![
        Box::new(ping::PingFactory),
        Box::new(battery::BatteryFactory),
        Box::new(clipboard::ClipboardFactory),
        Box::new(connectivity_report::ConnectivityReportFactory),
        Box::new(pausemusic::PauseMusicFactory),
        Box::new(share::ShareFactory),
        Box::new(mpriscontrol::MprisControlFactory),
        Box::new(mprisremote::MprisRemoteFactory),
        Box::new(notifications::NotificationsFactory),
        Box::new(findthisdevice::FindThisDeviceFactory),
        Box::new(systemvolume::SystemVolumeFactory),
        Box::new(screensaver_inhibit::ScreensaverInhibitFactory),
        Box::new(lockdevice::LockDeviceFactory),
        Box::new(presenter::PresenterFactory),
        Box::new(mousepad::MousepadFactory),
        Box::new(remoteinput::RemoteControlFactory),
        Box::new(remoteinput::RemoteKeyboardFactory),
        Box::new(findmyphone::FindMyPhoneFactory),
        Box::new(remotesystemvolume::RemoteSystemVolumeFactory),
        Box::new(sendnotifications::SendNotificationsFactory),
        Box::new(remotecommands::RemoteCommandsFactory),
        Box::new(runcommand::RunCommandFactory),
        Box::new(match server {
            Some(server) => sftp::SftpFactory::with_server(server),
            None => sftp::SftpFactory::new(),
        }),
        Box::new(digitizer::DigitizerFactory),
        Box::new(shareinputdevices::ShareInputDevicesFactory),
        Box::new(shareinputdevicesremote::ShareInputDevicesRemoteFactory),
        Box::new(contacts::ContactsFactory),
        Box::new(sms::SmsFactory),
        Box::new(telephony::TelephonyFactory),
        Box::new(mmtelephony::MmTelephonyFactory),
        Box::new(virtualmonitor::VirtualMonitorFactory),
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
    use handfast_runtime::{Device, DeviceHost, PluginConfig};

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

    #[test]
    fn runcommand_is_enabled_but_its_allowlist_defaults_to_empty() {
        // The plugin is advertised so a phone can see it, but with no config
        // it must not execute anything. Both halves matter: hiding the plugin
        // would be a capability lie, and an unconfigured allowlist that
        // matched everything would be a remote shell.
        let mock = MockDesktop::new();
        let capabilities = registry().capabilities(&mock.desktop()).unwrap();
        assert!(capabilities.accepts(types::RUNCOMMAND_REQUEST));
        let empty = PluginConfig::new();
        assert!(empty.get_list("runcommand", "commands").is_empty());
    }

    #[test]
    fn mpris_remote_is_advertised_only_with_the_media_adapter() {
        let registry = registry();

        // Without a media adapter the host cannot act on either MPRIS
        // direction, so neither plugin is enabled or advertised.
        let mock = MockDesktop::new();
        let mut without_media = mock.desktop();
        without_media.media = None;
        let capabilities = registry.capabilities(&without_media).unwrap();
        assert!(!capabilities.accepts(types::MPRIS));
        assert!(!DeviceHost::new(device(), &registry, without_media).is_enabled("mprisremote"));

        let with_media = mock.desktop();
        let capabilities = registry.capabilities(&with_media).unwrap();
        assert!(capabilities.accepts(types::MPRIS));
        assert!(DeviceHost::new(device(), &registry, with_media).is_enabled("mprisremote"));
    }
}
