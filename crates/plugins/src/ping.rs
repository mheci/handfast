//! Fully implemented ping plugin (Phase-1 parity).
//!
//! Replies to every inbound `kdeconnect.ping` with an outbound
//! `kdeconnect.ping`, and records the wall-clock time of the most recent
//! inbound ping for introspection surfaces (`hfctl devices --json` et al).

use std::time::SystemTime;

use handfast_protocol::{Packet, TYPE_PING};

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Echoes `kdeconnect.ping` packets back to the sender.
#[derive(Debug)]
pub struct PingPlugin {
    last_ping_at: Option<SystemTime>,
}

impl PingPlugin {
    /// Create a plugin instance with no pings observed yet.
    #[must_use]
    pub fn new() -> Self {
        Self { last_ping_at: None }
    }

    /// Wall-clock time of the most recent inbound ping, if any.
    #[must_use]
    pub fn last_ping_at(&self) -> Option<SystemTime> {
        self.last_ping_at
    }
}

impl Default for PingPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for PingPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::PING
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        if pkt.ty() != TYPE_PING {
            tracing::debug!(
                plugin = meta::PING.name,
                got = pkt.ty(),
                "ignoring foreign packet"
            );
            return Vec::new();
        }
        self.last_ping_at = Some(SystemTime::now());
        tracing::debug!(plugin = meta::PING.name, "replying to ping");
        vec![Packet::new(TYPE_PING, serde_json::json!({}))]
    }
}

/// Builds [`PingPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct PingFactory;

impl PluginFactory for PingFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::PING
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(PingPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replies_exactly_once_and_tracks_time() {
        let mut plugin = PingPlugin::new();
        assert!(plugin.last_ping_at().is_none());
        let pkt = Packet::new(TYPE_PING, json!({}));
        let replies = plugin.handle(&pkt);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ty(), TYPE_PING);
        assert!(plugin.last_ping_at().is_some());
    }

    #[test]
    fn ignores_foreign_packet_types_without_state_change() {
        let mut plugin = PingPlugin::new();
        let pkt = Packet::new("kdeconnect.battery", json!({"level": 50}));
        assert!(plugin.handle(&pkt).is_empty());
        let pkt = Packet::new("kdeconnect.mousepad.request", json!({"dx": 1}));
        assert!(plugin.handle(&pkt).is_empty());
        assert!(plugin.last_ping_at().is_none());
    }

    #[test]
    fn factory_builds_fresh_instances() {
        let a = PingFactory.create();
        let b = PingFactory.create();
        assert_eq!(a.meta().name, "ping");
        assert_eq!(b.meta().name, "ping");
    }
}
