//! Static metadata for every plugin in the KDE Connect desktop roster.

use handfast_proto::types;
use handfast_runtime::{DesktopRequirement, PluginMeta, PluginStatus};

const IMPLEMENTED: PluginStatus = PluginStatus::Implemented;
const PLANNED: PluginStatus = PluginStatus::Planned;

/// Battery status.
pub const BATTERY: PluginMeta = PluginMeta {
    name: "battery",
    title: "Battery monitor",
    status: IMPLEMENTED,
    incoming: &[types::BATTERY],
    outgoing: &[],
    desktop: DesktopRequirement::None,
};

/// Clipboard text sync.
pub const CLIPBOARD: PluginMeta = PluginMeta {
    name: "clipboard",
    title: "Clipboard synchronization",
    status: IMPLEMENTED,
    incoming: &[types::CLIPBOARD, types::CLIPBOARD_CONNECT],
    outgoing: &[],
    desktop: DesktopRequirement::Clipboard,
};

/// Phone signal strength.
pub const CONNECTIVITY_REPORT: PluginMeta = PluginMeta {
    name: "connectivity_report",
    title: "Connectivity monitor",
    status: IMPLEMENTED,
    incoming: &[types::CONNECTIVITY_REPORT],
    outgoing: &[types::CONNECTIVITY_REPORT_REQUEST],
    desktop: DesktopRequirement::None,
};

/// Contact synchronization.
pub const CONTACTS: PluginMeta = PluginMeta {
    name: "contacts",
    title: "Sync contacts",
    status: PLANNED,
    incoming: &[
        types::CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
        types::CONTACTS_RESPONSE_VCARDS,
    ],
    outgoing: &[
        types::CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
        types::CONTACTS_REQUEST_VCARDS_BY_UID,
    ],
    desktop: DesktopRequirement::Contacts,
};

/// Pressure-sensitive drawing tablet.
pub const DIGITIZER: PluginMeta = PluginMeta {
    name: "digitizer",
    title: "Drawing tablet",
    status: PLANNED,
    incoming: &[types::DIGITIZER, types::DIGITIZER_SESSION],
    outgoing: &[],
    desktop: DesktopRequirement::Input,
};

/// Ring a remote phone.
pub const FINDMYPHONE: PluginMeta = PluginMeta {
    name: "findmyphone",
    title: "Find other devices",
    status: PLANNED,
    incoming: &[],
    outgoing: &[types::FINDMYPHONE_REQUEST],
    desktop: DesktopRequirement::None,
};

/// Ring this desktop.
pub const FINDTHISDEVICE: PluginMeta = PluginMeta {
    name: "findthisdevice",
    title: "Find this device",
    status: PLANNED,
    incoming: &[types::FINDMYPHONE_REQUEST, types::FINDTHISDEVICE_PRIVATE],
    outgoing: &[],
    desktop: DesktopRequirement::Notifications,
};

/// Remote lock and unlock.
pub const LOCKDEVICE: PluginMeta = PluginMeta {
    name: "lockdevice",
    title: "Lock device",
    status: PLANNED,
    incoming: &[types::LOCK_REQUEST],
    outgoing: &[types::LOCK],
    desktop: DesktopRequirement::Screensaver,
};

/// ModemManager telephony.
pub const MMTELEPHONY: PluginMeta = PluginMeta {
    name: "mmtelephony",
    title: "ModemManager telephony",
    status: PLANNED,
    incoming: &[types::TELEPHONY_REQUEST_MUTE],
    outgoing: &[types::TELEPHONY],
    desktop: DesktopRequirement::Telephony,
};

/// Phone as touchpad and keyboard.
pub const MOUSEPAD: PluginMeta = PluginMeta {
    name: "mousepad",
    title: "Receive remote input",
    status: PLANNED,
    incoming: &[types::MOUSEPAD_REQUEST, types::MOUSEPAD_KEYBOARDSTATE],
    outgoing: &[types::MOUSEPAD_ECHO, types::MOUSEPAD_KEYBOARDSTATE],
    desktop: DesktopRequirement::Input,
};

/// Control desktop media from the phone.
pub const MPRISCONTROL: PluginMeta = PluginMeta {
    name: "mpriscontrol",
    title: "Multimedia control receiver",
    status: PLANNED,
    incoming: &[types::MPRIS_REQUEST],
    outgoing: &[types::MPRIS],
    desktop: DesktopRequirement::Media,
};

/// Control phone media from the desktop.
pub const MPRISREMOTE: PluginMeta = PluginMeta {
    name: "mprisremote",
    title: "Send multimedia control commands",
    status: PLANNED,
    incoming: &[types::MPRIS],
    outgoing: &[types::MPRIS_REQUEST],
    desktop: DesktopRequirement::Media,
};

