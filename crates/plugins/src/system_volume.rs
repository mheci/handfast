//! System-wide output volume tracking (`system_volume`).
//!
//! Observes inbound [`TYPE_SYSTEMVOLUME`] reports shaped like
//! `{"direction":"in","sink_description":"...","volume":0-100,"mute":bool}`
//! and records the most recent state for UI surfaces (`hfctl volume` et al).
//! Outbound reports are built with [`volume_report`]; actual audio-backend IO
//! (PipeWire/Pulse) arrives in Phase 3, so [`Plugin::handle`] stays purely
//! observational and never replies.
//!
//! Both the upstream `muted` spelling and Handfast's `mute` spelling of the
//! boolean flag are accepted on inbound reports.

use handfast_protocol::{Packet, TYPE_SYSTEMVOLUME};
use serde_json::Value;

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Most recently observed sink volume state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeState {
    /// Human-readable sink label, when the peer supplied one.
    pub sink_description: Option<String>,
    /// Volume percentage, always within `0..=100`.
    pub volume: u8,
    /// Whether the sink is muted.
    pub muted: bool,
}

/// Tracks remote-reported system volume; never panics on malformed input.
#[derive(Debug)]
pub struct SystemVolumePlugin {
    state: Option<VolumeState>,
}

impl SystemVolumePlugin {
    /// Create a plugin instance with no volume observed yet.
    #[must_use]
    pub fn new() -> Self {
        Self { state: None }
    }

    /// Latest observed volume state, if any report has arrived.
    #[must_use]
    pub fn state(&self) -> Option<&VolumeState> {
        self.state.as_ref()
    }

    fn observe(&mut self, body: &Value) {
        let Some(state) = extract_state(body) else {
            tracing::debug!(
                plugin = meta::SYSTEM_VOLUME.name,
                "discarding malformed volume report"
            );
            return;
        };
        tracing::debug!(
            plugin = meta::SYSTEM_VOLUME.name,
            volume = state.volume,
            muted = state.muted,
            "volume report recorded"
        );
        self.state = Some(state);
    }
}

impl Default for SystemVolumePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SystemVolumePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::SYSTEM_VOLUME
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        if pkt.ty() != TYPE_SYSTEMVOLUME {
            tracing::debug!(
                plugin = meta::SYSTEM_VOLUME.name,
                got = pkt.ty(),
                "ignoring foreign packet"
            );
            return Vec::new();
        }
        self.observe(&pkt.body);
        Vec::new()
    }
}

/// Builds an outbound volume report packet, clamping `percent` to `0..=100`.
#[must_use]
pub fn volume_report(percent: u8, muted: bool) -> Packet {
    Packet::new(
        TYPE_SYSTEMVOLUME,
        serde_json::json!({
            "direction": "in",
            "volume": percent.min(100),
            "mute": muted,
        }),
    )
}

