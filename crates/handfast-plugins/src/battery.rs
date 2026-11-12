//! Battery plugin.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    BatteryState, LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory,
    PluginMeta, PluginOutput,
};
use serde_json::Value;

use crate::roster;

/// Parses battery reports and emits typed state.
#[derive(Debug, Default)]
pub struct BatteryPlugin;

impl BatteryPlugin {
    /// Create a battery plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn parse(packet: &Packet) -> Option<BatteryState> {
    if packet.ptype != types::BATTERY {
        return None;
    }
    let raw_charge = packet.body.get("currentCharge").and_then(Value::as_i64)?;
    let charging = packet.body.get("isCharging").and_then(Value::as_bool)?;
    let threshold = packet
        .body
        .get("thresholdEvent")
        .and_then(Value::as_u64)
        .map(|value| value.min(1) as u8);
    let charge = match raw_charge {
        -1 => None,
        0..=100 => Some(raw_charge as u8),
        _ => return None,
    };
    Some(BatteryState {
        charge,
        charging,
        threshold,
    })
}

#[async_trait]
impl Plugin for BatteryPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::BATTERY
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::BATTERY {
            return Ok(PluginOutput::new());
        }
        match parse(packet) {
            Some(state) => Ok(PluginOutput::new().with_event(PluginEvent::Battery(state))),
            None => Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "discarding malformed battery report".to_string(),
            })),
        }
    }
}

/// Builds battery plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct BatteryFactory;

impl PluginFactory for BatteryFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::BATTERY
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(BatteryPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;
    use serde_json::json;

    async fn run(packet: Packet) -> PluginOutput {
        let desktop = MockDesktop::new();
        let device = Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        );
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop.desktop(),
        };
        BatteryPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn parses_battery_report() {
        let output = run(Packet::from_body(
            types::BATTERY,
            &json!({"currentCharge": 42, "isCharging": true, "thresholdEvent": 1}),
        )
        .unwrap())
        .await;
        assert_eq!(
            output.events,
            vec![PluginEvent::Battery(BatteryState {
                charge: Some(42),
                charging: true,
                threshold: Some(1),
            })]
        );
    }

    #[tokio::test]
    async fn negative_charge_means_no_battery() {
        let output = run(Packet::from_body(
            types::BATTERY,
            &json!({"currentCharge": -1, "isCharging": false}),
        )
        .unwrap())
        .await;
        assert_eq!(
            output.events,
            vec![PluginEvent::Battery(BatteryState {
                charge: None,
                charging: false,
                threshold: None,
            })]
        );
    }

    #[tokio::test]
    async fn malformed_report_is_logged_not_fatal() {
        let output =
            run(Packet::from_body(types::BATTERY, &json!({"currentCharge": 200})).unwrap()).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[test]
    fn threshold_is_clamped() {
        let packet = Packet::from_body(
            types::BATTERY,
            &json!({"currentCharge": 10, "isCharging": false, "thresholdEvent": 99}),
        )
        .unwrap();
        assert_eq!(parse(&packet).unwrap().threshold, Some(1));
    }
}