/// Receive phone notifications.
pub const NOTIFICATIONS: PluginMeta = PluginMeta {
    name: "notifications",
    title: "Receive notifications",
    status: PLANNED,
    incoming: &[
        types::NOTIFICATION,
        types::NOTIFICATION_ACTION,
        types::NOTIFICATION_REPLY,
        types::NOTIFICATION_REQUEST,
    ],
    outgoing: &[
        types::NOTIFICATION,
        types::NOTIFICATION_ACTION,
        types::NOTIFICATION_REPLY,
        types::NOTIFICATION_REQUEST,
    ],
    desktop: DesktopRequirement::Notifications,
};

/// Pause media during calls.
pub const PAUSEMUSIC: PluginMeta = PluginMeta {
    name: "pausemusic",
    title: "Pause media during calls",
    status: PLANNED,
    incoming: &[types::TELEPHONY],
    outgoing: &[types::MPRIS_REQUEST],
    desktop: DesktopRequirement::Media,
};

/// Ping.
pub const PING: PluginMeta = PluginMeta {
    name: "ping",
    title: "Ping",
    status: IMPLEMENTED,
    incoming: &[types::PING],
    outgoing: &[],
    desktop: DesktopRequirement::None,
};

/// Presentation pointer.
pub const PRESENTER: PluginMeta = PluginMeta {
    name: "presenter",
    title: "Presenter",
    status: PLANNED,
    incoming: &[types::PRESENTER],
    outgoing: &[],
    desktop: DesktopRequirement::Input,
};

/// Trigger commands on a remote device.
pub const REMOTECOMMANDS: PluginMeta = PluginMeta {
    name: "remotecommands",
    title: "Send commands to execute",
    status: PLANNED,
    incoming: &[types::RUNCOMMAND_REQUEST],
    outgoing: &[types::RUNCOMMAND],
    desktop: DesktopRequirement::Shell,
};

/// Send pointer input to a remote device.
pub const REMOTECONTROL: PluginMeta = PluginMeta {
    name: "remotecontrol",
    title: "Send remote mouse input",
    status: PLANNED,
    incoming: &[],
    outgoing: &[types::MOUSEPAD_REQUEST],
    desktop: DesktopRequirement::None,
};

/// Send keyboard input to a remote device.
pub const REMOTEKEYBOARD: PluginMeta = PluginMeta {
    name: "remotekeyboard",
    title: "Send remote keyboard input",
    status: PLANNED,
    incoming: &[],
    outgoing: &[types::MOUSEPAD_REQUEST],
    desktop: DesktopRequirement::None,
};

/// Control remote volume.
pub const REMOTESYSTEMVOLUME: PluginMeta = PluginMeta {
    name: "remotesystemvolume",
    title: "Remote volume control",
    status: PLANNED,
    incoming: &[types::SYSTEMVOLUME],
    outgoing: &[types::SYSTEMVOLUME_REQUEST],
    desktop: DesktopRequirement::Volume,
};

/// Execute commands configured on this device.
pub const RUNCOMMAND: PluginMeta = PluginMeta {
    name: "runcommand",
    title: "Execute remote commands",
    status: PLANNED,
    incoming: &[types::RUNCOMMAND_REQUEST],
    outgoing: &[types::RUNCOMMAND, types::RUNCOMMAND_OUTPUT],
    desktop: DesktopRequirement::Shell,
};

/// Inhibit the screensaver while connected.
pub const SCREENSAVER_INHIBIT: PluginMeta = PluginMeta {
    name: "screensaver_inhibit",
    title: "Inhibit screensaver",
    status: PLANNED,
    incoming: &[],
    outgoing: &[],
    desktop: DesktopRequirement::Screensaver,
};

/// Send desktop notifications to the phone.
pub const SENDNOTIFICATIONS: PluginMeta = PluginMeta {
    name: "sendnotifications",
    title: "Send notifications",
    status: PLANNED,
    incoming: &[types::NOTIFICATION_REQUEST],
    outgoing: &[types::NOTIFICATION],
    desktop: DesktopRequirement::Notifications,
};

/// Remote filesystem browser.
pub const SFTP: PluginMeta = PluginMeta {
    name: "sftp",
    title: "Remote filesystem browser",
    status: PLANNED,
    incoming: &[types::SFTP_REQUEST],
    outgoing: &[types::SFTP],
    desktop: DesktopRequirement::Files,
};

/// File, URL and text sharing.
pub const SHARE: PluginMeta = PluginMeta {
    name: "share",
    title: "Share and receive",
    status: PLANNED,
    incoming: &[types::SHARE_REQUEST, types::SHARE_REQUEST_UPDATE],
    outgoing: &[types::SHARE_REQUEST, types::SHARE_REQUEST_UPDATE],
    desktop: DesktopRequirement::Files,
};

/// Share local input devices with a remote device.
pub const SHAREINPUTDEVICES: PluginMeta = PluginMeta {
    name: "shareinputdevices",
    title: "Share input devices",
    status: PLANNED,
    incoming: &[types::SHAREINPUTDEVICES_REQUEST],
    outgoing: &[types::SHAREINPUTDEVICES, types::MOUSEPAD_REQUEST],
    desktop: DesktopRequirement::Input,
};