/// Extracts a coherent [`VolumeState`] from a report body, rejecting bodies
/// without a numeric `volume` field and clamping outliers into `0..=100`.
fn extract_state(body: &Value) -> Option<VolumeState> {
    let obj = body.as_object()?;
    let raw_volume = obj.get("volume").and_then(Value::as_f64)?;
    let muted = obj
        .get("muted")
        .or_else(|| obj.get("mute"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sink_description = obj
        .get("sink_description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(VolumeState {
        sink_description,
        volume: raw_volume.clamp(0.0, 100.0).round() as u8,
        muted,
    })
}

/// Builds [`SystemVolumePlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemVolumeFactory;

impl PluginFactory for SystemVolumeFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::SYSTEM_VOLUME
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SystemVolumePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sink_body() -> Value {
        json!({
            "direction": "in",
            "sink_description": "Built-in Speakers",
            "volume": 42,
            "mute": false,
        })
    }

    #[test]
    fn tracks_most_recent_state() {
        let mut plugin = SystemVolumePlugin::new();
        assert!(plugin.state().is_none());

        plugin.handle(&Packet::new(TYPE_SYSTEMVOLUME, sink_body()));
        let state = plugin.state();
        assert_eq!(
            state,
            Some(&VolumeState {
                sink_description: Some("Built-in Speakers".to_string()),
                volume: 42,
                muted: false,
            })
        );

        plugin.handle(&Packet::new(
            TYPE_SYSTEMVOLUME,
            json!({"direction": "in", "volume": 90, "mute": true}),
        ));
        let state = plugin.state();
        assert_eq!(
            state,
            Some(&VolumeState {
                sink_description: None,
                volume: 90,
                muted: true,
            })
        );
    }

    #[test]
    fn accepts_both_mute_spellings() {
        let mut plugin = SystemVolumePlugin::new();
        plugin.handle(&Packet::new(
            TYPE_SYSTEMVOLUME,
            json!({"volume": 1, "muted": true}),
        ));
        assert_eq!(plugin.state().map(|state| state.muted), Some(true));
        plugin.handle(&Packet::new(
            TYPE_SYSTEMVOLUME,
            json!({"volume": 2, "mute": true}),
        ));
        assert_eq!(plugin.state().map(|state| state.muted), Some(true));
        plugin.handle(&Packet::new(TYPE_SYSTEMVOLUME, json!({"volume": 3})));
        assert_eq!(plugin.state().map(|state| state.muted), Some(false));
    }

    #[test]
    fn clamps_out_of_range_volumes() {
        let mut plugin = SystemVolumePlugin::new();
        plugin.handle(&Packet::new(TYPE_SYSTEMVOLUME, json!({"volume": 250})));
        assert_eq!(plugin.state().map(|state| state.volume), Some(100));
        plugin.handle(&Packet::new(TYPE_SYSTEMVOLUME, json!({"volume": -7})));
        assert_eq!(plugin.state().map(|state| state.volume), Some(0));
        plugin.handle(&Packet::new(TYPE_SYSTEMVOLUME, json!({"volume": 33.6})));
        assert_eq!(plugin.state().map(|state| state.volume), Some(34));
    }

    #[test]
    fn ignores_malformed_bodies_without_state_change() {
        let mut plugin = SystemVolumePlugin::new();
        let bad_bodies = [
            Value::Null,
            json!("loud"),
            json!([]),
            json!({}),
            json!({"volume": "42"}),
            json!({"mute": true}),
            json!({"direction": "in"}),
        ];
        for body in bad_bodies {
            plugin.handle(&Packet::new(TYPE_SYSTEMVOLUME, body.clone()));
            assert!(plugin.state().is_none(), "body {body} mutated state");
        }
    }

    #[test]
    fn ignores_foreign_packet_types_without_state_change() {
        let mut plugin = SystemVolumePlugin::new();
        assert!(plugin
            .handle(&Packet::new("kdeconnect.ping", json!({})))
            .is_empty());
        assert!(plugin
            .handle(&Packet::new("kdeconnect.battery", json!({"volume": 5})))
            .is_empty());
        assert!(plugin
            .handle(&Packet::new(
                "kdeconnect.systemvolume.request",
                json!({"volume": 5})
            ))
            .is_empty());
        assert!(plugin.state().is_none());
    }

    #[test]
    fn volume_report_builds_clamped_packet() {
        let pkt = volume_report(77, true);
        assert_eq!(pkt.ty(), TYPE_SYSTEMVOLUME);
        assert_eq!(
            pkt.body,
            json!({"direction": "in", "volume": 77, "mute": true})
        );

        let pkt = volume_report(255, false);
        assert_eq!(pkt.body["volume"], json!(100));

        let pkt = volume_report(0, false);
        assert_eq!(pkt.body["volume"], json!(0));
        assert_eq!(pkt.body["mute"], json!(false));
    }

    #[test]
    fn handle_is_always_silent() {
        let mut plugin = SystemVolumePlugin::new();
        assert!(plugin
            .handle(&Packet::new(TYPE_SYSTEMVOLUME, sink_body()))
            .is_empty());
    }

    #[test]
    fn factory_builds_fresh_instances() {
        let a = SystemVolumeFactory.create();
        let b = SystemVolumeFactory.create();
        assert_eq!(a.meta().name, "system_volume");
        assert_eq!(b.meta().name, "system_volume");
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(SystemVolumePlugin::default().state(), None);
    }
}
