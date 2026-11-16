//! Typed events emitted by plugins.

use std::path::PathBuf;

use handfast_proto::{DeviceId, Packet};
use serde::{Deserialize, Serialize};

/// One audio sink reported by a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSink {
    /// Stable sink name used in requests.
    pub name: String,
    /// Display name.
    pub description: String,
    /// Current volume in the peer's units.
    pub volume: u32,
    /// Whether the sink is muted.
    pub muted: bool,
    /// Maximum volume in the peer's units.
    pub max_volume: u32,
    /// Whether the sink is the peer's default.
    pub enabled: bool,
}

impl RemoteSink {
    /// Current volume as a percentage of [`Self::max_volume`], rounded to the
    /// nearest point.
    #[must_use]
    pub fn percent(&self) -> u8 {
        if self.max_volume == 0 {
            return 0;
        }
        let scaled = u64::from(self.volume) * 100 + u64::from(self.max_volume) / 2;
        (scaled / u64::from(self.max_volume)).min(100) as u8
    }
}

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
    /// A peer reported its audio sinks.
    RemoteSinks {
        /// Sinks in name order.
        sinks: Vec<RemoteSink>,
    },
    /// Clipboard text arrived.
    Clipboard {
        /// Text content.
        text: String,
        /// Peer timestamp in milliseconds.
        timestamp: Option<i64>,
    },
    /// Share text arrived.
    ShareText {
        /// Text content.
        text: String,
    },
    /// Share URL arrived.
    ShareUrl {
        /// URL, restricted to `http`, `https` and `mailto` by the plugin.
        url: String,
    },
    /// Batch metadata arrived from a multi-file share.
    ShareBatch {
        /// Number of files in the batch.
        number_of_files: u32,
        /// Sum of the declared payload sizes.
        total_payload_size: u64,
    },
    /// A transfer moved bytes.
    ShareProgress {
        /// Transfer identifier assigned by the session.
        transfer_id: u64,
        /// Bytes moved so far.
        bytes: u64,
        /// Declared total, or `None` when the peer sent no size.
        total: Option<u64>,
    },
    /// A transfer finished and the file was saved.
    ShareCompleted {
        /// Transfer identifier assigned by the session.
        transfer_id: u64,
        /// Final path of the saved file.
        path: PathBuf,
        /// Bytes written.
        bytes: u64,
    },
    /// A transfer failed.
    ShareFailed {
        /// Transfer identifier, 0 when no transfer was created.
        transfer_id: u64,
        /// Human-readable failure reason.
        reason: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_percent_rounds_to_the_nearest_point() {
        let sink = RemoteSink {
            name: "speakers".to_string(),
            description: String::new(),
            volume: 26214,
            muted: false,
            max_volume: 65536,
            enabled: false,
        };
        assert_eq!(sink.percent(), 40);
        assert_eq!(
            RemoteSink {
                max_volume: 0,
                ..sink.clone()
            }
            .percent(),
            0
        );
        assert_eq!(
            RemoteSink {
                volume: 65536,
                ..sink
            }
            .percent(),
            100
        );
    }

    #[test]
    fn share_events_construct_and_compare() {
        let events = vec![
            PluginEvent::ShareText {
                text: "hello".to_string(),
            },
            PluginEvent::ShareUrl {
                url: "https://example.com".to_string(),
            },
            PluginEvent::ShareBatch {
                number_of_files: 2,
                total_payload_size: 12,
            },
            PluginEvent::ShareProgress {
                transfer_id: 7,
                bytes: 4,
                total: Some(12),
            },
            PluginEvent::ShareCompleted {
                transfer_id: 7,
                path: PathBuf::from("incoming/a.jpg"),
                bytes: 12,
            },
            PluginEvent::ShareFailed {
                transfer_id: 8,
                reason: "short payload".to_string(),
            },
        ];

        assert_eq!(events.clone(), events);
        assert_ne!(
            events[3],
            PluginEvent::ShareProgress {
                transfer_id: 7,
                bytes: 4,
                total: None,
            }
        );
    }
}
