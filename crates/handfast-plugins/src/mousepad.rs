//! Mousepad plugin.

use async_trait::async_trait;
use handfast_desktop::{KeyEvent, Modifiers, PointerEvent};
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

const SPECIAL_KEY_MAX: u8 = 32;
const CLICK_FIELDS: [&str; 6] = [
    "singleclick",
    "doubleclick",
    "middleclick",
    "rightclick",
    "singlehold",
    "singlerelease",
];

/// Injects remote pointer and keyboard input from a peer.
#[derive(Debug, Default)]
pub struct MousepadPlugin;

impl MousepadPlugin {
    /// Create a mousepad plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

enum Action {
    Pointer(PointerEvent),
    Key { event: KeyEvent, ack: bool },
    Button(&'static str),
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
            .ok_or("mousepad flag is not a boolean"),
    }
}

fn parse_modifiers(body: &Map<String, Value>) -> Result<Modifiers, &'static str> {
    Ok(Modifiers {
        alt: bool_field(body, "alt")?.unwrap_or(false),
        ctrl: bool_field(body, "ctrl")?.unwrap_or(false),
        shift: bool_field(body, "shift")?.unwrap_or(false),
        sup: bool_field(body, "super")?.unwrap_or(false),
    })
}

fn number_field(body: &Map<String, Value>, field: &str) -> Result<Option<f64>, &'static str> {
    match body.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Some)
            .ok_or("mousepad delta is not a number"),
    }
}

fn special_key(body: &Map<String, Value>) -> Result<Option<u8>, &'static str> {
    let Some(value) = body.get("specialKey") else {
        return Ok(None);
    };
    let code = value
        .as_i64()
        .ok_or("mousepad specialKey is not an integer")?;
    if code == 0 {
        return Ok(None);
    }
    let code = u8::try_from(code).map_err(|_| "mousepad specialKey is out of range")?;
    if code > SPECIAL_KEY_MAX {
        return Err("mousepad specialKey is out of range");
    }
    Ok(Some(code))
}

fn parse(packet: &Packet) -> Result<Action, &'static str> {
    let body = &packet.body;
    let scroll = bool_field(body, "scroll")?.unwrap_or(false);
    let modifiers = parse_modifiers(body)?;

    for field in CLICK_FIELDS {
        if bool_field(body, field)? == Some(true) {
            return Ok(Action::Button(field));
        }
    }

    if scroll {
        return Ok(Action::Pointer(PointerEvent {
            dx: number_field(body, "dx")?.unwrap_or(0.0),
            dy: number_field(body, "dy")?.unwrap_or(0.0),
            scroll: true,
        }));
    }

    let special = special_key(body)?;
    let key = match body.get("key") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or("mousepad key is not a string")?
                .to_string(),
        ),
    };
    let ack = bool_field(body, "sendAck")?.unwrap_or(false);

    if let Some(code) = special {
        return Ok(Action::Key {
            event: KeyEvent::Special { code, modifiers },
            ack,
        });
    }
    if let Some(text) = key.filter(|text| !text.is_empty()) {
        return Ok(Action::Key {
            event: KeyEvent::Text { text, modifiers },
            ack,
        });
    }

    let dx = number_field(body, "dx")?;
    let dy = number_field(body, "dy")?;
    match (dx, dy) {
        (Some(dx), Some(dy)) => Ok(Action::Pointer(PointerEvent {
            dx,
            dy,
            scroll: false,
        })),
        _ => Err("mousepad packet has no action"),
    }
}

fn echo_packet(event: &KeyEvent) -> Result<Packet, PluginError> {
    let mut body = Map::new();
    body.insert("isAck".to_string(), Value::Bool(true));
    let modifiers = match event {
        KeyEvent::Text { text, modifiers } => {
            body.insert("key".to_string(), Value::String(text.clone()));
            body.insert("specialKey".to_string(), json!(0));
            modifiers
        }
        KeyEvent::Special { code, modifiers } => {
            body.insert("key".to_string(), Value::String(String::new()));
            body.insert("specialKey".to_string(), json!(code));
            modifiers
        }
    };
    body.insert("alt".to_string(), Value::Bool(modifiers.alt));
    body.insert("ctrl".to_string(), Value::Bool(modifiers.ctrl));
    body.insert("shift".to_string(), Value::Bool(modifiers.shift));
    body.insert("super".to_string(), Value::Bool(modifiers.sup));
    Packet::new(types::MOUSEPAD_ECHO, Value::Object(body))
        .map_err(|error| PluginError::InvalidPacket(error.to_string()))
}

