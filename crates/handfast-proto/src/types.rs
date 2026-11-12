//! Packet type constants used by KDE Connect protocol version 8.
//!
//! Constants are grouped by plugin. The list is intentionally data, not an
//! enum: peers may introduce new packet types and Handfast must be able to
//! ignore them without failing the link.

/// Application-level packet used by some clients during startup.
pub const APP: &str = "kdeconnect.app";
/// Device identity exchange.
pub const IDENTITY: &str = "kdeconnect.identity";
/// Pairing request, accept or reject.
pub const PAIR: &str = "kdeconnect.pair";

/// Battery status report.
pub const BATTERY: &str = "kdeconnect.battery";
/// Battery status request.
pub const BATTERY_REQUEST: &str = "kdeconnect.battery.request";

/// Clipboard text update.
pub const CLIPBOARD: &str = "kdeconnect.clipboard";
/// Clipboard state sent when a device connects.
pub const CLIPBOARD_CONNECT: &str = "kdeconnect.clipboard.connect";
/// Clipboard file URI update.
pub const CLIPBOARD_FILE: &str = "kdeconnect.clipboard.file";

/// Connectivity report.
pub const CONNECTIVITY_REPORT: &str = "kdeconnect.connectivity_report";
/// Connectivity report request.
pub const CONNECTIVITY_REPORT_REQUEST: &str = "kdeconnect.connectivity_report.request";

/// Contact UID and timestamp request.
pub const CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS: &str =
    "kdeconnect.contacts.request_all_uids_timestamps";
/// Contact vCard request.
pub const CONTACTS_REQUEST_VCARDS_BY_UID: &str = "kdeconnect.contacts.request_vcards_by_uid";
/// Contact UID and timestamp response.
pub const CONTACTS_RESPONSE_UIDS_TIMESTAMPS: &str = "kdeconnect.contacts.response_uids_timestamps";
/// Contact vCard response.
pub const CONTACTS_RESPONSE_VCARDS: &str = "kdeconnect.contacts.response_vcards";

/// Digitizer input event.
pub const DIGITIZER: &str = "kdeconnect.digitizer";
/// Digitizer session lifecycle.
pub const DIGITIZER_SESSION: &str = "kdeconnect.digitizer.session";

/// Find-my-phone request.
pub const FINDMYPHONE_REQUEST: &str = "kdeconnect.findmyphone.request";
/// Private find-this-device request.
pub const FINDTHISDEVICE_PRIVATE: &str = "kdeconnect.private.findthisdevice";

/// Lock state report.
pub const LOCK: &str = "kdeconnect.lock";
/// Lock state request.
pub const LOCK_REQUEST: &str = "kdeconnect.lock.request";

/// Mousepad pointer or keyboard event.
pub const MOUSEPAD_REQUEST: &str = "kdeconnect.mousepad.request";
/// Mousepad acknowledgement.
pub const MOUSEPAD_ECHO: &str = "kdeconnect.mousepad.echo";
/// Mousepad keyboard state.
pub const MOUSEPAD_KEYBOARDSTATE: &str = "kdeconnect.mousepad.keyboardstate";

/// MPRIS player state.
pub const MPRIS: &str = "kdeconnect.mpris";
/// MPRIS control request.
pub const MPRIS_REQUEST: &str = "kdeconnect.mpris.request";

/// Notification state.
pub const NOTIFICATION: &str = "kdeconnect.notification";
/// Notification action activation.
pub const NOTIFICATION_ACTION: &str = "kdeconnect.notification.action";
/// Notification reply.
pub const NOTIFICATION_REPLY: &str = "kdeconnect.notification.reply";
/// Notification request.
pub const NOTIFICATION_REQUEST: &str = "kdeconnect.notification.request";

/// Ping.
pub const PING: &str = "kdeconnect.ping";

/// Presenter pointer event.
pub const PRESENTER: &str = "kdeconnect.presenter";

/// Run-command list.
pub const RUNCOMMAND: &str = "kdeconnect.runcommand";
/// Run-command execution request.
pub const RUNCOMMAND_REQUEST: &str = "kdeconnect.runcommand.request";
/// Run-command output.
pub const RUNCOMMAND_OUTPUT: &str = "kdeconnect.runcommand.output";

/// SFTP login information.
pub const SFTP: &str = "kdeconnect.sftp";
/// SFTP start request.
pub const SFTP_REQUEST: &str = "kdeconnect.sftp.request";

