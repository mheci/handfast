//! System volume plugin.
//!
//! The desktop side of this plugin is a sink provider, not a controller. On
//! connect it pushes `kdeconnect.systemvolume {"sinkList": [...]}`, and it
//! answers a peer's `kdeconnect.systemvolume.request {"requestSinks": true}`
//! with the same list. A control packet addresses one sink by name and carries
//! `volume` in that sink's own units, `muted`, or `enabled` to make it the
//! default output.
//!
//! `sinkList` entries use the desktop client's field names: `name`,
//! `description`, `volume`, `maxVolume`, `muted` and `enabled`.

use async_trait::async_trait;
use handfast_desktop::AudioSink;
use handfast_proto::{types, Packet};
use handfast_runtime::{LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput};
use serde_json::{json, Value};

use crate::roster;

/// Largest sink list sent in one packet.
const MAX_SINKS: usize = 64;
/// Largest accepted sink name, matching the desktop client's identifier length.
const MAX_NAME_BYTES: usize = 256;

/// Applies inbound volume controls to the desktop output sinks.
#[derive(Debug, Default)]
pub struct SystemVolumePlugin;

impl SystemVolumePlugin {
    /// Create a system volume plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn warn(message: &str) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.to_string(),
    })
}

/// One control body, after validation.
#[derive(Debug, PartialEq, Eq)]
struct Control {
    name: String,
    volume: Option<i64>,
    muted: Option<bool>,
    enabled: Option<bool>,
}

/// Parse a control packet. `None` means "not a control packet".
///
/// A packet with no recognised field is a warning, not a control.
fn parse(packet: &Packet) -> Result<Option<Control>, String> {
    let name = match packet.body.get("name") {
        Some(Value::String(name)) if name.is_empty() => return Err("sink name is empty".into()),
        Some(Value::String(name)) if name.len() > MAX_NAME_BYTES => {
            return Err(format!("sink name is longer than {MAX_NAME_BYTES} bytes"))
        }
        Some(Value::String(name)) => name.clone(),
        Some(_) => return Err("sink name is not a string".into()),
        None => return Ok(None),
    };
    let volume = match packet.body.get("volume") {
        Some(value) => Some(
            value
                .as_i64()
                .filter(|volume| *volume >= 0)
                .ok_or("volume is not a non-negative integer")?,
        ),
        None => None,
    };
    let muted = match packet.body.get("muted") {
        Some(value) => Some(value.as_bool().ok_or("muted is not a boolean")?),
        None => None,
    };
    let enabled = match packet.body.get("enabled") {
        Some(value) => Some(value.as_bool().ok_or("enabled is not a boolean")?),
        None => None,
    };
    if volume.is_none() && muted.is_none() && enabled.is_none() {
        return Err("volume control has no volume, muted or enabled field".into());
    }
    Ok(Some(Control {
        name,
        volume,
        muted,
        enabled,
    }))
}

/// Build the `sinkList` packet from the adapter's sinks.
fn sink_list_packet(sinks: &[AudioSink]) -> Result<Packet, PluginError> {
    let list: Vec<Value> = sinks
        .iter()
        .take(MAX_SINKS)
        .map(|sink| {
            json!({
                "name": sink.name,
                "description": sink.description,
                "volume": sink.volume,
                "maxVolume": sink.max_volume,
                "muted": sink.muted,
                "enabled": sink.enabled,
            })
        })
        .collect();
    Packet::new(types::SYSTEMVOLUME, json!({ "sinkList": list }))
        .map_err(|error| PluginError::InvalidPacket(error.to_string()))
}

async fn current_sinks(
    volume: &dyn handfast_desktop::Volume,
) -> Result<Vec<AudioSink>, PluginError> {
    volume
        .sinks()
        .await
        .map_err(|error| PluginError::Backend(error.to_string()))
}

#[async_trait]
impl Plugin for SystemVolumePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SYSTEMVOLUME
    }

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let Some(volume) = ctx.desktop.volume.as_ref() else {
            return Ok(warn("system volume adapter unavailable"));
        };
        let sinks = current_sinks(volume.as_ref()).await?;
        Ok(PluginOutput::new().with_packet(sink_list_packet(&sinks)?))
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::SYSTEMVOLUME_REQUEST {
            // A peer's own sink list is for the remote-volume direction.
            return Ok(PluginOutput::new());
        }
        let Some(backend) = ctx.desktop.volume.as_ref() else {
            return Ok(warn("system volume adapter unavailable"));
        };
        if packet
            .body
            .get("requestSinks")
            .is_some_and(|value| value.is_boolean() || value.is_null())
        {
            let sinks = current_sinks(backend.as_ref()).await?;
            return Ok(PluginOutput::new().with_packet(sink_list_packet(&sinks)?));
        }
        let control = match parse(packet) {
            Ok(Some(control)) => control,
            Ok(None) => return Ok(warn("discarding system volume packet without a sink name")),
            Err(reason) => return Ok(warn(&format!("discarding system volume packet: {reason}"))),
        };
        if control.enabled.is_some() {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: "changing the default output sink is not supported".to_string(),
            }));
        }
        let known = current_sinks(backend.as_ref()).await?;
        if !known.iter().any(|sink| sink.name == control.name) {
            return Ok(warn(&format!("unknown audio sink {}", control.name)));
        }
        backend
            .set_sink(&control.name, control.volume, control.muted)
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;

        // Push the new state so the peer's sliders follow the change.
        let sinks = current_sinks(backend.as_ref()).await?;
        let mut output = PluginOutput::new().with_packet(sink_list_packet(&sinks)?);
        output.events.push(PluginEvent::Log {
            level: LogLevel::Info,
            message: format!("applied a volume control to sink {}", control.name),
        });
        Ok(output)
    }
}

