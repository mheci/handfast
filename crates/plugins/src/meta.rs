//! Compile-time [`PluginMeta`] constants for the full Phase-1 roster.
//!
//! Packet type names mirror the upstream kdeconnect-kde wire protocol. Flags
//! (`requires_wayland`, `requires_dbus`, `default_enabled`) reflect the
//! intended Phase-3 desktop integration and may be revisited then.

use crate::PluginMeta;

/// Battery status reporting.
pub const BATTERY: PluginMeta = PluginMeta {
    name: "battery",
    title: "Battery report",
    incoming: &["kdeconnect.battery"],
    outgoing: &["kdeconnect.battery.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// Clipboard synchronization between devices.
pub const CLIPBOARD: PluginMeta = PluginMeta {
    name: "clipboard",
    title: "Clipboard sync",
    incoming: &["kdeconnect.clipboard"],
    outgoing: &["kdeconnect.clipboard", "kdeconnect.clipboard.connect"],
    default_enabled: true,
    requires_wayland: true,
    requires_dbus: false,
};

/// Cellular/Wi-Fi connectivity state of the remote device.
pub const CONNECTIVITY_REPORT: PluginMeta = PluginMeta {
    name: "connectivity_report",
    title: "Connectivity report",
    incoming: &["kdeconnect.connectivity_report"],
    outgoing: &["kdeconnect.connectivity_report.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// Contact list exchange used by SMS and telephony surfaces.
pub const CONTACTS: PluginMeta = PluginMeta {
    name: "contacts",
    title: "Contacts",
    incoming: &[
        "kdeconnect.contacts.response_uids_timestamps",
        "kdeconnect.contacts.response_vcards",
    ],
    outgoing: &[
        "kdeconnect.contacts.request_all_uids",
        "kdeconnect.contacts.request_uids_by_timestamp",
        "kdeconnect.contacts.request_vcards_by_uid",
    ],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// Ring the paired phone to locate it.
pub const FINDMYPHONE: PluginMeta = PluginMeta {
    name: "findmyphone",
    title: "Find my phone",
    incoming: &["kdeconnect.findmyphone.request"],
    outgoing: &["kdeconnect.findmyphone.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// Remote keyboard/pointer input from the phone.
pub const MOUSEPAD: PluginMeta = PluginMeta {
    name: "mousepad",
    title: "Remote input",
    incoming: &[
        "kdeconnect.mousepad.request",
        "kdeconnect.mousepad.keyboardstate",
    ],
    outgoing: &[
        "kdeconnect.mousepad.echo",
        "kdeconnect.mousepad.keyboardstate",
    ],
    default_enabled: true,
    requires_wayland: true,
    requires_dbus: false,
};

/// Media player enumeration and transport control.
pub const MPRIS: PluginMeta = PluginMeta {
    name: "mpris",
    title: "Media player control",
    incoming: &["kdeconnect.mpris", "kdeconnect.mpris.request"],
    outgoing: &["kdeconnect.mpris", "kdeconnect.mpris.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: true,
};

/// Desktop notification forwarding.
pub const NOTIFICATIONS: PluginMeta = PluginMeta {
    name: "notifications",
    title: "Notifications",
    incoming: &[
        "kdeconnect.notification",
        "kdeconnect.notification.reply",
        "kdeconnect.notification.request",
    ],
    outgoing: &[
        "kdeconnect.notification",
        "kdeconnect.notification.reply",
        "kdeconnect.notification.request",
    ],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: true,
};

/// Pause local media players while a call is active.
pub const PAUSE_MUSIC: PluginMeta = PluginMeta {
    name: "pause_music",
    title: "Pause music on call",
    incoming: &["kdeconnect.telephony"],
    outgoing: &["kdeconnect.mpris.request"],
    default_enabled: false,
    requires_wayland: false,
    requires_dbus: true,
};

/// Latency probe; fully implemented in Phase 1 (see [`crate::ping`]).
pub const PING: PluginMeta = PluginMeta {
    name: "ping",
    title: "Ping",
    incoming: &["kdeconnect.ping"],
    outgoing: &["kdeconnect.ping"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// Remotely execute user-defined commands on this device.
pub const RUN_COMMANDS: PluginMeta = PluginMeta {
    name: "run_commands",
    title: "Run commands",
    incoming: &["kdeconnect.runcommand", "kdeconnect.runcommand.request"],
    outgoing: &["kdeconnect.runcommand", "kdeconnect.runcommand.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// SFTP mount exposure for the paired device.
pub const REMOTE_FILESYSTEM: PluginMeta = PluginMeta {
    name: "remote_filesystem",
    title: "Remote filesystem",
    incoming: &["kdeconnect.sftp"],
    outgoing: &["kdeconnect.sftp"],
    default_enabled: false,
    requires_wayland: false,
    requires_dbus: false,
};

/// File/URL/transfer sharing between devices.
pub const SHARE: PluginMeta = PluginMeta {
    name: "share",
    title: "Share",
    incoming: &["kdeconnect.share.request"],
    outgoing: &["kdeconnect.share.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// SMS message listing and sending.
pub const SMS: PluginMeta = PluginMeta {
    name: "sms",
    title: "SMS messages",
    incoming: &["kdeconnect.sms.messages", "kdeconnect.sms.request"],
    outgoing: &["kdeconnect.sms.messages", "kdeconnect.sms.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// System-wide output volume control.
pub const SYSTEM_VOLUME: PluginMeta = PluginMeta {
    name: "system_volume",
    title: "System volume",
    incoming: &["kdeconnect.systemvolume", "kdeconnect.systemvolume.request"],
    outgoing: &["kdeconnect.systemvolume", "kdeconnect.systemvolume.request"],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: false,
};

/// Call/sms event surfacing from the phone.
pub const TELEPHONY: PluginMeta = PluginMeta {
    name: "telephony",
    title: "Telephony",
    incoming: &["kdeconnect.telephony"],
    outgoing: &[],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: true,
};

/// Synthetic input events injected into the desktop session.
pub const VIRTUAL_INPUT: PluginMeta = PluginMeta {
    name: "virtual_input",
    title: "Virtual input",
    incoming: &["kdeconnect.virtualinput"],
    outgoing: &["kdeconnect.virtualinput"],
    default_enabled: true,
    requires_wayland: true,
    requires_dbus: false,
};

/// Every roster entry, in the same deterministic order as
/// [`crate::registry`].
pub const ALL: &[&PluginMeta] = &[
    &BATTERY,
    &CLIPBOARD,
    &CONNECTIVITY_REPORT,
    &CONTACTS,
    &FINDMYPHONE,
    &MOUSEPAD,
    &MPRIS,
    &NOTIFICATIONS,
    &PAUSE_MUSIC,
    &PING,
    &RUN_COMMANDS,
    &REMOTE_FILESYSTEM,
    &SHARE,
    &SMS,
    &SYSTEM_VOLUME,
    &TELEPHONY,
    &VIRTUAL_INPUT,
];
