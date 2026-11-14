//! Connectivity report plugin.

use std::collections::BTreeMap;

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};

use crate::roster;

/// One subscription's reported network state.
#[derive(Debug)]
struct Subscription {
    network_type: String,
    strength: u8,
}

/// Parsed `kdeconnect.connectivity_report` body.
#[derive(Debug)]
struct ConnectivityReport {
    subscriptions: BTreeMap<String, Subscription>,
}

impl ConnectivityReport {
    fn summary(&self) -> String {
        if self.subscriptions.is_empty() {
            return "connectivity: no subscriptions".to_string();
        }
        let parts: Vec<String> = self
            .subscriptions
            .iter()
            .map(|(id, subscription)| {
                format!(
                    "{id} {} {}/4",
                    subscription.network_type, subscription.strength
                )
            })
            .collect();
        format!("connectivity: {}", parts.join(", "))
    }
}

fn parse(packet: &Packet) -> Option<ConnectivityReport> {
    let strengths = packet.body.get("signalStrengths")?.as_object()?;
    let mut subscriptions = BTreeMap::new();
    for (id, value) in strengths {
        let entry = value.as_object()?;
        let network_type = entry.get("networkType")?.as_str()?.to_string();
        let strength = u8::try_from(entry.get("signalStrength")?.as_u64()?).ok()?;
        if strength > 4 {
            return None;
        }
        subscriptions.insert(
            id.clone(),
            Subscription {
                network_type,
                strength,
            },
        );
    }
    Some(ConnectivityReport { subscriptions })
}

/// Parses connectivity reports and logs a summary.
#[derive(Debug, Default)]
pub struct ConnectivityReportPlugin;

impl ConnectivityReportPlugin {
    /// Create a connectivity report plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Plugin for ConnectivityReportPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CONNECTIVITY_REPORT
    }

    async fn on_connect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let request = Packet::empty(types::CONNECTIVITY_REPORT_REQUEST)
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(request))
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::CONNECTIVITY_REPORT {
            return Ok(PluginOutput::new());
        }
        match parse(packet) {
            Some(report) => Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: report.summary(),
            })),
            None => Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "discarding malformed connectivity report".to_string(),
            })),
        }
    }
}

/// Builds connectivity report plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectivityReportFactory;

impl PluginFactory for ConnectivityReportFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CONNECTIVITY_REPORT
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ConnectivityReportPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;
    use serde_json::json;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run(packet: Packet) -> PluginOutput {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        ConnectivityReportPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn valid_report_emits_summary() {
        let output = run(Packet::from_body(
            types::CONNECTIVITY_REPORT,
            &json!({
                "signalStrengths": {
                    "6": {"networkType": "4G", "signalStrength": 3},
                    "17": {"networkType": "HSPA", "signalStrength": 0}
                }
            }),
        )
        .unwrap())
        .await;
        assert_eq!(
            output.events,
            vec![PluginEvent::Log {
                level: LogLevel::Info,
                message: "connectivity: 17 HSPA 0/4, 6 4G 3/4".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn empty_report_logs_no_subscriptions() {
        let output = run(Packet::from_body(
            types::CONNECTIVITY_REPORT,
            &json!({"signalStrengths": {}}),
        )
        .unwrap())
        .await;
        assert_eq!(
            output.events,
            vec![PluginEvent::Log {
                level: LogLevel::Info,
                message: "connectivity: no subscriptions".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn malformed_report_is_a_warning() {
        for body in [
            json!({}),
            json!({"signalStrengths": []}),
            json!({"signalStrengths": {"6": {"signalStrength": 3}}}),
            json!({"signalStrengths": {"6": {"networkType": "4G"}}}),
            json!({"signalStrengths": {"6": {"networkType": "4G", "signalStrength": 5}}}),
            json!({"signalStrengths": {"6": {"networkType": "4G", "signalStrength": -1}}}),
        ] {
            let output = run(Packet::from_body(types::CONNECTIVITY_REPORT, &body).unwrap()).await;
            assert!(
                matches!(
                    output.events.as_slice(),
                    [PluginEvent::Log {
                        level: LogLevel::Warn,
                        ..
                    }]
                ),
                "expected one warning for {body}"
            );
        }
    }

    #[tokio::test]
    async fn on_connect_requests_a_report() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let output = ConnectivityReportPlugin::new()
            .on_connect(&mut ctx)
            .await
            .unwrap();
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::CONNECTIVITY_REPORT_REQUEST);
        assert!(output.packets[0].body.is_empty());
        assert!(output.events.is_empty());
    }

    #[test]
    fn meta_matches_roster() {
        assert_eq!(
            ConnectivityReportPlugin::new().meta(),
            &roster::CONNECTIVITY_REPORT
        );
        assert_eq!(
            ConnectivityReportFactory.meta(),
            &roster::CONNECTIVITY_REPORT
        );
    }
}
