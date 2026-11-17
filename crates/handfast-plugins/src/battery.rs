//! Battery plugin.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use handfast_desktop::PowerState;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    BatteryState, LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory,
    PluginMeta, PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Reports the host battery and parses phone battery reports.
#[derive(Debug)]
pub struct BatteryPlugin {
    interval: Duration,
    last_poll: Option<Instant>,
    last_report: Option<Option<PowerState>>,
}

impl BatteryPlugin {
    /// Create a battery plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interval: POLL_INTERVAL,
            last_poll: None,
            last_report: None,
        }
    }

    /// Create a battery plugin with a custom poll interval.
    #[must_use]
    pub const fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            last_poll: None,
            last_report: None,
        }
    }
}

impl Default for BatteryPlugin {
    fn default() -> Self {
        Self::new()
    }
}

fn battery_packet(state: Option<PowerState>) -> Result<Packet, ProtoError> {
    let (charge, charging) = match state {
        Some(state) => (i64::from(state.percent), state.charging),
        None => (-1, false),
    };
    Packet::new(
        types::BATTERY,
        json!({ "currentCharge": charge, "isCharging": charging }),
    )
}

fn failure(message: impl Into<String>) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.into(),
    })
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

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let request = Packet::new(types::BATTERY_REQUEST, json!({"request": true}))
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        let mut output = PluginOutput::new().with_packet(request);
        if let Some(power) = ctx.desktop.power.as_ref() {
            match power.state().await {
                Ok(state) => {
                    let packet = battery_packet(state)
                        .map_err(|error| PluginError::Backend(error.to_string()))?;
                    output = output.with_packet(packet);
                    self.last_report = Some(state);
                }
                Err(error) => {
                    output = output.with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("failed to read the host battery: {error}"),
                    });
                }
            }
        }
        self.last_poll = Some(Instant::now());
        Ok(output)
    }

    async fn on_tick(&mut self, ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        let Some(power) = ctx.desktop.power.as_ref() else {
            return Ok(PluginOutput::new());
        };
        if let Some(last) = self.last_poll {
            if last.elapsed() < self.interval {
                return Ok(PluginOutput::new());
            }
        }
        self.last_poll = Some(Instant::now());
        match power.state().await {
            Ok(state) => {
                if self.last_report == Some(state) {
                    return Ok(PluginOutput::new());
                }
                self.last_report = Some(state);
                let packet = battery_packet(state)
                    .map_err(|error| PluginError::Backend(error.to_string()))?;
                Ok(PluginOutput::new().with_packet(packet))
            }
            Err(error) => Ok(failure(format!("failed to read the host battery: {error}"))),
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
    use std::sync::{Arc, Mutex};

    use super::*;
    use handfast_desktop::{Desktop, MockDesktop, Power, Result as DesktopResult};
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

    struct FakePower {
        state: Mutex<Option<PowerState>>,
    }

    #[async_trait]
    impl Power for FakePower {
        async fn state(&self) -> DesktopResult<Option<PowerState>> {
            Ok(*self.state.lock().unwrap())
        }
    }

    fn desktop_with_power(state: Option<PowerState>) -> (Arc<FakePower>, Desktop) {
        let power = Arc::new(FakePower {
            state: Mutex::new(state),
        });
        let desktop = Desktop {
            power: Some(Arc::clone(&power) as Arc<dyn Power>),
            ..Desktop::default()
        };
        (power, desktop)
    }

    async fn run_connect(desktop: &Desktop, plugin: &mut BatteryPlugin) -> PluginOutput {
        let device = Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        );
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_connect(&mut ctx).await.unwrap()
    }

    async fn run_tick(desktop: &Desktop, plugin: &mut BatteryPlugin) -> PluginOutput {
        let device = Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        );
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    #[tokio::test]
    async fn connect_requests_the_phone_and_reports_the_host_battery() {
        let (_power, desktop) = desktop_with_power(Some(PowerState {
            percent: 42,
            charging: true,
        }));
        let mut plugin = BatteryPlugin::new();

        let output = run_connect(&desktop, &mut plugin).await;

        assert_eq!(output.packets.len(), 2);
        assert_eq!(output.packets[0].ptype, types::BATTERY_REQUEST);
        assert_eq!(output.packets[0].body["request"], true);
        assert_eq!(output.packets[1].ptype, types::BATTERY);
        assert_eq!(output.packets[1].body["currentCharge"], 42);
        assert_eq!(output.packets[1].body["isCharging"], true);
    }

    #[tokio::test]
    async fn connect_without_power_requests_the_phone_only() {
        let desktop = Desktop::default();
        let mut plugin = BatteryPlugin::new();

        let output = run_connect(&desktop, &mut plugin).await;

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::BATTERY_REQUEST);
    }

    #[tokio::test]
    async fn tick_reports_a_change_once() {
        let (power, desktop) = desktop_with_power(Some(PowerState {
            percent: 42,
            charging: true,
        }));
        let mut plugin = BatteryPlugin::with_interval(Duration::ZERO);
        run_connect(&desktop, &mut plugin).await;
        *power.state.lock().unwrap() = Some(PowerState {
            percent: 41,
            charging: false,
        });

        let output = run_tick(&desktop, &mut plugin).await;

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body["currentCharge"], 41);
        assert!(run_tick(&desktop, &mut plugin).await.packets.is_empty());
    }

    #[tokio::test]
    async fn tick_reports_no_battery_as_minus_one() {
        let (power, desktop) = desktop_with_power(None);
        let mut plugin = BatteryPlugin::with_interval(Duration::ZERO);
        let output = run_connect(&desktop, &mut plugin).await;
        assert_eq!(output.packets[1].body["currentCharge"], -1);

        *power.state.lock().unwrap() = Some(PowerState {
            percent: 10,
            charging: false,
        });
        let output = run_tick(&desktop, &mut plugin).await;

        assert_eq!(output.packets[0].body["currentCharge"], 10);
    }

    #[tokio::test]
    async fn tick_respects_the_interval() {
        let (power, desktop) = desktop_with_power(Some(PowerState {
            percent: 42,
            charging: true,
        }));
        let mut plugin = BatteryPlugin::new();
        run_connect(&desktop, &mut plugin).await;
        *power.state.lock().unwrap() = Some(PowerState {
            percent: 41,
            charging: false,
        });

        assert!(run_tick(&desktop, &mut plugin).await.packets.is_empty());
    }

    #[tokio::test]
    async fn tick_without_power_is_empty() {
        let desktop = Desktop::default();
        let mut plugin = BatteryPlugin::with_interval(Duration::ZERO);

        assert!(run_tick(&desktop, &mut plugin).await.packets.is_empty());
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
