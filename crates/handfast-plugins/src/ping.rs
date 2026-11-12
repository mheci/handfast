//! Ping plugin.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta, PluginOutput,
};
use serde_json::Value;

use crate::roster;

/// Emits a typed event for every inbound ping.
#[derive(Debug, Default)]
pub struct PingPlugin;

impl PingPlugin {
    /// Create a ping plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Plugin for PingPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::PING
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::PING {
            return Ok(PluginOutput::new());
        }
        let message = packet
            .body
            .get("message")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        Ok(PluginOutput::new().with_event(PluginEvent::Ping { message }))
    }
}

/// Builds ping plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct PingFactory;

impl PluginFactory for PingFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::PING
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(PingPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;
    use serde_json::json;

    fn context<'a>(
        desktop: &'a handfast_desktop::Desktop,
        device: &'a Device,
    ) -> PluginContext<'a> {
        PluginContext { device, desktop }
    }

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Desktop,
        )
    }

    #[tokio::test]
    async fn ping_emits_event() {
        let desktop = MockDesktop::new();
        let device = device();
        let desktop = desktop.desktop();
        let mut ctx = context(&desktop, &device);
        let packet = Packet::from_body(types::PING, &json!({"message": "hello"})).unwrap();
        let output = PingPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap();
        assert_eq!(
            output.events,
            vec![PluginEvent::Ping {
                message: Some("hello".to_string())
            }]
        );
    }

    #[tokio::test]
    async fn ping_without_message_is_valid() {
        let desktop = MockDesktop::new();
        let device = device();
        let desktop = desktop.desktop();
        let mut ctx = context(&desktop, &device);
        let packet = Packet::empty(types::PING).unwrap();
        let output = PingPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap();
        assert_eq!(output.events, vec![PluginEvent::Ping { message: None }]);
    }

    #[tokio::test]
    async fn foreign_packets_are_ignored() {
        let desktop = MockDesktop::new();
        let device = device();
        let desktop = desktop.desktop();
        let mut ctx = context(&desktop, &device);
        let packet = Packet::empty(types::BATTERY).unwrap();
        let output = PingPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap();
        assert!(output.packets.is_empty());
        assert!(output.events.is_empty());
    }
}
