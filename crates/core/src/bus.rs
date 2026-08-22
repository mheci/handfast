//! In-process broadcast event bus.
//!
//! [`Bus`] wraps a `tokio::sync::broadcast` channel and is the spine of the
//! daemon: discovery, transfers, notifications, clipboard and log plumbing all
//! publish [`Event`]s while UI surfaces subscribe. Publishing never panics and
//! never blocks — events are dropped silently (debug-logged) when nobody
//! listens or when a slow subscriber falls behind.

use tokio::sync::broadcast;

/// Capacity of the underlying broadcast channel.
const BUS_CAPACITY: usize = 1024;

/// Events emitted by daemon subsystems.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A device announced itself on the network.
    DeviceFound {
        /// Stable device identifier (certificate fingerprint hash).
        id: String,
        /// Human-readable device name.
        name: String,
    },
    /// A previously visible device disappeared.
    DeviceLost {
        /// Device identifier.
        id: String,
    },
    /// Connectivity state changed (e.g. `"online"`, `"offline"`).
    DeviceStateChanged {
        /// Device identifier.
        id: String,
        /// New state label.
        state: String,
    },
    /// Progress update for an ongoing file transfer.
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
    /// Structured log record for UI surfaces (e.g. the TUI console pane).
    LogRecord {
        /// Severity label (`"info"`, `"warn"`, ...).
        level: String,
        /// Rendered message.
        msg: String,
    },
    /// The daemon is shutting down; subscribers should wind down.
    DaemonShutdown,
}

/// Clonable handle onto the process-wide event bus.
#[derive(Debug, Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    /// Create a bus with a bounded queue of [`BUS_CAPACITY`] events.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    /// Register a new subscriber receiving every event from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publish an event to all current subscribers.
    ///
    /// Never panics: with zero receivers (or any other send failure) the event
    /// is dropped and the fact logged at debug level.
    pub fn publish(&self, ev: Event) {
        if let Err(broadcast::error::SendError(_)) = self.tx.send(ev) {
            tracing::debug!(
                target: "handfast::bus",
                "event dropped: no active receivers"
            );
        }
    }

    /// Number of currently subscribed receivers.
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_subscribe_roundtrip() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);

        bus.publish(Event::DeviceLost { id: "d1".to_string() });
        match rx.try_recv() {
            Ok(Event::DeviceLost { id }) => assert_eq!(id, "d1"),
            other => panic!("expected DeviceLost, got {other:?}"),
        }
    }

    #[test]
    fn publish_without_receivers_does_not_panic() {
        let bus = Bus::new();
        assert_eq!(bus.receiver_count(), 0);
        bus.publish(Event::DaemonShutdown);
        bus.publish(Event::ClipboardUpdated { text: "ignored".to_string() });
        assert_eq!(bus.receiver_count(), 0);
    }

    #[test]
    fn dropped_receiver_does_not_break_publishing() {
        let bus = Bus::new();
        {
            let mut rx = bus.subscribe();
            bus.publish(Event::DeviceStateChanged {
                id: "d".to_string(),
                state: "online".to_string(),
            });
            assert!(rx.try_recv().is_ok());
        } // rx dropped here

        // Publishing keeps working after subscribers disappear.
        bus.publish(Event::DaemonShutdown);
        assert_eq!(bus.receiver_count(), 0);
    }

    #[test]
    fn multiple_subscribers_each_receive_a_copy() {
        let bus = Bus::new();
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);

        bus.publish(Event::ClipboardUpdated { text: "hi".to_string() });
        assert!(matches!(
            first.try_recv(),
            Ok(Event::ClipboardUpdated { .. })
        ));
        assert!(matches!(
            second.try_recv(),
            Ok(Event::ClipboardUpdated { .. })
        ));
    }
}
