//! Digitizer plugin.
//!
//! The Android app drives a desktop drawing tablet: `kdeconnect.digitizer
//! .session` opens and closes a session in the phone's device pixels, and
//! `kdeconnect.digitizer` carries tool events. The desktop injects them as
//! absolute pen input.
//!
//! Named gap: the Windows input adapter injects absolute position and buttons
//! only. Pen pressure needs the Windows pen injection APIs, which the
//! no-unsafe rule excludes, so pressure is ignored on Windows and never faked.
//! Wayland injects pressure through a uinput virtual tablet.

use std::sync::Arc;

use async_trait::async_trait;
use handfast_desktop::{DesktopError, Input, PenEvent, PenTool};
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{Map, Value};

use crate::roster;

/// Receives drawing-tablet events from a phone and injects absolute pen input.
#[derive(Debug, Default)]
pub struct DigitizerPlugin {
    /// Coordinate space of the active session, if any.
    session: Option<Session>,
}

/// The device coordinate space a session start announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Session {
    width: i32,
    height: i32,
}

impl DigitizerPlugin {
    /// Create a digitizer plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { session: None }
    }
}

/// What one `kdeconnect.digitizer.session` packet asks for.
enum Action {
    Start {
        width: i32,
        height: i32,
        resolution_x: i32,
        resolution_y: i32,
    },
    End,
}

fn warn(message: &str) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.to_string(),
    })
}

fn bool_field(body: &Map<String, Value>, field: &str) -> Result<Option<bool>, &'static str> {
    match body.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or("digitizer flag is not a boolean"),
    }
}

fn integer_field(body: &Map<String, Value>, field: &str) -> Result<Option<i32>, &'static str> {
    match body.get(field) {
        None => Ok(None),
        Some(value) => {
            let parsed = value.as_i64().ok_or("digitizer field is not an integer")?;
            i32::try_from(parsed)
                .map(Some)
                .map_err(|_| "digitizer field is out of range")
        }
    }
}

fn required_integer(body: &Map<String, Value>, field: &str) -> Result<i32, &'static str> {
    integer_field(body, field)?.ok_or("digitizer session packet is missing a field")
}

fn pressure_field(body: &Map<String, Value>) -> Result<Option<f64>, &'static str> {
    match body.get("pressure") {
        None => Ok(None),
        Some(value) => {
            let pressure = value.as_f64().ok_or("digitizer pressure is not a number")?;
            if !pressure.is_finite() {
                return Err("digitizer pressure is not finite");
            }
            if !(0.0..=1.0).contains(&pressure) {
                return Err("digitizer pressure is out of range");
            }
            Ok(Some(pressure))
        }
    }
}

fn tool_field(body: &Map<String, Value>) -> Result<Option<PenTool>, &'static str> {
    match body.get("tool") {
        None => Ok(None),
        // The exact match also rejects strings with control characters.
        Some(Value::String(tool)) => match tool.as_str() {
            "Pen" => Ok(Some(PenTool::Pen)),
            "Rubber" => Ok(Some(PenTool::Rubber)),
            _ => Err("digitizer tool is not Pen or Rubber"),
        },
        Some(_) => Err("digitizer tool is not a string"),
    }
}

fn parse_session(packet: &Packet) -> Result<Action, &'static str> {
    let body = &packet.body;
    match body.get("action") {
        Some(Value::String(action)) => match action.as_str() {
            "start" => {
                let width = required_integer(body, "width")?;
                let height = required_integer(body, "height")?;
                let resolution_x = required_integer(body, "resolutionX")?;
                let resolution_y = required_integer(body, "resolutionY")?;
                if width < 1 || height < 1 {
                    return Err("digitizer session dimensions are out of range");
                }
                Ok(Action::Start {
                    width,
                    height,
                    resolution_x,
                    resolution_y,
                })
            }
            "end" => Ok(Action::End),
            _ => Err("digitizer session action is not start or end"),
        },
        Some(_) => Err("digitizer action is not a string"),
        None => Err("digitizer session packet has no action"),
    }
}

/// Parse one tool event against the active session.
///
/// Position is clamped into the session space. A `active: false` packet is a
/// lift: touching and pressure are forced off even if the packet says
/// otherwise.
fn parse_event(packet: &Packet, session: &Session) -> Result<PenEvent, &'static str> {
    let body = &packet.body;
    let mut touching = bool_field(body, "touching")?.unwrap_or(false);
    let mut pressure = pressure_field(body)?.unwrap_or(0.0);
    let tool = tool_field(body)?.unwrap_or(PenTool::Pen);
    let x = integer_field(body, "x")?
        .unwrap_or(0)
        .clamp(0, session.width - 1);
    let y = integer_field(body, "y")?
        .unwrap_or(0)
        .clamp(0, session.height - 1);
    if bool_field(body, "active")? == Some(false) {
        touching = false;
        pressure = 0.0;
    }
    Ok(PenEvent {
        x,
        y,
        pressure,
        touching,
        tool,
    })
}

