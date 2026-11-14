//! System volume plugin.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

const SINK_NAME: &str = "default";
const SINK_DESCRIPTION: &str = "System volume";
const MAX_VOLUME: u8 = 100;

/// Applies inbound volume controls to the desktop output volume.
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

fn percent(value: &Value) -> Option<u8> {
    let value = u8::try_from(value.as_u64()?).ok()?;
    (value <= 100).then_some(value)
}

fn parse(packet: &Packet) -> Option<(Option<u8>, Option<bool>)> {
    let raw_volume = packet.body.get("volume");
    let raw_muted = packet.body.get("muted");
    if raw_volume.is_none() && raw_muted.is_none() {
        return None;
    }
    let volume = match raw_volume {
        Some(value) => Some(percent(value)?),
        None => None,
    };
    let muted = match raw_muted {
        Some(value) => Some(value.as_bool()?),
        None => None,
    };
    Some((volume, muted))
}

fn sink_list_packet(percent: u8, muted: bool) -> Result<Packet, PluginError> {
    Packet::new(
        types::SYSTEMVOLUME,
        json!({
            "sinkList": [{
                "name": SINK_NAME,
                "description": SINK_DESCRIPTION,
                "volume": percent,
                "muted": muted,
                "maxVolume": MAX_VOLUME,
                "enabled": true,
            }]
        }),
    )
    .map_err(|error| PluginError::InvalidPacket(error.to_string()))
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
        if ctx.desktop.volume.is_none() {
            return Ok(warn("system volume adapter unavailable"));
        }
        let packet = Packet::new(types::SYSTEMVOLUME_REQUEST, json!({ "requestSinks": true }))
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if !matches!(
            packet.ptype.as_str(),
            types::SYSTEMVOLUME | types::SYSTEMVOLUME_REQUEST
        ) {
            return Ok(PluginOutput::new());
        }
        let Some(backend) = ctx.desktop.volume.as_ref() else {
            return Ok(warn("system volume adapter unavailable"));
        };
        if packet.body.get("requestSinks").and_then(Value::as_bool) == Some(true) {
            let current = backend
                .get()
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?;
            return Ok(
                PluginOutput::new().with_packet(sink_list_packet(current.percent, current.muted)?)
            );
        }
        let Some((volume, muted)) = parse(packet) else {
            return Ok(warn(
                "discarding system volume packet without volume or muted",
            ));
        };
        let (percent, muted) = match (volume, muted) {
            (Some(percent), Some(muted)) => (percent, muted),
            (Some(percent), None) => {
                let current = backend
                    .get()
                    .await
                    .map_err(|error| PluginError::Backend(error.to_string()))?;
                (percent, current.muted)
            }
            (None, Some(muted)) => {
                let current = backend
                    .get()
                    .await
                    .map_err(|error| PluginError::Backend(error.to_string()))?;
                (current.percent, muted)
            }
            (None, None) => {
                return Ok(warn(
                    "discarding system volume packet without volume or muted",
                ))
            }
        };
        backend
            .set(percent, muted)
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        let message = if muted {
            format!("system volume set to {percent}% (muted)")
        } else {
            format!("system volume set to {percent}%")
        };
        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message,
        }))
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
    use handfast_runtime::Device;

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
        };
        SystemVolumePlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn volume_packet_updates_the_desktop() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet =
            Packet::from_body(types::SYSTEMVOLUME, &json!({"volume": 42, "muted": true})).unwrap();
        let output = run(&desktop, packet).await;
        assert_eq!(
            mock.volume(),
            VolumeState {
                percent: 42,
                muted: true,
            }
        );
        assert_eq!(
            output.events,
            vec![PluginEvent::Log {
                level: LogLevel::Info,
                message: "system volume set to 42% (muted)".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn mute_only_packet_preserves_volume() {
        let mock = MockDesktop::new();
        let volume = mock.desktop().volume.clone().unwrap();
        volume.set(37, false).await.unwrap();
        let desktop = mock.desktop();
        let packet = Packet::from_body(types::SYSTEMVOLUME, &json!({"muted": true})).unwrap();
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
    async fn volume_request_updates_the_desktop() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::SYSTEMVOLUME_REQUEST,
            &json!({"name": "default", "volume": 15}),
        )
        .unwrap();
        run(&desktop, packet).await;
        assert_eq!(
            mock.volume(),
            VolumeState {
                percent: 15,
                muted: false,
            }
        );
    }

    #[tokio::test]
    async fn request_sinks_returns_the_current_sink() {
        let mock = MockDesktop::new();
        let volume = mock.desktop().volume.clone().unwrap();
        volume.set(63, true).await.unwrap();
        let desktop = mock.desktop();
        let packet =
            Packet::from_body(types::SYSTEMVOLUME_REQUEST, &json!({"requestSinks": true})).unwrap();
        let output = run(&desktop, packet).await;
        assert_eq!(
            output.packets,
            vec![Packet::from_body(
                types::SYSTEMVOLUME,
                &json!({
                    "sinkList": [{
                        "name": "default",
                        "description": "System volume",
                        "volume": 63,
                        "muted": true,
                        "maxVolume": 100,
                        "enabled": true,
                    }]
                }),
            )
            .unwrap()]
        );
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn malformed_volume_packet_is_a_warning() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::SYSTEMVOLUME,
            &json!({"volume": "loud", "muted": true}),
        )
        .unwrap();
        let output = run(&desktop, packet).await;
        assert_eq!(mock.volume(), VolumeState::default());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn missing_volume_adapter_is_a_warning() {
        let desktop = Desktop::default();
        let packet =
            Packet::from_body(types::SYSTEMVOLUME, &json!({"volume": 10, "muted": false})).unwrap();
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
    async fn on_connect_without_adapter_is_a_warning() {
        let desktop = Desktop::default();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let output = SystemVolumePlugin::new()
            .on_connect(&mut ctx)
            .await
            .unwrap();
        assert!(output.packets.is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn on_connect_requests_the_sink_list() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let output = SystemVolumePlugin::new()
            .on_connect(&mut ctx)
            .await
            .unwrap();
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::SYSTEMVOLUME_REQUEST);
        assert_eq!(output.packets[0].body["requestSinks"], json!(true));
        assert!(output.events.is_empty());
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(SystemVolumePlugin::new().meta(), &roster::SYSTEMVOLUME);
        assert_eq!(SystemVolumeFactory.meta(), &roster::SYSTEMVOLUME);
    }
}
