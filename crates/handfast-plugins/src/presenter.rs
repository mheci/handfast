//! Presenter plugin.

use async_trait::async_trait;
use handfast_desktop::PointerEvent;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{Map, Value};

use crate::roster;

/// Injects relative pointer motion from a presentation remote.
#[derive(Debug, Default)]
pub struct PresenterPlugin;

impl PresenterPlugin {
    /// Create a presenter plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

enum Action {
    Move(PointerEvent),
    Stop,
}

fn warn(message: &str) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.to_string(),
    })
}

fn delta(body: &Map<String, Value>, field: &str) -> Result<f64, &'static str> {
    match body.get(field) {
        None => Ok(0.0),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or("presenter delta is not a number"),
    }
}

fn stop(body: &Map<String, Value>) -> Result<bool, &'static str> {
    match body.get("stop") {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or("presenter stop is not a boolean"),
    }
}

fn parse(packet: &Packet) -> Result<Action, &'static str> {
    if stop(&packet.body)? {
        return Ok(Action::Stop);
    }
    if !packet.body.contains_key("dx") && !packet.body.contains_key("dy") {
        return Err("presenter packet has no delta");
    }
    // shortcut: clamp normalized deltas to one screen; revisit when the input adapter reports geometry.
    Ok(Action::Move(PointerEvent {
        dx: delta(&packet.body, "dx")?.clamp(-1.0, 1.0),
        dy: delta(&packet.body, "dy")?.clamp(-1.0, 1.0),
        scroll: false,
    }))
}

#[async_trait]
impl Plugin for PresenterPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::PRESENTER
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::PRESENTER {
            return Ok(PluginOutput::new());
        }
        let event = match parse(packet) {
            Ok(Action::Move(event)) => event,
            Ok(Action::Stop) => return Ok(PluginOutput::new()),
            Err(message) => return Ok(warn(message)),
        };
        let Some(input) = ctx.desktop.input.as_ref() else {
            return Ok(warn("input adapter unavailable"));
        };
        input
            .pointer(event)
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        Ok(PluginOutput::new())
    }
}

/// Builds presenter plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresenterFactory;

impl PluginFactory for PresenterFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::PRESENTER
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(PresenterPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::mock::InputEvent;
    use handfast_desktop::{Desktop, MockDesktop};
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

    async fn run(desktop: &Desktop, packet: Packet) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        PresenterPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn movement_is_injected_as_relative_pointer_motion() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(types::PRESENTER, &json!({"dx": 0.25, "dy": -0.5})).unwrap();
        let output = run(&desktop, packet).await;
        assert_eq!(
            mock.input_events(),
            vec![InputEvent::Pointer(PointerEvent {
                dx: 0.25,
                dy: -0.5,
                scroll: false,
            })]
        );
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn stop_is_accepted_without_input() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(types::PRESENTER, &json!({"stop": true})).unwrap();
        let output = run(&desktop, packet).await;
        assert!(mock.input_events().is_empty());
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn missing_delta_is_a_warning() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let output = run(&desktop, Packet::empty(types::PRESENTER).unwrap()).await;
        assert!(mock.input_events().is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn malformed_delta_is_a_warning() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet =
            Packet::from_body(types::PRESENTER, &json!({"dx": "left", "dy": 0.0})).unwrap();
        let output = run(&desktop, packet).await;
        assert!(mock.input_events().is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn out_of_range_delta_is_clamped() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let packet = Packet::from_body(types::PRESENTER, &json!({"dx": 5.0, "dy": -5.0})).unwrap();
        run(&desktop, packet).await;
        assert_eq!(
            mock.input_events(),
            vec![InputEvent::Pointer(PointerEvent {
                dx: 1.0,
                dy: -1.0,
                scroll: false,
            })]
        );
    }

    #[tokio::test]
    async fn missing_input_adapter_is_a_warning() {
        let desktop = Desktop::default();
        let packet = Packet::from_body(types::PRESENTER, &json!({"dx": 0.1, "dy": 0.1})).unwrap();
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
    async fn foreign_packets_are_ignored() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let output = run(&desktop, Packet::empty(types::PING).unwrap()).await;
        assert!(mock.input_events().is_empty());
        assert!(output.events.is_empty());
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(PresenterPlugin::new().meta(), &roster::PRESENTER);
        assert_eq!(PresenterFactory.meta(), &roster::PRESENTER);
    }
}
