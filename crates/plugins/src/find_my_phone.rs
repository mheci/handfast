//! Find-my-phone responder (`findmyphone`).
//!
//! Receives [`TYPE_FINDMYPHONE`] requests whose body carries
//! `{"action":"start"|"stop"}`. A `start` flips the plugin into ringing
//! state (audible alarm IO lands in Phase 3) and echoes the request back to
//! the sender as acknowledgement; a `stop` silences it. Unrecognized actions
//! and malformed bodies are logged and dropped without state changes.

use handfast_protocol::{Packet, TYPE_FINDMYPHONE};
use serde_json::Value;

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Acknowledges ring requests from the paired device.
#[derive(Debug)]
pub struct FindMyPhonePlugin {
    ringing: bool,
}

impl FindMyPhonePlugin {
    /// Create a plugin instance that is not ringing.
    #[must_use]
    pub const fn new() -> Self {
        Self { ringing: false }
    }

    /// Whether a `start` request is currently active.
    #[must_use]
    pub const fn is_ringing(&self) -> bool {
        self.ringing
    }

    /// Returns the acknowledgement echo for a recognized action, updating
    /// ringing state; `None` for anything unrecognized.
    fn apply_action(&mut self, body: &Value) -> Option<Packet> {
        let action = body.get("action").and_then(Value::as_str)?;
        match action {
            "start" => {
                self.ringing = true;
                tracing::info!(plugin = meta::FINDMYPHONE.name, "ring started");
                Some(Packet::new(TYPE_FINDMYPHONE, body.clone()))
            }
            "stop" => {
                self.ringing = false;
                tracing::info!(plugin = meta::FINDMYPHONE.name, "ring stopped");
                Some(Packet::new(TYPE_FINDMYPHONE, body.clone()))
            }
            other => {
                tracing::debug!(
                    plugin = meta::FINDMYPHONE.name,
                    action = other,
                    "ignoring unknown findmyphone action"
                );
                None
            }
        }
    }
}

impl Default for FindMyPhonePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for FindMyPhonePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::FINDMYPHONE
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        if pkt.ty() != TYPE_FINDMYPHONE {
            tracing::debug!(
                plugin = meta::FINDMYPHONE.name,
                got = pkt.ty(),
                "ignoring foreign packet"
            );
            return Vec::new();
        }
        match self.apply_action(&pkt.body) {
            Some(echo) => vec![echo],
            None => Vec::new(),
        }
    }
}

/// Builds [`FindMyPhonePlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct FindMyPhoneFactory;

impl PluginFactory for FindMyPhoneFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::FINDMYPHONE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(FindMyPhonePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn start_rings_and_echoes_request() {
        let mut plugin = FindMyPhonePlugin::new();
        assert!(!plugin.is_ringing());
        let body = json!({"action": "start"});
        let replies = plugin.handle(&Packet::new(TYPE_FINDMYPHONE, body));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ty(), TYPE_FINDMYPHONE);
        assert_eq!(replies[0].body, json!({"action": "start"}));
        assert!(plugin.is_ringing());
    }

    #[test]
    fn stop_silences_and_echoes_request() {
        let mut plugin = FindMyPhonePlugin::new();
        plugin.handle(&Packet::new(TYPE_FINDMYPHONE, json!({"action": "start"})));
        assert!(plugin.is_ringing());
        let replies = plugin.handle(&Packet::new(TYPE_FINDMYPHONE, json!({"action": "stop"})));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ty(), TYPE_FINDMYPHONE);
        assert_eq!(replies[0].body, json!({"action": "stop"}));
        assert!(!plugin.is_ringing());
    }

    #[test]
    fn repeated_start_is_idempotent() {
        let mut plugin = FindMyPhonePlugin::new();
        plugin.handle(&Packet::new(TYPE_FINDMYPHONE, json!({"action": "start"})));
        let replies = plugin.handle(&Packet::new(TYPE_FINDMYPHONE, json!({"action": "start"})));
        assert_eq!(replies.len(), 1);
        assert!(plugin.is_ringing());
    }

    #[test]
    fn unknown_or_missing_actions_are_dropped() {
        let mut plugin = FindMyPhonePlugin::new();
        for body in [
            json!({"action": "dance"}),
            json!({"action": ""}),
            json!({}),
            json!({"Action": "start"}),
            json!("start"),
            Value::Null,
            json!([]),
        ] {
            let replies = plugin.handle(&Packet::new(TYPE_FINDMYPHONE, body.clone()));
            assert!(replies.is_empty(), "body {body} produced replies");
            assert!(!plugin.is_ringing(), "body {body} flipped state");
        }
    }

    #[test]
    fn ignores_foreign_packet_types_without_state_change() {
        let mut plugin = FindMyPhonePlugin::new();
        assert!(plugin
            .handle(&Packet::new("kdeconnect.ping", json!({"action": "start"})))
            .is_empty());
        assert!(plugin
            .handle(&Packet::new(
                "kdeconnect.findmyphone.response",
                json!({"action": "stop"})
            ))
            .is_empty());
        assert!(!plugin.is_ringing());
    }

    #[test]
    fn fresh_instance_starts_silent() {
        assert!(!FindMyPhonePlugin::default().is_ringing());
    }

    #[test]
    fn factory_builds_fresh_instances() {
        let a = FindMyPhoneFactory.create();
        let b = FindMyPhoneFactory.create();
        assert_eq!(a.meta().name, "findmyphone");
        assert_eq!(b.meta().name, "findmyphone");
    }
}
