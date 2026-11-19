//! Battery plugin.
//!
//! The protocol is symmetric but requestless: either side sends
//! `kdeconnect.battery` with `currentCharge`, `isCharging` and
//! `thresholdEvent`, and the other side simply records it. The desktop client
//! pushes its state from `connected()` and re-sends on every change; both
//! official clients ignore `currentCharge` of -1, which means "no battery".
//!
//! Handfast therefore reports the host battery on connect, re-reports it when
//! it changes, and parses the peer's reports so a UI can show the phone's
//! charge.

use async_trait::async_trait;
use handfast_desktop::traits::BatteryStatus;
use handfast_proto::{types, Packet};
use handfast_runtime::{BatteryState, LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory,
    PluginMeta, PluginOutput};
use serde_json::{json, Value};

use crate::roster;

/// Below this charge the desktop client raises a threshold event.
pub const LOW_CHARGE: i32 = 15;

/// Build the `kdeconnect.battery` body for a host battery state.
///
/// `currentCharge` is -1 when the host has no battery, which is what both
/// official clients send and expect.
#[must_use]
pub fn battery_packet(status: BatteryStatus) -> Packet {
    let mut body = serde_json::Map::new();
    body.insert("currentCharge".to_string(), json!(status.charge));
    body.insert("isCharging".to_string(), json!(status.charging));
    body.insert(
        "thresholdEvent".to_string(),
        json!(i64::from(status.threshold.min(1))),
    );
    Packet {
        id: 0,
        ptype: types::BATTERY.to_string(),
        body,
        payload_size: None,
        payload_transfer_info: None,
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

/// Reports the host battery and parses the peer's.
#[derive(Debug, Default)]
pub struct BatteryPlugin {
    last_reported: Option<BatteryStatus>,
}

impl BatteryPlugin {
    /// Create a battery plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_reported: None,
        }
    }

    /// Send the host state, skipping an unchanged repeat unless `force`.
    async fn report(
        &mut self,
        ctx: &mut PluginContext<'_>,
        force: bool,
    ) -> Result<Option<Packet>, PluginError> {
        let Some(battery) = ctx.desktop.battery.as_ref() else {
            return Ok(None);
        };
        let status = battery
            .status()
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        if !force && self.last_reported == Some(status) {
            return Ok(None);
        }
        self.last_reported = Some(status);
        Ok(Some(battery_packet(status)))
    }
}

#[async_trait]
impl Plugin for BatteryPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::BATTERY
    }

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        if ctx.desktop.battery.is_none() {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "battery adapter unavailable".to_string(),
            }));
        }
        // The desktop client pushes its state on connect and never asks; a
        // connected peer does the same, so both sides learn each other's
        // charge without an extra round trip.
        match self.report(ctx, true).await? {
            Some(packet) => Ok(PluginOutput::new().with_packet(packet)),
            None => Ok(PluginOutput::new()),
        }
    }

    async fn on_tick(&mut self, ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if ctx.desktop.battery.is_none() {
            return Ok(PluginOutput::new());
        }
        match self.report(ctx, false).await? {
            Some(packet) => Ok(PluginOutput::new().with_packet(packet)),
            None => Ok(PluginOutput::new()),
        }
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
    use handfast_runtime::{Device, PluginConfig};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run(desktop: &handfast_desktop::Desktop, packet: Packet) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
            config: PluginConfig::empty(),
        };
        BatteryPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_peer_report_becomes_a_typed_event() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::BATTERY,
            &json!({"currentCharge": 55, "isCharging": true}),
        )
        .unwrap();
        let output = run(&desktop, packet).await;
        assert_eq!(
            output.events,
            vec![PluginEvent::Battery(BatteryState {
                charge: Some(55),
                charging: true,
                threshold: None,
            })]
        );
    }

    #[tokio::test]
    async fn a_negative_charge_means_no_battery() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::BATTERY,
            &json!({"currentCharge": -1, "isCharging": false}),
        )
        .unwrap();
        let output = run(&desktop, packet).await;
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
    async fn an_impossible_charge_is_discarded() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(
            types::BATTERY,
            &json!({"currentCharge": 101, "isCharging": false}),
        )
        .unwrap();
        let output = run(&desktop, packet).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log { .. }]
        ));
    }

    #[tokio::test]
    async fn the_host_report_carries_every_wire_field() {
        let mock = MockDesktop::new();
        mock.set_battery(BatteryStatus {
            charge: 33,
            charging: false,
            threshold: 0,
        });
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let output = BatteryPlugin::new().on_tick(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 1);
        let packet = &output.packets[0];
        assert_eq!(packet.ptype, types::BATTERY);
        assert_eq!(packet.body["currentCharge"], json!(33));
        assert_eq!(packet.body["isCharging"], json!(false));
        assert_eq!(packet.body["thresholdEvent"], json!(0));
    }

    #[tokio::test]
    async fn connect_reports_without_asking() {
        let mock = MockDesktop::new();
        mock.set_battery(BatteryStatus::default());
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let output = BatteryPlugin::new().on_connect(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 1, "one state push, no request");
        assert_eq!(output.packets[0].ptype, types::BATTERY);
        assert_eq!(output.packets[0].body["currentCharge"], json!(-1));
    }

    #[tokio::test]
    async fn a_tick_skips_an_unchanged_report() {
        let mock = MockDesktop::new();
        mock.set_battery(BatteryStatus {
            charge: 61,
            charging: true,
            threshold: 0,
        });
        let desktop = mock.desktop();
        let device = device();
        let mut plugin = BatteryPlugin::new();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let first = plugin.on_tick(&mut ctx).await.unwrap();
        assert_eq!(first.packets.len(), 1);
        let second = plugin.on_tick(&mut ctx).await.unwrap();
        assert!(
            second.packets.is_empty(),
            "an unchanged state is not re-sent"
        );
        mock.set_battery(BatteryStatus {
            charge: 60,
            charging: true,
            threshold: 0,
        });
        let third = plugin.on_tick(&mut ctx).await.unwrap();
        assert_eq!(third.packets.len(), 1, "a change is re-sent");
    }

    #[tokio::test]
    async fn a_host_without_an_adapter_reports_nothing() {
        let desktop = handfast_desktop::Desktop::default();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let output = BatteryPlugin::new().on_connect(&mut ctx).await.unwrap();
        assert!(output.packets.is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
        let output = BatteryPlugin::new().on_tick(&mut ctx).await.unwrap();
        assert!(output.packets.is_empty());
    }
}
