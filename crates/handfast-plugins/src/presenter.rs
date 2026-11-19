//! Presenter plugin.
//!
//! The desktop client does not move the pointer for this plugin. It shows a
//! fullscreen, click-through window with a marker and moves that marker by the
//! normalised `dx`/`dy` of each packet, starting from the centre. `stop` closes
//! the view and a 500 ms timer closes it when the packets stop, so a
//! presentation remote can drive a slide show without taking over the mouse.
//!
//! Handfast follows that behaviour through the [`Presenter`] desktop adapter.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{Map, Value};

use crate::roster;

/// Deltas outside this range are not a presentation move.
///
/// A whole screen per packet is already the maximum the desktop client can act
/// on, because it clamps the accumulated position to 0-1.
const MAX_DELTA: f64 = 1.0;

/// Notifies the on-screen presentation view of remote moves.
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
    Move { dx: f32, dy: f32 },
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
            .filter(|value| value.abs() <= MAX_DELTA)
            .ok_or("presenter delta is not a usable number"),
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
    Ok(Action::Move {
        dx: delta(&packet.body, "dx")? as f32,
        dy: delta(&packet.body, "dy")? as f32,
    })
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
        let action = match parse(packet) {
            Ok(action) => action,
            Err(message) => return Ok(warn(message)),
        };
        let Some(presenter) = ctx.desktop.presenter.as_ref() else {
            return Ok(warn("presentation view unavailable"));
        };
        match action {
            Action::Move { dx, dy } => presenter
                .present(dx, dy)
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?,
            Action::Stop => presenter
                .stop()
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?,
        }
        Ok(PluginOutput::new())
    }

    async fn on_disconnect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        // A dropped link must not leave a marker on screen.
        if let Some(presenter) = ctx.desktop.presenter.as_ref() {
            if let Err(error) = presenter.stop().await {
                tracing::debug!(%error, "failed to close the presentation view");
            }
        }
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

    async fn run(desktop: &Desktop, body: Value) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        let packet = Packet::from_body(types::PRESENTER, &body).unwrap();
        PresenterPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_move_reaches_the_presentation_view() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"dx": 0.25, "dy": -0.1})).await;
        assert!(output.events.is_empty());
        assert_eq!(mock.presentations(), vec![Some((0.25, -0.1))]);
    }

    #[tokio::test]
    async fn stop_closes_the_view() {
        let mock = MockDesktop::new();
        run(&mock.desktop(), json!({"stop": true})).await;
        assert_eq!(mock.presentations(), vec![None]);
    }

    #[tokio::test]
    async fn an_out_of_range_delta_is_rejected() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({"dx": 25.0})).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log { level: LogLevel::Warn, .. }]
        ));
        assert!(mock.presentations().is_empty());
    }

    #[tokio::test]
    async fn a_packet_without_a_delta_is_rejected() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), json!({})).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log { level: LogLevel::Warn, .. }]
        ));
    }

    #[tokio::test]
    async fn disconnect_closes_the_view() {
        let mock = MockDesktop::new();
        let device = device();
        let desktop = mock.desktop();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        PresenterPlugin::new()
            .on_disconnect(&mut ctx)
            .await
            .unwrap();
        assert_eq!(mock.presentations(), vec![None]);
    }
}
