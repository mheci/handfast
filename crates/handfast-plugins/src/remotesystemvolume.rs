//! Remote system volume plugin.
//!
//! The desktop keeps the audio sinks a peer reports and sends volume or mute
//! requests back. The peer owns its mixer; this plugin never touches a local
//! audio device.

use std::collections::BTreeMap;

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput, RemoteSink,
};
use serde_json::{json, Value};

use crate::roster;

const MAX_NAME_BYTES: usize = 256;
const MAX_DESCRIPTION_BYTES: usize = 1024;
const MAX_SINKS: usize = 64;
const MAX_VOLUME: u32 = 1_000_000;

/// Whether `name` is a sink name a peer may use in a request.
#[must_use]
pub fn valid_sink_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
}

fn valid_description(text: &str) -> bool {
    text.len() <= MAX_DESCRIPTION_BYTES && !text.chars().any(char::is_control)
}

fn number(value: Option<&Value>) -> Option<u32> {
    u32::try_from(value?.as_u64()?).ok()
}

fn parse_sink(value: &Value) -> Option<RemoteSink> {
    let object = value.as_object()?;
    let name = object.get("name")?.as_str()?;
    if !valid_sink_name(name) {
        return None;
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_description(description) {
        return None;
    }
    let volume = number(object.get("volume"))?;
    let max_volume = match object.get("maxVolume") {
        None => 100,
        Some(value) => number(Some(value))?,
    };
    if max_volume == 0 || max_volume > MAX_VOLUME || volume > max_volume {
        return None;
    }
    let muted = object
        .get("muted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(RemoteSink {
        name: name.to_string(),
        description: description.to_string(),
        volume,
        muted,
        max_volume,
        enabled,
    })
}

fn log(level: LogLevel, message: impl Into<String>) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level,
        message: message.into(),
    })
}

/// Tracks the audio sinks a peer reports.
#[derive(Debug, Default)]
pub struct RemoteSystemVolumePlugin {
    sinks: BTreeMap<String, RemoteSink>,
}

impl RemoteSystemVolumePlugin {
    fn model(&self) -> PluginOutput {
        PluginOutput::new().with_event(PluginEvent::RemoteSinks {
            sinks: self.sinks.values().cloned().collect(),
        })
    }

    fn sink_list(&mut self, packet: &Packet) -> PluginOutput {
        let Some(list) = packet.body.get("sinkList") else {
            return self.delta(packet);
        };
        let Some(list) = list.as_array() else {
            return log(LogLevel::Warn, "remote sink list is not an array");
        };
        self.sinks.clear();
        let mut skipped = 0usize;
        for value in list {
            if self.sinks.len() >= MAX_SINKS {
                skipped += 1;
                continue;
            }
            match parse_sink(value) {
                Some(sink) => {
                    self.sinks.insert(sink.name.clone(), sink);
                }
                None => skipped += 1,
            }
        }
        let output = self.model();
        if skipped == 0 {
            output
        } else {
            output.with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: format!("skipped {skipped} invalid remote sink entries"),
            })
        }
    }

    fn delta(&mut self, packet: &Packet) -> PluginOutput {
        let Some(name) = packet.body.get("name").and_then(Value::as_str) else {
            return log(LogLevel::Warn, "remote sink packet without a name");
        };
        if !valid_sink_name(name) {
            return log(LogLevel::Warn, "remote sink packet with an invalid name");
        }
        let Some(sink) = self.sinks.get_mut(name) else {
            return log(
                LogLevel::Warn,
                format!("remote sink packet for an unknown sink: {name}"),
            );
        };
        let volume = match packet.body.get("volume") {
            None => None,
            Some(value) => match number(Some(value)) {
                Some(volume) if volume <= sink.max_volume => Some(volume),
                _ => {
                    return log(
                        LogLevel::Warn,
                        format!("remote sink volume for {name} is out of range"),
                    )
                }
            },
        };
        let muted = match packet.body.get("muted") {
            None => None,
            Some(value) => match value.as_bool() {
                Some(muted) => Some(muted),
                None => {
                    return log(
                        LogLevel::Warn,
                        format!("remote sink mute for {name} is not a boolean"),
                    )
                }
            },
        };
        let enabled = match packet.body.get("enabled") {
            None => None,
            Some(value) => match value.as_bool() {
                Some(enabled) => Some(enabled),
                None => {
                    return log(
                        LogLevel::Warn,
                        format!("remote sink default flag for {name} is not a boolean"),
                    )
                }
            },
        };
        if volume.is_none() && muted.is_none() && enabled.is_none() {
            return log(
                LogLevel::Warn,
                format!("remote sink packet for {name} changes nothing"),
            );
        }
        if let Some(volume) = volume {
            sink.volume = volume;
        }
        if let Some(muted) = muted {
            sink.muted = muted;
        }
        if let Some(enabled) = enabled {
            sink.enabled = enabled;
        }
        self.model()
    }
}

#[async_trait]
impl Plugin for RemoteSystemVolumePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTESYSTEMVOLUME
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::SYSTEMVOLUME {
            return Ok(PluginOutput::new());
        }
        Ok(self.sink_list(packet))
    }
}

/// Creates one sink model per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteSystemVolumeFactory;

impl PluginFactory for RemoteSystemVolumeFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTESYSTEMVOLUME
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteSystemVolumePlugin::default())
    }
}

/// Build a request that sets one peer sink's volume in the peer's units.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn volume_request(name: &str, volume: u32) -> Result<Packet, ProtoError> {
    Packet::new(
        types::SYSTEMVOLUME_REQUEST,
        json!({ "name": name, "volume": volume }),
    )
}