/// Builds system volume plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemVolumeFactory;

impl PluginFactory for SystemVolumeFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SYSTEMVOLUME
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SystemVolumePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::{Desktop, MockDesktop, VolumeState};
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run(desktop: &Desktop, packet: Packet) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
            config: PluginConfig::empty(),
        };
        SystemVolumePlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn connect_pushes_the_sink_list() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let output = SystemVolumePlugin::new()
            .on_connect(&mut ctx)
            .await
            .unwrap();
        assert_eq!(output.packets.len(), 1);
        let packet = &output.packets[0];
        assert_eq!(packet.ptype, types::SYSTEMVOLUME);
        assert_eq!(packet.body["sinkList"][0]["name"], json!("default"));
        assert_eq!(packet.body["sinkList"][0]["maxVolume"], json!(100));
    }

    #[tokio::test]
    async fn request_sinks_answers_with_the_list() {
        let mock = MockDesktop::new();
        mock.desktop()
            .volume
            .as_ref()
            .unwrap()
            .set(63, true)
            .await
            .unwrap();
        let desktop = mock.desktop();
        let packet =
            Packet::from_body(types::SYSTEMVOLUME_REQUEST, &json!({"requestSinks": true})).unwrap();
        let output = run(&desktop, packet).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body["sinkList"][0]["volume"], json!(63));
        assert_eq!(output.packets[0].body["sinkList"][0]["muted"], json!(true));
    }

    #[tokio::test]
    async fn a_volume_control_addresses_the_named_sink() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::SYSTEMVOLUME_REQUEST,
            &json!({"name": "default", "volume": 15}),
        )
        .unwrap();
        let output = run(&desktop, packet).await;
        assert_eq!(
            mock.volume(),
            VolumeState {
                percent: 15,
                muted: false,
            }
        );
        assert_eq!(output.packets.len(), 1, "the new state is pushed back");
        assert_eq!(output.packets[0].body["sinkList"][0]["volume"], json!(15));
    }

    #[tokio::test]
    async fn a_mute_control_preserves_the_volume() {
        let mock = MockDesktop::new();
        mock.desktop()
            .volume
            .as_ref()
            .unwrap()
            .set(37, false)
            .await
            .unwrap();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::SYSTEMVOLUME_REQUEST,
            &json!({"name": "default", "muted": true}),
        )
        .unwrap();
        run(&desktop, packet).await;
        assert_eq!(
            mock.volume(),
            VolumeState {
                percent: 37,
                muted: true,
            }
        );
    }

    #[tokio::test]
    async fn an_unknown_sink_is_a_warning() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::SYSTEMVOLUME_REQUEST,
            &json!({"name": "hdmi-9", "volume": 10}),
        )
        .unwrap();
        let output = run(&desktop, packet).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
        assert_eq!(mock.volume(), VolumeState::default());
    }

    #[tokio::test]
    async fn a_control_without_a_field_is_a_warning() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet =
            Packet::from_body(types::SYSTEMVOLUME_REQUEST, &json!({"name": "default"})).unwrap();
        let output = run(&desktop, packet).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_peer_sink_list_is_ignored() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::SYSTEMVOLUME_REQUEST,
            &json!({"sinkList": [{"name": "x", "volume": 1, "maxVolume": 2}]}),
        )
        .unwrap();
        let output = run(&desktop, packet).await;
        assert!(output.packets.is_empty());
        assert_eq!(mock.volume(), VolumeState::default());
    }

    #[tokio::test]
    async fn a_host_without_an_adapter_warns() {
        let output = run(
            &Desktop::default(),
            Packet::from_body(types::SYSTEMVOLUME_REQUEST, &json!({"requestSinks": true})).unwrap(),
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

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(SystemVolumePlugin::new().meta(), &roster::SYSTEMVOLUME);
        assert_eq!(SystemVolumeFactory.meta(), &roster::SYSTEMVOLUME);
    }
}