/// File, URL or text share request.
pub const SHARE_REQUEST: &str = "kdeconnect.share.request";
/// Multi-file share metadata.
pub const SHARE_REQUEST_UPDATE: &str = "kdeconnect.share.request.update";

/// Shared input device state.
pub const SHAREINPUTDEVICES: &str = "kdeconnect.shareinputdevices";
/// Shared input device request.
pub const SHAREINPUTDEVICES_REQUEST: &str = "kdeconnect.shareinputdevices.request";

/// SMS message list.
pub const SMS_MESSAGES: &str = "kdeconnect.sms.messages";
/// SMS send request.
pub const SMS_REQUEST: &str = "kdeconnect.sms.request";
/// SMS conversation request.
pub const SMS_REQUEST_CONVERSATION: &str = "kdeconnect.sms.request_conversation";
/// SMS conversations request.
pub const SMS_REQUEST_CONVERSATIONS: &str = "kdeconnect.sms.request_conversations";
/// SMS attachment request.
pub const SMS_REQUEST_ATTACHMENT: &str = "kdeconnect.sms.request_attachment";
/// SMS attachment transfer.
pub const SMS_ATTACHMENT_FILE: &str = "kdeconnect.sms.attachment_file";

/// System volume state.
pub const SYSTEMVOLUME: &str = "kdeconnect.systemvolume";
/// System volume request.
pub const SYSTEMVOLUME_REQUEST: &str = "kdeconnect.systemvolume.request";

/// Telephony state.
pub const TELEPHONY: &str = "kdeconnect.telephony";
/// Telephony mute request.
pub const TELEPHONY_REQUEST_MUTE: &str = "kdeconnect.telephony.request_mute";

/// Virtual monitor state.
pub const VIRTUALMONITOR: &str = "kdeconnect.virtualmonitor";
/// Virtual monitor request.
pub const VIRTUALMONITOR_REQUEST: &str = "kdeconnect.virtualmonitor.request";

/// Every known packet type, for validation tests and capability checks.
pub const ALL: &[&str] = &[
    APP,
    IDENTITY,
    PAIR,
    BATTERY,
    BATTERY_REQUEST,
    CLIPBOARD,
    CLIPBOARD_CONNECT,
    CLIPBOARD_FILE,
    CONNECTIVITY_REPORT,
    CONNECTIVITY_REPORT_REQUEST,
    CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
    CONTACTS_REQUEST_VCARDS_BY_UID,
    CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
    CONTACTS_RESPONSE_VCARDS,
    DIGITIZER,
    DIGITIZER_SESSION,
    FINDMYPHONE_REQUEST,
    FINDTHISDEVICE_PRIVATE,
    LOCK,
    LOCK_REQUEST,
    MOUSEPAD_REQUEST,
    MOUSEPAD_ECHO,
    MOUSEPAD_KEYBOARDSTATE,
    MPRIS,
    MPRIS_REQUEST,
    NOTIFICATION,
    NOTIFICATION_ACTION,
    NOTIFICATION_REPLY,
    NOTIFICATION_REQUEST,
    PING,
    PRESENTER,
    RUNCOMMAND,
    RUNCOMMAND_REQUEST,
    RUNCOMMAND_OUTPUT,
    SFTP,
    SFTP_REQUEST,
    SHARE_REQUEST,
    SHARE_REQUEST_UPDATE,
    SHAREINPUTDEVICES,
    SHAREINPUTDEVICES_REQUEST,
    SMS_MESSAGES,
    SMS_REQUEST,
    SMS_REQUEST_CONVERSATION,
    SMS_REQUEST_CONVERSATIONS,
    SMS_REQUEST_ATTACHMENT,
    SMS_ATTACHMENT_FILE,
    SYSTEMVOLUME,
    SYSTEMVOLUME_REQUEST,
    TELEPHONY,
    TELEPHONY_REQUEST_MUTE,
    VIRTUALMONITOR,
    VIRTUALMONITOR_REQUEST,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_types_are_unique_and_namespaced() {
        let unique: HashSet<&str> = ALL.iter().copied().collect();
        assert_eq!(unique.len(), ALL.len(), "duplicate packet type constant");
        for ty in ALL {
            assert!(
                crate::packet::is_valid_packet_type(ty),
                "invalid packet type {ty}"
            );
        }
    }
}