async fn start_session(
    session: &mut Option<Session>,
    input: &Arc<dyn Input>,
    width: i32,
    height: i32,
    resolution_x: i32,
    resolution_y: i32,
) -> Result<PluginOutput, PluginError> {
    match input
        .start_pen_session(width, height, resolution_x, resolution_y)
        .await
    {
        Ok(()) => {
            *session = Some(Session { width, height });
            Ok(PluginOutput::new())
        }
        Err(DesktopError::Unsupported(message)) => Ok(warn(&format!(
            "input adapter cannot start a pen session: {message}"
        ))),
        Err(error) => Err(PluginError::Backend(error.to_string())),
    }
}

async fn end_session(
    session: &mut Option<Session>,
    input: &Arc<dyn Input>,
) -> Result<PluginOutput, PluginError> {
    *session = None;
    match input.end_pen_session().await {
        Ok(()) => Ok(PluginOutput::new()),
        Err(DesktopError::Unsupported(message)) => Ok(warn(&format!(
            "input adapter cannot end the pen session: {message}"
        ))),
        Err(error) => Err(PluginError::Backend(error.to_string())),
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
        if packet.ptype == types::DIGITIZER_SESSION {
            let action = match parse_session(packet) {
                Ok(action) => action,
                Err(message) => return Ok(warn(message)),
            };
            let Some(input) = ctx.desktop.input.as_ref() else {
                return Ok(warn("input adapter unavailable"));
            };
            return match action {
                Action::Start {
                    width,
                    height,
                    resolution_x,
                    resolution_y,
                } => {
                    start_session(
                        &mut self.session,
                        input,
                        width,
                        height,
                        resolution_x,
                        resolution_y,
                    )
                    .await
                }
                Action::End => end_session(&mut self.session, input).await,
            };
        }
        if packet.ptype == types::DIGITIZER {
            let Some(session) = self.session else {
                return Ok(warn("pen event without an active session"));
            };
            let event = match parse_event(packet, &session) {
                Ok(event) => event,
                Err(message) => return Ok(warn(message)),
            };
            let Some(input) = ctx.desktop.input.as_ref() else {
                return Ok(warn("input adapter unavailable"));
            };
            return match input.pen(event).await {
                Ok(()) => Ok(PluginOutput::new()),
                Err(DesktopError::Unsupported(message)) => Ok(warn(&format!(
                    "input adapter cannot inject pen events: {message}"
                ))),
                Err(error) => Err(PluginError::Backend(error.to_string())),
            };
        }
        Ok(PluginOutput::new())
    }

    async fn on_disconnect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        if self.session.take().is_some() {
            if let Some(input) = ctx.desktop.input.as_ref() {
                // Best effort: the desktop drops the tablet with the adapter.
                if let Err(error) = input.end_pen_session().await {
                    return Ok(warn(&format!(
                        "input adapter cannot end the pen session: {error}"
                    )));
                }
            }
        }
        Ok(PluginOutput::new())
    }
}

