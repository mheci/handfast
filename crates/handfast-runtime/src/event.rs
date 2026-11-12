//! Typed events emitted by plugins.

use handfast_proto::{DeviceId, Packet};
use serde::Serialize;

/// Battery state reported by a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BatteryState {
    /// Charge percentage, or `None` when the peer has no battery.
    pub charge: Option<u8>,
    /// Whether the peer is charging.
    pub charging: bool,
    /// Threshold event reported by the peer, 0 or 1.
    pub threshold: Option<u8>,
}

/// Severity for plugin log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LogLevel {
    /// Informational.
    Info,
    /// Recoverable problem.
    Warn,
    /// Dropped or rejected input.
    Error,
}

/// Events a plugin can emit to the daemon and UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum PluginEvent {
    /// A ping was received.
    Ping {
        /// Optional message.
        message: Option<String>,
    },
    /// Battery state changed.
    Battery(BatteryState),
    /// Clipboard text arrived.
    Clipboard {
        /// Text content.
        text: String,
        /// Peer timestamp in milliseconds.
        timestamp: Option<i64>,
    },
    /// Plugin-level log line.
    Log {
        /// Severity.
        level: LogLevel,
        /// Human-readable message.
        message: String,
    },
}

/// Runtime events for the daemon and IPC layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Event {
    /// A device link was established.
    DeviceConnected {
        /// Device identifier.
        device: DeviceId,
    },
    /// A device link was closed.
    DeviceDisconnected {
        /// Device identifier.
        device: DeviceId,
    },
    /// A packet was accepted from a peer.
    PacketReceived {
        /// Device identifier.
        device: DeviceId,
        /// Decoded packet.
        packet: Packet,
    },
    /// A plugin emitted a typed event.
    Plugin {
        /// Device identifier.
        device: DeviceId,
        /// Plugin name.
        plugin: &'static str,
        /// Plugin event.
        event: PluginEvent,
    },
}
