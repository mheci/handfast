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
            Event::TransferProgress {
                id,
                bytes_done,
                bytes_total,
            } => ServerEvent::TransferProgress {
                id,
                bytes_done,
                bytes_total,
            },
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
