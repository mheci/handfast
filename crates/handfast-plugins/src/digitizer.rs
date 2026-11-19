//! Turn a graphics tablet or touchscreen into a phone drawing surface.
//!
//! The phone sends two packet types and the desktop sends none:
//!
//! - `kdeconnect.digitizer.session` announces the surface: `active`, `touching`,
//!   `tool` (`pen`, `eraser`, `finger`), and the geometry — `width`, `height`,
//!   `resolutionX`, `resolutionY`. The geometry matters because the phone's
//!   coordinates are in tablet units, not screen pixels.
//! - `kdeconnect.digitizer` carries one sample: `x`, `y`, `pressure`, and the
//!   state flags.
//!
//! Handfast maps both onto [`handfast_desktop::Input`]. Coordinates are already
//! normalised by the phone against the announced geometry, so the plugin passes
//! them through unchanged; only `pressure` is clamped, because a pointer
//! backend has no pressure axis and the value is informational.
//!
//! A sample is dropped when no session is active. The phone re-sends the
//! session on reconnect, but a stray sample must never move the desktop
//! pointer, so the guard is on the session, not on the sample.

use async_trait::async_trait;

use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

/// Largest accepted surface dimension, in tablet units.
const MAX_EXTENT: f64 = 100_000.0;

/// Tools the wire announces.
const TOOLS: [&str; 3] = ["pen", "eraser", "finger"];

/// State of the drawing surface.
#[derive(Debug, Clone, PartialEq)]
pub struct DigitizerSession {
    /// Whether the surface accepts input.
    pub active: bool,
    /// Whether the tool is currently down.
    pub touching: bool,
    /// Tool name: `pen`, `eraser` or `finger`.
    pub tool: String,
    /// Surface width in tablet units.
    pub width: f64,
    /// Surface height in tablet units.
    pub height: f64,
    /// Horizontal resolution, kept for the phone's benefit.
    pub resolution_x: i64,
    /// Vertical resolution.
    pub resolution_y: i64,
}

impl Default for DigitizerSession {
    fn default() -> Self {
        Self {
            active: false,
            touching: false,
            tool: "pen".to_string(),
            width: 0.0,
            height: 0.0,
            resolution_x: 0,
            resolution_y: 0,
        }
    }
}

impl DigitizerSession {
    /// Read a session packet.
    ///
    /// Unknown tools fall back to `pen` rather than refusing the session: a
    /// newer phone naming a tool Handfast does not model should still be able
    /// to draw.
    #[must_use]
    pub fn from_body(body: &serde_json::Map<String, Value>) -> Self {
        let tool = body
            .get("tool")
            .and_then(Value::as_str)
            .filter(|tool| TOOLS.contains(tool))
            .unwrap_or("pen")
            .to_string();
        Self {
            active: body.get("active").and_then(Value::as_bool).unwrap_or(false),
            touching: body
                .get("touching")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tool,
            width: body
                .get("width")
                .and_then(Value::as_f64)
                .filter(|width| (0.0..=MAX_EXTENT).contains(width))
                .unwrap_or(0.0),
            height: body
                .get("height")
                .and_then(Value::as_f64)
                .filter(|height| (0.0..=MAX_EXTENT).contains(height))
                .unwrap_or(0.0),
            resolution_x: body.get("resolutionX").and_then(Value::as_i64).unwrap_or(0),
            resolution_y: body.get("resolutionY").and_then(Value::as_i64).unwrap_or(0),
        }
    }

    /// Whether the geometry is usable for normalising coordinates.
    #[must_use]
    pub fn has_geometry(&self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }

    /// Whether this session can receive samples.
    #[must_use]
    pub const fn can_draw(&self) -> bool {
        self.active
    }
}

/// One pointer sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Horizontal position in tablet units.
    pub x: f64,
    /// Vertical position in tablet units.
    pub y: f64,
    /// Pressure, 0.0-1.0.
    pub pressure: f64,
}