/// Accept shared input from a remote device.
pub const SHAREINPUTDEVICESREMOTE: PluginMeta = PluginMeta {
    name: "shareinputdevicesremote",
    title: "Receive shared input devices",
    status: PLANNED,
    incoming: &[types::SHAREINPUTDEVICES, types::MOUSEPAD_REQUEST],
    outgoing: &[types::SHAREINPUTDEVICES_REQUEST],
    desktop: DesktopRequirement::Input,
};

/// SMS and MMS.
pub const SMS: PluginMeta = PluginMeta {
    name: "sms",
    title: "SMS integration",
    status: PLANNED,
    incoming: &[
        types::SMS_MESSAGES,
        types::SMS_REQUEST_CONVERSATION,
        types::SMS_REQUEST_CONVERSATIONS,
        types::SMS_REQUEST_ATTACHMENT,
        types::SMS_ATTACHMENT_FILE,
    ],
    outgoing: &[
        types::SMS_MESSAGES,
        types::SMS_REQUEST,
        types::SMS_REQUEST_CONVERSATION,
        types::SMS_REQUEST_CONVERSATIONS,
        types::SMS_REQUEST_ATTACHMENT,
    ],
    desktop: DesktopRequirement::Telephony,
};

/// Receive volume controls from the phone.
pub const SYSTEMVOLUME: PluginMeta = PluginMeta {
    name: "systemvolume",
    title: "Receive volume controls",
    status: PLANNED,
    incoming: &[types::SYSTEMVOLUME_REQUEST],
    outgoing: &[types::SYSTEMVOLUME],
    desktop: DesktopRequirement::Volume,
};

/// Incoming call notifications.
pub const TELEPHONY: PluginMeta = PluginMeta {
    name: "telephony",
    title: "Telephony integration",
    status: PLANNED,
    incoming: &[types::TELEPHONY, types::TELEPHONY_REQUEST_MUTE],
    outgoing: &[types::TELEPHONY_REQUEST_MUTE],
    desktop: DesktopRequirement::Telephony,
};

/// Use the phone as a virtual monitor.
pub const VIRTUALMONITOR: PluginMeta = PluginMeta {
    name: "virtualmonitor",
    title: "Virtual monitor",
    status: PLANNED,
    incoming: &[types::VIRTUALMONITOR, types::VIRTUALMONITOR_REQUEST],
    outgoing: &[types::VIRTUALMONITOR, types::VIRTUALMONITOR_REQUEST],
    desktop: DesktopRequirement::Display,
};

/// Every plugin in the roster, in upstream order.
pub const ALL: &[&PluginMeta] = &[
    &BATTERY,
    &CLIPBOARD,
    &CONNECTIVITY_REPORT,
    &CONTACTS,
    &DIGITIZER,
    &FINDMYPHONE,
    &FINDTHISDEVICE,
    &LOCKDEVICE,
    &MMTELEPHONY,
    &MOUSEPAD,
    &MPRISCONTROL,
    &MPRISREMOTE,
    &NOTIFICATIONS,
    &PAUSEMUSIC,
    &PING,
    &PRESENTER,
    &REMOTECOMMANDS,
    &REMOTECONTROL,
    &REMOTEKEYBOARD,
    &REMOTESYSTEMVOLUME,
    &RUNCOMMAND,
    &SCREENSAVER_INHIBIT,
    &SENDNOTIFICATIONS,
    &SFTP,
    &SHARE,
    &SHAREINPUTDEVICES,
    &SHAREINPUTDEVICESREMOTE,
    &SMS,
    &SYSTEMVOLUME,
    &TELEPHONY,
    &VIRTUALMONITOR,
];

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::packet::is_valid_packet_type;
    use std::collections::HashSet;

    #[test]
    fn roster_has_all_thirty_one_plugins() {
        assert_eq!(ALL.len(), 31);
    }

    #[test]
    fn names_are_unique() {
        let names: HashSet<&str> = ALL.iter().map(|meta| meta.name).collect();
        assert_eq!(names.len(), ALL.len());
    }

    #[test]
    fn implemented_plugins_have_an_incoming_contract() {
        for meta in ALL.iter().filter(|meta| meta.status == IMPLEMENTED) {
            assert!(
                !meta.incoming.is_empty(),
                "{} is implemented but handles no packets",
                meta.name
            );
        }
    }

    #[test]
    fn packet_types_are_namespaced() {
        for meta in ALL {
            for packet_type in meta.incoming.iter().chain(meta.outgoing.iter()) {
                assert!(
                    is_valid_packet_type(packet_type),
                    "{} exposes {packet_type}",
                    meta.name
                );
            }
        }
    }

    #[test]
    fn exactly_four_plugins_are_implemented() {
        let implemented = ALL.iter().filter(|meta| meta.status == IMPLEMENTED).count();
        assert_eq!(implemented, 4);
    }
}
