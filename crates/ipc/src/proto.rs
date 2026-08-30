//! Protocol message types exchanged over the IPC socket.

use serde::{Deserialize, Serialize};

/// Requests a local client may send to the daemon.
///
/// Serialized as `{"method": "<variant>", "params": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Query daemon identity and capabilities.
    DaemonInfo,
    /// List all known devices.
    DeviceList,
    /// Start pairing with a discovered device.
    DevicePair {
        /// Target device identifier.
        device_id: String,
    },
    /// Revoke an existing pairing.
    DeviceUnpair {
        /// Target device identifier.
        device_id: String,
    },
    /// List plugins available for a device together with their enabled state.
    PluginList {
        /// Target device identifier.
        device_id: String,
    },
    /// Enable or disable one plugin on a device.
    PluginSetEnabled {
        /// Target device identifier.
        device_id: String,
        /// Plugin identifier.
        plugin: String,
        /// Desired enabled state.
        enabled: bool,
    },
    /// Send a local file to a device.
    SendFile {
        /// Target device identifier.
        device_id: String,
        /// Absolute path of the file to transfer.
        path: String,
    },
    /// List notifications currently shown on this machine.
    NotificationList,
    /// Dismiss one notification by id.
    NotificationDismiss {
        /// Notification identifier.
        notification_id: String,
    },
    /// Read the local clipboard text.
    ClipboardGet,
    /// Overwrite the local clipboard text.
    ClipboardSet {
        /// New clipboard content.
        text: String,
    },
    /// Liveness probe; answers with [`Response::ok_json`] and no payload.
    Ping,
    /// List all transfers (active and finished).
    TransferList,
    /// Cancel an ongoing transfer.
    TransferCancel {
        /// Transfer identifier.
        transfer_id: String,
    },
    /// List commands available for a device.
    RunCommandList {
        /// Target device identifier.
        device_id: String,
    },
    /// Run one named command on a device.
    RunCommand {
        /// Target device identifier.
        device_id: String,
        /// Command identifier from [`Request::RunCommandList`].
        command_name: String,
    },
    /// Set the local output volume.
    SetVolume {
        /// Desired volume percentage (0-100).
        percent: u8,
    },
    /// Query the local output volume.
    GetVolume,
    /// Share local text with a device.
    ShareText {
        /// Target device identifier.
        device_id: String,
        /// Text payload.
        text: String,
    },
    /// Open a URL on a device.
    ShareUrl {
        /// Target device identifier.
        device_id: String,
        /// URL to open.
        url: String,
    },
    /// Ask a device for its current battery state.
    RequestBattery {
        /// Target device identifier.
        device_id: String,
    },
    /// Send an SMS from a paired phone.
    SendSms {
        /// Target device identifier.
        device_id: String,
        /// Recipient phone number.
        number: String,
        /// Message body text.
        text: String,
    },
}

/// Replies sent by the daemon for every [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Successful reply carrying an arbitrary JSON result.
    Ok {
        /// Result payload (`null` when empty).
        result: serde_json::Value,
    },
    /// Failure reply with a machine-readable code and human message.
    Err {
        /// Numeric error code (application defined).
        code: i32,
        /// Human-readable description.
        message: String,
    },
}

impl Response {
    /// Build a successful response wrapping `v`.
    #[must_use]
    pub fn ok_json(v: serde_json::Value) -> Self {
        Response::Ok { result: v }
    }

    /// Build an error response.
    #[must_use]
    pub fn err(code: i32, msg: impl Into<String>) -> Self {
        Response::Err {
            code,
            message: msg.into(),
        }
    }
}