impl Sample {
    /// Read a sample packet.
    #[must_use]
    pub fn from_body(body: &serde_json::Map<String, Value>) -> Option<Self> {
        let x = body.get("x").and_then(Value::as_f64)?;
        let y = body.get("y").and_then(Value::as_f64)?;
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let pressure = body
            .get("pressure")
            .and_then(Value::as_f64)
            .filter(|pressure| pressure.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        Some(Self { x, y, pressure })
    }

    /// Normalise to 0.0-1.0 against a session geometry.
    #[must_use]
    pub fn normalise(&self, session: &DigitizerSession) -> (f64, f64) {
        if !session.has_geometry() {
            return (0.0, 0.0);
        }
        (
            (self.x / session.width).clamp(0.0, 1.0),
            (self.y / session.height).clamp(0.0, 1.0),
        )
    }
}

/// Maps phone tablet input onto the desktop pointer.
#[derive(Debug, Default)]
pub struct DigitizerPlugin {
    session: DigitizerSession,
    /// Samples forwarded, for tests and the log.
    forwarded: usize,
    /// Samples dropped because no session was active.
    dropped: usize,
}

impl DigitizerPlugin {
    /// Create a digitizer plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            session: DigitizerSession {
                active: false,
                touching: false,
                tool: String::new(),
                width: 0.0,
                height: 0.0,
                resolution_x: 0,
                resolution_y: 0,
            },
            forwarded: 0,
            dropped: 0,
        }
    }

    /// The current session.
    #[must_use]
    pub fn session(&self) -> &DigitizerSession {
        &self.session
    }

    /// Samples forwarded to the desktop.
    #[must_use]
    pub const fn forwarded(&self) -> usize {
        self.forwarded
    }

    /// Samples dropped for lack of a session.
    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }
}