#[async_trait]
impl Plugin for MousepadPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MOUSEPAD
    }

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        if ctx.desktop.input.is_none() {
            return Ok(warn("input adapter unavailable"));
        }
        let packet = Packet::new(types::MOUSEPAD_KEYBOARDSTATE, json!({ "state": true }))
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::MOUSEPAD_REQUEST {
            return Ok(PluginOutput::new());
        }
        let action = match parse(packet) {
            Ok(action) => action,
            Err(message) => return Ok(warn(message)),
        };
        let Some(input) = ctx.desktop.input.as_ref() else {
            return Ok(warn("input adapter unavailable"));
        };
        match action {
            Action::Pointer(event) => {
                input
                    .pointer(event)
                    .await
                    .map_err(|error| PluginError::Backend(error.to_string()))?;
                Ok(PluginOutput::new())
            }
            Action::Key { event, ack } => {
                let echo = if ack {
                    Some(echo_packet(&event)?)
                } else {
                    None
                };
                input
                    .key(event)
                    .await
                    .map_err(|error| PluginError::Backend(error.to_string()))?;
                Ok(echo.map_or_else(PluginOutput::new, |packet| {
                    PluginOutput::new().with_packet(packet)
                }))
            }
            Action::Button(name) => Ok(warn(&format!(
                "input adapter cannot inject pointer button: {name}"
            ))),
        }
    }
}

/// Builds mousepad plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct MousepadFactory;

impl PluginFactory for MousepadFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::MOUSEPAD
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(MousepadPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::mock::InputEvent;
    use handfast_desktop::{Desktop, MockDesktop};
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
        MousepadPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap()
    }

    async fn connect(desktop: &Desktop) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        MousepadPlugin::new().on_connect(&mut ctx).await.unwrap()
    }

    fn request(body: Value) -> Packet {
        Packet::from_body(types::MOUSEPAD_REQUEST, &body).unwrap()
    }

    #[tokio::test]
    async fn movement_is_injected() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), request(json!({"dx": 3.0, "dy": -2.0}))).await;
        assert_eq!(
            mock.input_events(),
            vec![InputEvent::Pointer(PointerEvent {
                dx: 3.0,
                dy: -2.0,
                scroll: false,
            })]
        );
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn scroll_is_injected() {
        let mock = MockDesktop::new();
        run(
            &mock.desktop(),
            request(json!({"scroll": true, "dx": 0.0, "dy": -1.5})),
        )
        .await;
        assert_eq!(
            mock.input_events(),
            vec![InputEvent::Pointer(PointerEvent {
                dx: 0.0,
                dy: -1.5,
                scroll: true,
            })]
        );
    }

    #[tokio::test]
    async fn text_key_with_modifiers_is_injected() {
        let mock = MockDesktop::new();
        run(
            &mock.desktop(),
            request(json!({"key": "c", "ctrl": true, "shift": true})),
        )
        .await;
        assert_eq!(
            mock.input_events(),
            vec![InputEvent::Key(KeyEvent::Text {
                text: "c".to_string(),
                modifiers: Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Modifiers::default()
                },
            })]
        );
    }

    #[tokio::test]
    async fn special_key_with_modifiers_is_injected() {
        let mock = MockDesktop::new();
        run(
            &mock.desktop(),
            request(json!({"specialKey": 12, "alt": true})),
        )
        .await;
        assert_eq!(
            mock.input_events(),
            vec![InputEvent::Key(KeyEvent::Special {
                code: 12,
                modifiers: Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            })]
        );
    }

    #[tokio::test]
    async fn special_key_out_of_range_is_a_warning() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), request(json!({"specialKey": 33}))).await;
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
        let output = run(&mock.desktop(), request(json!({"dx": "left", "dy": 1.0}))).await;
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
    async fn malformed_modifier_is_a_warning() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), request(json!({"key": "a", "ctrl": 1}))).await;
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
    async fn pointer_button_is_a_warning() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), request(json!({"singleclick": true}))).await;
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
    async fn send_ack_echoes_the_key() {
        let mock = MockDesktop::new();
        let output = run(
            &mock.desktop(),
            request(json!({"key": "a", "sendAck": true})),
        )
        .await;
        assert_eq!(
            output.packets,
            vec![Packet::from_body(
                types::MOUSEPAD_ECHO,
                &json!({
                    "isAck": true,
                    "key": "a",
                    "specialKey": 0,
                    "alt": false,
                    "ctrl": false,
                    "shift": false,
                    "super": false,
                }),
            )
            .unwrap()]
        );
    }

    #[tokio::test]
    async fn connect_reports_keyboard_support() {
        let mock = MockDesktop::new();
        let output = connect(&mock.desktop()).await;
        assert_eq!(
            output.packets,
            vec![
                Packet::from_body(types::MOUSEPAD_KEYBOARDSTATE, &json!({"state": true}),).unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn connect_without_adapter_is_a_warning() {
        let output = connect(&Desktop::default()).await;
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
    async fn missing_input_adapter_is_a_warning() {
        let output = run(&Desktop::default(), request(json!({"dx": 1.0, "dy": 1.0}))).await;
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
        let output = run(&mock.desktop(), Packet::empty(types::PING).unwrap()).await;
        assert!(mock.input_events().is_empty());
        assert!(output.events.is_empty());
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(MousepadPlugin::new().meta(), &roster::MOUSEPAD);
        assert_eq!(MousepadFactory.meta(), &roster::MOUSEPAD);
    }
}