/// Build a request that sets one peer sink's mute state.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn mute_request(name: &str, muted: bool) -> Result<Packet, ProtoError> {
    Packet::new(
        types::SYSTEMVOLUME_REQUEST,
        json!({ "name": name, "muted": muted }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Desktop,
        )
    }

    async fn run(plugin: &mut RemoteSystemVolumePlugin, packet: Packet) -> PluginOutput {
        let device = device();
        let desktop = handfast_desktop::Desktop::default();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    fn packet(body: Value) -> Packet {
        Packet::from_body(types::SYSTEMVOLUME, &body).unwrap()
    }

    fn sinks(output: &PluginOutput) -> &[RemoteSink] {
        output
            .events
            .iter()
            .find_map(|event| match event {
                PluginEvent::RemoteSinks { sinks } => Some(sinks.as_slice()),
                _ => None,
            })
            .expect("a sink event")
    }

    #[tokio::test]
    async fn a_sink_list_replaces_the_model() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        let output = run(
            &mut plugin,
            packet(json!({
                "sinkList": [
                    {
                        "name": "speakers",
                        "description": "Built-in speakers",
                        "volume": 32768,
                        "muted": false,
                        "maxVolume": 65536,
                        "enabled": true,
                    },
                    {
                        "name": "headphones",
                        "description": "Headphones",
                        "volume": 40,
                        "muted": true,
                        "maxVolume": 100,
                    },
                ]
            })),
        )
        .await;

        let sinks = sinks(&output);
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].name, "headphones");
        assert_eq!(sinks[0].percent(), 40);
        assert!(sinks[0].muted);
        assert!(!sinks[0].enabled);
        assert_eq!(sinks[1].name, "speakers");
        assert_eq!(sinks[1].percent(), 50);
        assert!(sinks[1].enabled);
    }

    #[tokio::test]
    async fn invalid_sinks_are_skipped_with_a_warning() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        let output = run(
            &mut plugin,
            packet(json!({
                "sinkList": [
                    { "name": "", "volume": 1, "maxVolume": 10 },
                    { "name": "over", "volume": 11, "maxVolume": 10 },
                    { "name": "bad\u{0}name", "volume": 1, "maxVolume": 10 },
                    { "name": "zero", "volume": 0, "maxVolume": 0 },
                    { "name": "good", "volume": 5, "maxVolume": 10 },
                ]
            })),
        )
        .await;

        assert_eq!(sinks(&output).len(), 1);
        assert_eq!(sinks(&output)[0].name, "good");
        assert!(matches!(
            output.events.last(),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_delta_updates_volume_mute_and_default() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        run(
            &mut plugin,
            packet(json!({
                "sinkList": [
                    { "name": "speakers", "volume": 10, "muted": false, "maxVolume": 100 }
                ]
            })),
        )
        .await;

        let output = run(
            &mut plugin,
            packet(json!({
                "name": "speakers",
                "volume": 70,
                "muted": true,
                "enabled": true,
            })),
        )
        .await;

        let sinks = sinks(&output);
        assert_eq!(sinks[0].volume, 70);
        assert!(sinks[0].muted);
        assert!(sinks[0].enabled);
    }

    #[tokio::test]
    async fn a_sink_list_is_capped() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        let entries: Vec<Value> = (0..MAX_SINKS + 1)
            .map(|index| {
                json!({
                    "name": format!("sink-{index}"),
                    "volume": 1,
                    "maxVolume": 10,
                })
            })
            .collect();
        let output = run(&mut plugin, packet(json!({ "sinkList": entries }))).await;

        assert_eq!(sinks(&output).len(), MAX_SINKS);
        assert!(matches!(
            output.events.last(),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_unknown_sink_delta_is_a_warning() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        let output = run(&mut plugin, packet(json!({"name": "ghost", "volume": 5}))).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_delta_without_changes_is_a_warning() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        run(
            &mut plugin,
            packet(json!({
                "sinkList": [{ "name": "speakers", "volume": 10, "maxVolume": 100 }]
            })),
        )
        .await;
        let output = run(&mut plugin, packet(json!({"name": "speakers"}))).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn an_out_of_range_volume_is_a_warning() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        run(
            &mut plugin,
            packet(json!({
                "sinkList": [{ "name": "speakers", "volume": 10, "maxVolume": 100 }]
            })),
        )
        .await;
        let output = run(
            &mut plugin,
            packet(json!({"name": "speakers", "volume": 101})),
        )
        .await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_non_array_sink_list_is_a_warning() {
        let mut plugin = RemoteSystemVolumePlugin::default();
        let output = run(&mut plugin, packet(json!({"sinkList": "nope"}))).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[test]
    fn requests_carry_the_name_and_value() {
        let volume = volume_request("speakers", 32768).unwrap();
        assert_eq!(volume.ptype, types::SYSTEMVOLUME_REQUEST);
        assert_eq!(volume.body["name"], "speakers");
        assert_eq!(volume.body["volume"], 32768);

        let muted = mute_request("speakers", true).unwrap();
        assert_eq!(muted.ptype, types::SYSTEMVOLUME_REQUEST);
        assert_eq!(muted.body["name"], "speakers");
        assert_eq!(muted.body["muted"], true);
    }

    #[test]
    fn sink_names_are_validated() {
        assert!(valid_sink_name(
            "alsa_output.pci-0000_00_1b.0.analog-stereo"
        ));
        assert!(!valid_sink_name(""));
        assert!(!valid_sink_name("bad\nname"));
        assert!(!valid_sink_name(&"a".repeat(MAX_NAME_BYTES + 1)));
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(
            &roster::REMOTESYSTEMVOLUME,
            RemoteSystemVolumePlugin::default().meta()
        );
        assert_eq!(
            &roster::REMOTESYSTEMVOLUME,
            RemoteSystemVolumeFactory.meta()
        );
        assert_eq!(
            roster::REMOTESYSTEMVOLUME.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