#[async_trait]
impl Plugin for DigitizerPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::DIGITIZER
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        let Some(input) = &ctx.desktop.input else {
            return Ok(PluginOutput::new());
        };

        match packet.ptype.as_str() {
            types::DIGITIZER_SESSION => {
                self.session = DigitizerSession::from_body(&packet.body);
                let message = if self.session.can_draw() {
                    format!(
                        "tablet surface active: {} at {}x{}",
                        self.session.tool, self.session.width, self.session.height
                    )
                } else {
                    "tablet surface inactive".to_string()
                };
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message,
                }))
            }
            types::DIGITIZER => {
                if !self.session.can_draw() {
                    self.dropped += 1;
                    return Ok(PluginOutput::new());
                }
                let Some(sample) = Sample::from_body(&packet.body) else {
                    self.dropped += 1;
                    return Ok(PluginOutput::new());
                };
                let (x, y) = sample.normalise(&self.session);
                // Absolute, not relative: a tablet sample names a position on
                // the surface, so a delta would drift away from the stroke.
                if let Err(error) = input.pointer_absolute(x, y, self.session.touching).await {
                    // Logged once per failure rather than returned: one
                    // failed injection must not disable the plugin.
                    self.dropped += 1;
                    return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("pointer injection failed: {error}"),
                    }));
                }
                self.forwarded += 1;
                Ok(PluginOutput::new())
            }
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Builds [`DigitizerPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct DigitizerFactory;

impl PluginFactory for DigitizerFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::DIGITIZER
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(DigitizerPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    async fn send(
        plugin: &mut DigitizerPlugin,
        desktop: &handfast_desktop::Desktop,
        ptype: &str,
        body: Value,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(ptype, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    fn session_body() -> Value {
        serde_json::json!({
            "active": true,
            "touching": false,
            "tool": "pen",
            "width": 1000.0,
            "height": 500.0,
            "resolutionX": 100,
            "resolutionY": 100,
        })
    }

    #[test]
    fn the_roster_entry_is_inbound_only() {
        assert_eq!(roster::DIGITIZER.status, PluginStatus::Implemented);
        assert_eq!(
            roster::DIGITIZER.incoming,
            // Wire order is the sample type first, then the session.
            &[types::DIGITIZER, types::DIGITIZER_SESSION]
        );
        assert!(roster::DIGITIZER.outgoing.is_empty());
    }

    #[tokio::test]
    async fn a_session_carries_the_geometry() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::DIGITIZER_SESSION,
            session_body(),
        )
        .await;

        let session = plugin.session();
        assert!(session.active);
        assert_eq!(session.tool, "pen");
        assert_eq!(session.width, 1000.0);
        assert_eq!(session.height, 500.0);
        assert_eq!(session.resolution_x, 100);
    }

    #[tokio::test]
    async fn an_unknown_tool_falls_back_to_pen() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let mut body = session_body();
        body["tool"] = serde_json::json!("stylus-2030");
        send(&mut plugin, &mock.desktop(), types::DIGITIZER_SESSION, body).await;
        assert_eq!(plugin.session().tool, "pen");
    }

    #[tokio::test]
    async fn a_sample_moves_the_pointer_normalised_to_the_geometry() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::DIGITIZER_SESSION,
            session_body(),
        )
        .await;
        send(
            &mut plugin,
            &mock.desktop(),
            types::DIGITIZER,
            serde_json::json!({ "x": 500.0, "y": 250.0, "pressure": 0.5 }),
        )
        .await;

        assert_eq!(plugin.forwarded(), 1);
        let events = mock.input_events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            handfast_desktop::mock::InputEvent::Absolute { x, y, pressed } => {
                assert!((x - 0.5).abs() < 1e-6);
                assert!((y - 0.5).abs() < 1e-6);
                assert!(!pressed, "not touching, so no press");
            }
            other => panic!("expected an absolute pointer event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn touching_becomes_a_press() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let mut body = session_body();
        body["touching"] = serde_json::json!(true);
        send(&mut plugin, &mock.desktop(), types::DIGITIZER_SESSION, body).await;
        send(
            &mut plugin,
            &mock.desktop(),
            types::DIGITIZER,
            serde_json::json!({ "x": 100.0, "y": 100.0 }),
        )
        .await;

        let events = mock.input_events();
        match &events[0] {
            handfast_desktop::mock::InputEvent::Absolute { pressed, .. } => {
                assert!(pressed, "touching must press");
            }
            other => panic!("expected an absolute pointer event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sample_without_a_session_never_moves_the_pointer() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::DIGITIZER,
            serde_json::json!({ "x": 10.0, "y": 10.0 }),
        )
        .await;

        assert!(mock.input_events().is_empty());
        assert_eq!(plugin.dropped(), 1);
        assert_eq!(plugin.forwarded(), 0);
    }

    #[tokio::test]
    async fn coordinates_outside_the_surface_are_clamped() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::DIGITIZER_SESSION,
            session_body(),
        )
        .await;
        send(
            &mut plugin,
            &mock.desktop(),
            types::DIGITIZER,
            serde_json::json!({ "x": 5000.0, "y": -20.0 }),
        )
        .await;

        match &mock.input_events()[0] {
            handfast_desktop::mock::InputEvent::Absolute { x, y, .. } => {
                assert!((x - 1.0).abs() < 1e-6);
                assert!(y.abs() < 1e-6);
            }
            other => panic!("expected an absolute pointer event, got {other:?}"),
        }
    }

    #[test]
    fn malformed_samples_are_rejected() {
        let missing_y = serde_json::json!({ "y": 1.0 });
        assert!(Sample::from_body(missing_y.as_object().unwrap()).is_none());
        let missing_x = serde_json::json!({ "x": 1.0 });
        assert!(Sample::from_body(missing_x.as_object().unwrap()).is_none());
        let clamped = serde_json::json!({ "x": 1.0, "y": 2.0, "pressure": 9.0 });
        let sample = Sample::from_body(clamped.as_object().unwrap()).unwrap();
        assert!((sample.pressure - 1.0).abs() < 1e-6);
    }
}
