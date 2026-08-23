//! Telephony plugin: call and SMS events surfacing from the paired phone.
//!
//! Upstream `kdeconnect.telephony` bodies carry
//! `{"event": ..., "phoneNumber": ...}` where `event` is one of `ringing`,
//! `talking`, `missedCall` or `sms` (legacy builds also send `stopRinging`,
//! which is tracked like any other event but never triggers a reply). The
//! plugin records the most recent event per instance; UI surfaces read it via
//! [`TelephonyPlugin::last_event`].
//!
//! When configured with [`TelephonyPlugin::with_pause_music`], an incoming
//! ring produces a mute-music *suggestion*: a `kdeconnect.mpris.request`
//! carrying the `PlayPause` action with an empty player id (i.e. "any
//! player"). Actually driving the desktop MPRIS session stays with the
//! dedicated `pause_music` plugin in Phase 3; this side only emits the wire
//! packet, so no D-Bus is required here.

use handfast_protocol::{Packet, TYPE_MPRIS_REQUEST, TYPE_TELEPHONY};
use serde_json::Value;

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Phone started ringing.
pub const EVENT_RINGING: &str = "ringing";

/// Call answered and in progress.
pub const EVENT_TALKING: &str = "talking";

/// Call rang out without being answered.
pub const EVENT_MISSED_CALL: &str = "missedCall";

/// Inbound text message.
pub const EVENT_SMS: &str = "sms";

/// Tracks call/SMS events from the phone and suggests muting music on rings.
#[derive(Debug)]
pub struct TelephonyPlugin {
    last_call_state: Option<String>,
    last_phone_number: Option<String>,
    pause_music_on_ring: bool,
}

impl TelephonyPlugin {
    /// Create a plugin instance with no event observed yet and the
    /// pause-on-ring suggestion disabled (matching `meta::PAUSE_MUSIC`'s
    /// `default_enabled: false`; configuration wiring arrives in Phase 3).
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_call_state: None,
            last_phone_number: None,
            pause_music_on_ring: false,
        }
    }

    /// Enable emitting the mute-music suggestion (`kdeconnect.mpris.request`
    /// with `PlayPause`) whenever the phone reports `ringing`.
    #[must_use]
    pub fn with_pause_music(mut self, enabled: bool) -> Self {
        self.pause_music_on_ring = enabled;
        self
    }

    /// Consumes the instance and returns the latest observed telephony event
    /// as `(state, number)` — for example `("ringing", "+15550100")`. The
    /// number is the empty string when the phone omitted it. `None` until any
    /// event carrying an `event` field has been received.
    #[must_use]
    pub fn last_event(self) -> Option<(String, String)> {
        let Self {
            last_call_state,
            last_phone_number,
            ..
        } = self;
        last_call_state.zip(last_phone_number)
    }

    /// Records the event carried by `pkt`, returning the mute-music
    /// suggestion when applicable.
    fn record(&mut self, pkt: &Packet) -> Vec<Packet> {
        let Some(event) = pkt.body.get("event").and_then(Value::as_str) else {
            tracing::debug!(
                plugin = meta::TELEPHONY.name,
                "telephony body without string event; ignoring"
            );
            return Vec::new();
        };
        let number = pkt
            .body
            .get("phoneNumber")
            .and_then(Value::as_str)
            .unwrap_or("");
        self.last_call_state = Some(event.to_owned());
        self.last_phone_number = Some(number.to_owned());
        tracing::debug!(
            plugin = meta::TELEPHONY.name,
            event,
            number,
            "telephony event"
        );
        if event == EVENT_RINGING && self.pause_music_on_ring {
            tracing::debug!(
                plugin = meta::TELEPHONY.name,
                "ringing; suggesting PlayPause to media players"
            );
            return vec![Packet::new(
                TYPE_MPRIS_REQUEST,
                serde_json::json!({ "player": "", "action": "PlayPause" }),
            )];
        }
        Vec::new()
    }
}

impl Default for TelephonyPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for TelephonyPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::TELEPHONY
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        if pkt.ty() != TYPE_TELEPHONY {
            tracing::debug!(
                plugin = meta::TELEPHONY.name,
                got = pkt.ty(),
                "ignoring foreign packet"
            );
            return Vec::new();
        }
        self.record(pkt)
    }
}