/// Server-pushed events streamed to clients alongside responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ServerEvent {
    /// First frame after connecting; identifies the daemon.
    Hello {
        /// Wire protocol version ([`crate::IPC_VERSION`]).
        version: u32,
        /// Application name.
        app: String,
        /// Daemon process id.
        pid: u32,
    },
    /// A device announced itself on the network.
    DeviceFound {
        /// Stable device identifier.
        id: String,
        /// Human-readable device name.
        name: String,
    },
    /// A previously visible device disappeared.
    DeviceLost {
        /// Device identifier.
        id: String,
    },
    /// Connectivity state changed.
    DeviceStateChanged {
        /// Device identifier.
        id: String,
        /// New state label.
        state: String,
    },
    /// Progress update for an ongoing transfer.
    TransferProgress {
        /// Transfer identifier.
        id: String,
        /// Bytes transferred so far.
        bytes_done: u64,
        /// Total transfer size in bytes.
        bytes_total: u64,
    },
    /// A new transfer was registered.
    TransferAdded {
        /// Transfer identifier.
        id: String,
        /// Peer device identifier.
        device_id: String,
        /// Direction label (`"outgoing"` or `"incoming"`).
        direction: String,
        /// Name of the transferred file.
        file_name: String,
        /// Total transfer size in bytes.
        total: u64,
    },
    /// A transfer completed successfully.
    TransferFinished {
        /// Transfer identifier.
        id: String,
    },
    /// A transfer failed or was cancelled.
    TransferFailed {
        /// Transfer identifier.
        id: String,
        /// Human-readable failure reason.
        reason: String,
    },
    /// An incoming notification arrived on a paired device.
    NotificationReceived {
        /// Notification identifier.
        id: String,
        /// Originating application name.
        app: String,
        /// Notification title.
        title: String,
        /// Notification body text.
        body: String,
    },
    /// Remote clipboard content was received.
    ClipboardUpdated {
        /// Clipboard text.
        text: String,
    },
    /// Structured log record forwarded from the daemon.
    LogRecord {
        /// Severity label.
        level: String,
        /// Rendered message.
        msg: String,
    },
    /// A paired device reported new battery state.
    BatteryChanged {
        /// Device identifier.
        device_id: String,
        /// Battery percentage (0-100).
        level: u8,
        /// Whether the device is currently charging.
        charging: bool,
    },
    /// A paired phone reported a telephony state change.
    TelephonyEvent {
        /// Device identifier.
        device_id: String,
        /// New state label (`"ringing"`, `"talking"`, …).
        state: String,
        /// Remote number when known.
        number: Option<String>,
    },
    /// Local output volume changed.
    VolumeChanged {
        /// Volume percentage (0-100).
        percent: u8,
        /// Whether output is muted.
        muted: bool,
    },
    /// Result of a remotely executed command.
    CommandResult {
        /// Device identifier.
        device_id: String,
        /// Command identifier from [`Request::RunCommandList`].
        name: String,
        /// Whether execution succeeded.
        success: bool,
        /// Captured command output.
        output: String,
    },
    /// The daemon is shutting down.
    DaemonShutdown,
}

impl From<handfast_core::bus::Event> for ServerEvent {
    fn from(event: handfast_core::bus::Event) -> Self {
        use handfast_core::bus::Event;
        match event {
            Event::DeviceFound { id, name } => ServerEvent::DeviceFound { id, name },
            Event::DeviceLost { id } => ServerEvent::DeviceLost { id },
            Event::DeviceStateChanged { id, state } => {
                ServerEvent::DeviceStateChanged { id, state }
            }
            Event::TransferAdded {
                id,
                device_id,
                direction,
                file_name,
                file_size,
            } => ServerEvent::TransferAdded {
                id,
                device_id,
                direction,
                file_name,
                total: file_size,
            },
            Event::TransferProgress {
                id,
                bytes_done,
                bytes_total,
            } => ServerEvent::TransferProgress {
                id,
                bytes_done,
                bytes_total,
            },
            Event::TransferFinished { id } => ServerEvent::TransferFinished { id },
            Event::TransferFailed { id, reason } => ServerEvent::TransferFailed { id, reason },
            Event::NotificationReceived {
                id,
                app,
                title,
                body,
            } => ServerEvent::NotificationReceived {
                id,
                app,
                title,
                body,
            },
            Event::ClipboardUpdated { text } => ServerEvent::ClipboardUpdated { text },
            Event::LogRecord { level, msg } => ServerEvent::LogRecord { level, msg },
            Event::DaemonShutdown => ServerEvent::DaemonShutdown,
        }
    }
}