/// Builds digitizer plugin instances.
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
    use handfast_desktop::mock::InputEvent;
    use handfast_desktop::{Desktop, MockDesktop, PointerButton};
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;
    use serde_json::json;

    use crate::roster::DIGITIZER;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn handle(
        plugin: &mut DigitizerPlugin,
        desktop: &Desktop,
        packet: Packet,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    fn session_start(body: Value) -> Packet {
        Packet::from_body(types::DIGITIZER_SESSION, &body).unwrap()
    }

    fn tool_event(body: Value) -> Packet {
        Packet::from_body(types::DIGITIZER, &body).unwrap()
    }

    fn start() -> Packet {
        session_start(json!({
            "action": "start",
            "width": 1440,
            "height": 3200,
            "resolutionX": 6,
            "resolutionY": 6,
        }))
    }

    fn pen_start(width: i32, height: i32, resolution: i32) -> InputEvent {
        InputEvent::PenStart {
            width,
            height,
            resolution_x: resolution,
            resolution_y: resolution,
        }
    }

    fn is_warning(output: &PluginOutput) -> bool {
        matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        )
    }

    #[test]
    fn session_start_parses_every_field() {
        let action = parse_session(&start()).unwrap();
        assert_eq!(
            match action {
                Action::Start {
                    width,
                    height,
                    resolution_x,
                    resolution_y,
                } => (width, height, resolution_x, resolution_y),
                Action::End => panic!("the start packet parsed as end"),
            },
            (1440, 3200, 6, 6)
        );
    }

    #[test]
    fn session_end_parses_without_fields() {
        let packet = session_start(json!({"action": "end"}));
        assert!(matches!(parse_session(&packet).unwrap(), Action::End));
    }

    #[test]
    fn event_parses_every_optional_field() {
        let session = Session {
            width: 1440,
            height: 3200,
        };
        let packet = tool_event(json!({
            "active": true,
            "touching": true,
            "tool": "Rubber",
            "x": 100,
            "y": 200,
            "pressure": 0.75,
        }));
        assert_eq!(
            parse_event(&packet, &session).unwrap(),
            PenEvent {
                x: 100,
                y: 200,
                pressure: 0.75,
                touching: true,
                tool: PenTool::Rubber,
            }
        );

        // Missing fields fall back to a hovering pen at the origin.
        let packet = tool_event(json!({}));
        assert_eq!(
            parse_event(&packet, &session).unwrap(),
            PenEvent {
                x: 0,
                y: 0,
                pressure: 0.0,
                touching: false,
                tool: PenTool::Pen,
            }
        );
    }

    #[test]
    fn event_position_is_clamped_into_the_session() {
        let session = Session {
            width: 1440,
            height: 3200,
        };
        let packet = tool_event(json!({"x": -5, "y": 9999}));
        assert_eq!(
            parse_event(&packet, &session).unwrap(),
            PenEvent {
                x: 0,
                y: 3199,
                pressure: 0.0,
                touching: false,
                tool: PenTool::Pen,
            }
        );
    }

    #[test]
    fn active_false_forces_a_lift() {
        let session = Session {
            width: 1440,
            height: 3200,
        };
        let packet = tool_event(json!({
            "active": false,
            "touching": true,
            "pressure": 0.9,
            "x": 7,
            "y": 8,
        }));
        assert_eq!(
            parse_event(&packet, &session).unwrap(),
            PenEvent {
                x: 7,
                y: 8,
                pressure: 0.0,
                touching: false,
                tool: PenTool::Pen,
            }
        );
    }

    #[test]
    fn malformed_session_packets_are_rejected() {
        for body in [
            json!({}),
            json!({"action": "open"}),
            json!({"action": 3}),
            json!({"action": "start"}),
            json!({"action": "start", "width": 0, "height": 100, "resolutionX": 1, "resolutionY": 1}),
            json!({"action": "start", "width": -10, "height": 100, "resolutionX": 1, "resolutionY": 1}),
            json!({"action": "start", "width": "big", "height": 100, "resolutionX": 1, "resolutionY": 1}),
            json!({"action": "start", "width": 100, "height": 100, "resolutionX": 1}),
            json!({"action": "start", "width": 100, "height": 100, "resolutionX": true, "resolutionY": 1}),
        ] {
            let packet = session_start(body);
            assert!(parse_session(&packet).is_err(), "packet parsed: {packet:?}");
        }
    }

    #[test]
    fn malformed_event_fields_are_rejected() {
        let session = Session {
            width: 1440,
            height: 3200,
        };
        for body in [
            json!({"pressure": 1.5}),
            json!({"pressure": -0.1}),
            json!({"pressure": "hard"}),
            json!({"touching": 1}),
            json!({"active": "yes"}),
            json!({"tool": "pencil"}),
            json!({"tool": 3}),
            json!({"x": "left"}),
            json!({"y": 1.5}),
            json!({"x": 5, "y": 6, "pressure": 2.0}),
        ] {
            let packet = tool_event(body);
            assert!(
                parse_event(&packet, &session).is_err(),
                "packet parsed: {packet:?}"
            );
        }
    }

    #[tokio::test]
    async fn session_lifecycle_is_injected_in_order() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let desktop = mock.desktop();

        handle(&mut plugin, &desktop, start()).await;
        handle(
            &mut plugin,
            &desktop,
            tool_event(json!({"x": 10, "y": 20, "pressure": 0.5, "touching": true})),
        )
        .await;
        handle(
            &mut plugin,
            &desktop,
            session_start(json!({"action": "end"})),
        )
        .await;

        assert_eq!(
            mock.input_events(),
            vec![
                pen_start(1440, 3200, 6),
                InputEvent::Pen(PenEvent {
                    x: 10,
                    y: 20,
                    pressure: 0.5,
                    touching: true,
                    tool: PenTool::Pen,
                }),
                InputEvent::PenEnd,
            ]
        );
    }

    #[tokio::test]
    async fn tool_events_are_clamped_to_the_session() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let desktop = mock.desktop();

        handle(&mut plugin, &desktop, start()).await;
        handle(
            &mut plugin,
            &desktop,
            tool_event(
                json!({"x": 5000, "y": -1, "touching": true, "tool": "Rubber", "pressure": 1.0}),
            ),
        )
        .await;

        assert_eq!(
            mock.input_events()[1],
            InputEvent::Pen(PenEvent {
                x: 1439,
                y: 0,
                pressure: 1.0,
                touching: true,
                tool: PenTool::Rubber,
            })
        );
    }

    #[tokio::test]
    async fn a_new_session_replaces_the_previous_one() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let desktop = mock.desktop();

        handle(&mut plugin, &desktop, start()).await;
        handle(
            &mut plugin,
            &desktop,
            session_start(json!({
                "action": "start",
                "width": 100,
                "height": 100,
                "resolutionX": 1,
                "resolutionY": 1,
            })),
        )
        .await;
        handle(&mut plugin, &desktop, tool_event(json!({"x": 50, "y": 50}))).await;

        assert_eq!(
            mock.input_events()[..2],
            [pen_start(1440, 3200, 6), pen_start(100, 100, 1)]
        );
        assert_eq!(
            mock.input_events()[2],
            InputEvent::Pen(PenEvent {
                x: 50,
                y: 50,
                pressure: 0.0,
                touching: false,
                tool: PenTool::Pen,
            })
        );
    }

    #[tokio::test]
    async fn disconnect_ends_the_session() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let desktop = mock.desktop();

        handle(&mut plugin, &desktop, start()).await;
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_disconnect(&mut ctx).await.unwrap();

        assert!(mock
            .input_events()
            .last()
            .is_some_and(|event| *event == InputEvent::PenEnd));
    }

    #[tokio::test]
    async fn events_without_a_session_are_a_warning() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let desktop = mock.desktop();

        let output = handle(&mut plugin, &desktop, tool_event(json!({"x": 1, "y": 1}))).await;
        assert!(mock.input_events().is_empty());
        assert!(is_warning(&output));
    }

    #[tokio::test]
    async fn missing_input_adapter_is_a_warning() {
        let desktop = Desktop::default();
        let mut plugin = DigitizerPlugin::new();

        let output = handle(&mut plugin, &desktop, start()).await;
        assert!(is_warning(&output));

        // Without a started session the plugin state stays empty.
        let output = handle(&mut plugin, &desktop, tool_event(json!({"x": 1, "y": 1}))).await;
        assert!(is_warning(&output));
    }

    /// An input adapter without digitizer support; the pen methods fall back
    /// to the trait defaults, which report `Unsupported`.
    struct FailingInput;

    #[async_trait]
    impl Input for FailingInput {
        async fn pointer(
            &self,
            _event: handfast_desktop::PointerEvent,
        ) -> Result<(), DesktopError> {
            Ok(())
        }

        async fn button(
            &self,
            _button: PointerButton,
            _action: handfast_desktop::ButtonAction,
        ) -> Result<(), DesktopError> {
            Ok(())
        }

        async fn key(&self, _event: handfast_desktop::KeyEvent) -> Result<(), DesktopError> {
            Ok(())
        }
    }

    fn failing_desktop() -> Desktop {
        Desktop {
            input: Some(Arc::new(FailingInput)),
            ..Desktop::default()
        }
    }

    #[tokio::test]
    async fn unsupported_adapter_is_a_warning() {
        let desktop = failing_desktop();
        let mut plugin = DigitizerPlugin::new();

        let output = handle(&mut plugin, &desktop, start()).await;
        assert!(is_warning(&output));

        // The session was not opened, so a following event warns too.
        let output = handle(&mut plugin, &desktop, tool_event(json!({"x": 1, "y": 1}))).await;
        assert!(is_warning(&output));
    }

    #[tokio::test]
    async fn unsupported_end_is_a_warning() {
        let desktop = failing_desktop();
        let mut plugin = DigitizerPlugin::new();

        let output = handle(
            &mut plugin,
            &desktop,
            session_start(json!({"action": "end"})),
        )
        .await;
        assert!(is_warning(&output));
    }

    #[tokio::test]
    async fn foreign_packets_are_ignored() {
        let mock = MockDesktop::new();
        let mut plugin = DigitizerPlugin::new();
        let desktop = mock.desktop();

        let output = handle(&mut plugin, &desktop, Packet::empty(types::PING).unwrap()).await;
        assert!(mock.input_events().is_empty());
        assert!(output.events.is_empty());
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(DigitizerPlugin::new().meta(), &DIGITIZER);
        assert_eq!(DigitizerFactory.meta(), &DIGITIZER);
    }
}