/// Builds [`TelephonyPlugin`] instances (pause-on-ring disabled by default).
#[derive(Debug, Clone, Copy, Default)]
pub struct TelephonyFactory;

impl PluginFactory for TelephonyFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::TELEPHONY
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(TelephonyPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_protocol::TYPE_PING;
    use serde_json::json;

    fn ringing() -> Packet {
        Packet::new(
            TYPE_TELEPHONY,
            json!({"event": "ringing", "phoneNumber": "+15550100", "contactName": "Ada"}),
        )
    }

    #[test]
    fn ringing_emits_playpause_suggestion_when_configured() {
        let mut plugin = TelephonyPlugin::new().with_pause_music(true);
        let replies = plugin.handle(&ringing());
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ty(), TYPE_MPRIS_REQUEST);
        assert_eq!(replies[0].body["action"], "PlayPause");
        assert_eq!(replies[0].body["player"], "");
    }

    #[test]
    fn ringing_is_silent_by_default_but_still_tracked() {
        let mut plugin = TelephonyPlugin::new();
        assert!(plugin.handle(&ringing()).is_empty());
        let (state, number) = plugin.last_event().expect("ringing must be tracked");
        assert_eq!(state, EVENT_RINGING);
        assert_eq!(number, "+15550100");
    }

    #[test]
    fn sms_talking_and_missedcall_are_tracked_without_replies() {
        let bodies = [
            json!({"event": "sms", "phoneNumber": "+15550101", "messageBody": "hi"}),
            json!({"event": "talking", "phoneNumber": "+15550100"}),
            json!({"event": "missedCall", "phoneNumber": "+15550102"}),
        ];
        let mut plugin = TelephonyPlugin::new().with_pause_music(true);
        for body in &bodies {
            assert!(plugin
                .handle(&Packet::new(TYPE_TELEPHONY, body.clone()))
                .is_empty());
        }
        let (state, number) = plugin.last_event().expect("events must be tracked");
        assert_eq!(state, EVENT_MISSED_CALL);
        assert_eq!(number, "+15550102");
    }

    #[test]
    fn missing_phone_number_degrades_to_empty_string() {
        let mut plugin = TelephonyPlugin::new();
        plugin.handle(&Packet::new(TYPE_TELEPHONY, json!({"event": "sms"})));
        let (_, number) = plugin.last_event().expect("event must be tracked");
        assert_eq!(number, "");
    }

    #[test]
    fn unknown_events_are_tracked_without_replies() {
        let mut plugin = TelephonyPlugin::new().with_pause_music(true);
        let stop = Packet::new(TYPE_TELEPHONY, json!({"event": "stopRinging"}));
        assert!(plugin.handle(&stop).is_empty());
        assert_eq!(
            plugin.last_event().map(|(state, _)| state),
            Some("stopRinging".to_owned())
        );
    }

    #[test]
    fn malformed_bodies_never_panic_or_change_state() {
        let bodies = vec![
            Value::Null,
            json!({}),
            json!({"event": null}),
            json!({"event": 42}),
            json!({"event": ["ringing"]}),
            json!({"event": {"k": "ringing"}}),
            json!({"phoneNumber": "+15550100"}),
            json!("not an object"),
            json!([]),
        ];
        let mut plugin = TelephonyPlugin::new().with_pause_music(true);
        for body in bodies {
            assert!(
                plugin.handle(&Packet::new(TYPE_TELEPHONY, body)).is_empty(),
                "garbage must not produce replies"
            );
        }
        assert!(plugin.last_event().is_none(), "garbage must not be tracked");
    }

    #[test]
    fn foreign_packet_types_are_ignored() {
        let mut plugin = TelephonyPlugin::new();
        assert!(plugin.handle(&Packet::new(TYPE_PING, json!({}))).is_empty());
        assert!(plugin
            .handle(&Packet::new("kdeconnect.battery", json!({"level": 5})))
            .is_empty());
        assert!(plugin.last_event().is_none());
    }

    #[test]
    fn factory_builds_fresh_instances_with_default_config() {
        assert_eq!(TelephonyFactory.meta().name, "telephony");
        let mut first = TelephonyFactory.create();
        let mut second = TelephonyFactory.create();
        // Both fresh instances start untracked and silent on ring.
        for plugin in [&mut first, &mut second] {
            assert!(plugin.handle(&ringing()).is_empty());
        }
    }
}
