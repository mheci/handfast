//! Remote input: drive the peer's pointer and keyboard from this desktop.
//!
//! Two plugins share this module because they share one packet:
//! `kdeconnect.mousepad.request` carries both mouse and keyboard events, and
//! the receiver decides by presence of `key` or `specialKey`. `remotecontrol`
//! (upstream "Send remote mouse input") and `remotekeyboard` (upstream "Send
//! remote keyboard input") are two halves of the same wire format, the mirror
//! of [`crate::mousepad`], which injects what a phone sends us.
//!
//! A pointer event is `dx`/`dy`, or one of the click and hold flags. A key
//! event is `key` (text, possibly multi-character) or `specialKey` (1-32, the
//! upstream table), plus the four modifier booleans.
//!
//! `sendAck` is the readiness probe: the receiver echoes a handled key event
//! back as `kdeconnect.mousepad.echo` with `isAck: true`, and
//! [`RemoteKeyboardPlugin`] records the player as ready. `remotecontrol` never
//! sets it, because the Android mouse receiver does not answer acks.

use async_trait::async_trait;
use handfast_desktop::{Modifiers, PointerAction, PointerButton, PointerEvent};
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Largest accepted special-key code, matching the upstream table.
pub const SPECIAL_KEY_MAX: u8 = 32;

/// Longest accepted text payload, in bytes.
///
/// The README allows multi-character strings "for performance reasons", so this
/// is far larger than one keystroke but still bounded.
const MAX_TEXT_BYTES: usize = 1024;

/// Why an outbound input event was rejected before it queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteInputError {
    /// The packet body could not be built into a `kdeconnect.mousepad.request`.
    ///
    /// Unreachable for the bodies this module builds, since every one is a JSON
    /// object of scalars, but it keeps the builders honest instead of unwrapping.
    Packet,
    /// The delta was not finite, or both deltas were zero.
    InvalidDelta,
    /// The special-key code was above [`SPECIAL_KEY_MAX`].
    InvalidSpecialKey,
    /// The text was empty or longer than 1024 bytes.
    InvalidText,
}

impl std::fmt::Display for RemoteInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDelta => f.write_str("pointer delta out of range"),
            Self::InvalidSpecialKey => f.write_str("special key out of range"),
            Self::InvalidText => f.write_str("key text out of range"),
            Self::Packet => f.write_str("packet body could not be built"),
        }
    }
}

impl std::error::Error for RemoteInputError {}

/// Add the four modifier booleans to a packet body, as the desktop client does.
fn insert_modifiers(body: &mut Map<String, Value>, modifiers: Modifiers) {
    body.insert("shift".to_string(), Value::Bool(modifiers.shift));
    body.insert("ctrl".to_string(), Value::Bool(modifiers.ctrl));
    body.insert("alt".to_string(), Value::Bool(modifiers.alt));
    body.insert("super".to_string(), Value::Bool(modifiers.sup));
}

fn wrap(body: Map<String, Value>) -> Result<Packet, RemoteInputError> {
    Packet::new(types::MOUSEPAD_REQUEST, Value::Object(body))
        .map_err(|_| RemoteInputError::Packet)
}

/// Build the request for a relative pointer movement.
///
/// # Errors
///
/// Fails for a non-finite delta or a movement of nothing at all.
pub fn movement_packet(dx: f64, dy: f64) -> Result<Packet, RemoteInputError> {
    if !dx.is_finite() || !dy.is_finite() {
        return Err(RemoteInputError::InvalidDelta);
    }
    if dx == 0.0 && dy == 0.0 {
        return Err(RemoteInputError::InvalidDelta);
    }
    wrap(Map::from_iter([
        ("dx".to_string(), json!(dx)),
        ("dy".to_string(), json!(dy)),
    ]))
}

/// Build the request for one pointer event, as the mousepad receiver reads it.
///
/// # Errors
///
/// Fails for a movement or scroll event with an unusable delta.
pub fn pointer_packet(event: &PointerEvent) -> Result<Packet, RemoteInputError> {
    match event.action {
        PointerAction::Move => movement_packet(event.dx, event.dy),
        PointerAction::Scroll => {
            if !event.dx.is_finite() || !event.dy.is_finite() {
                return Err(RemoteInputError::InvalidDelta);
            }
            let mut body = Map::new();
            body.insert("scroll".to_string(), Value::Bool(true));
            body.insert("dx".to_string(), json!(event.dx));
            body.insert("dy".to_string(), json!(event.dy));
            wrap(body)
        }
        PointerAction::Click { button, count } => {
            let field = match (button, count) {
                (PointerButton::Left, 1) => "singleclick",
                (PointerButton::Left, _) => "doubleclick",
                (PointerButton::Middle, _) => "middleclick",
                (PointerButton::Right, _) => "rightclick",
            };
            let mut body = Map::new();
            body.insert(field.to_string(), Value::Bool(true));
            wrap(body)
        }
        PointerAction::Press => {
            let mut body = Map::new();
            body.insert("singlehold".to_string(), Value::Bool(true));
            wrap(body)
        }
        PointerAction::Release => {
            let mut body = Map::new();
            body.insert("singlerelease".to_string(), Value::Bool(true));
            wrap(body)
        }
    }
}

/// Build a key request.
///
/// `text` is sent as `key` and `code` as `specialKey`; the receiver ignores
/// `key` when `specialKey` is in range, which matches the upstream README.
/// `ack` sets `sendAck` so the receiver echoes the event once it handled it.
///
/// # Errors
///
/// Fails when both a code and text are absent, the code is out of range, or the
/// text is empty or oversized.
pub fn key_packet(
    text: Option<&str>,
    code: Option<u8>,
    modifiers: Modifiers,
    ack: bool,
) -> Result<Packet, RemoteInputError> {
    if let Some(code) = code {
        if code == 0 || code > SPECIAL_KEY_MAX {
            return Err(RemoteInputError::InvalidSpecialKey);
        }
    }
    if let Some(text) = text {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(RemoteInputError::InvalidText);
        }
    }
    if code.is_none() && text.is_none() {
        return Err(RemoteInputError::InvalidText);
    }
    let mut body = Map::new();
    if let Some(text) = text {
        body.insert("key".to_string(), Value::String(text.to_string()));
    }
    if let Some(code) = code {
        body.insert("specialKey".to_string(), json!(code));
    }
    insert_modifiers(&mut body, modifiers);
    if ack {
        body.insert("sendAck".to_string(), Value::Bool(true));
    }
    wrap(body)
}

fn warn(message: String) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message,
    })
}

/// Sends pointer events to the peer: upstream `remotecontrol`.
///
/// The desktop client exposes this as `moveCursor(dx, dy)` and an escape hatch
/// that forwards an arbitrary body. Handfast keeps the typed surface and drops
/// the escape hatch, because a raw body would bypass every bound above.
#[derive(Debug, Default)]
pub struct RemoteControlPlugin {
    /// Requests queued for the next tick.
    pending: Vec<Packet>,
}

impl RemoteControlPlugin {
    /// Create a remote control plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Queue a relative pointer movement.
    ///
    /// # Errors
    ///
    /// Fails for a non-finite delta or a movement of nothing.
    pub fn move_cursor(&mut self, dx: f64, dy: f64) -> Result<(), RemoteInputError> {
        self.pending.push(movement_packet(dx, dy)?);
        Ok(())
    }

    /// Queue any pointer event: movement, scroll, click, press or release.
    ///
    /// # Errors
    ///
    /// Fails when the event has an unusable delta.
    pub fn send_pointer(&mut self, event: &PointerEvent) -> Result<(), RemoteInputError> {
        self.pending.push(pointer_packet(event)?);
        Ok(())
    }

    /// Number of queued requests.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[async_trait]
impl Plugin for RemoteControlPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECONTROL
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        output.packets = std::mem::take(&mut self.pending);
        Ok(output)
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        _packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        // The mouse direction is outbound only; the roster advertises no
        // incoming type, so a peer sending one is confused rather than hostile.
        Ok(PluginOutput::new())
    }
}

/// Builds [`RemoteControlPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteControlFactory;

impl PluginFactory for RemoteControlFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECONTROL
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteControlPlugin::new())
    }
}

/// Sends key events to the peer: upstream `remotekeyboard`.
///
/// Also tracks readiness. A key event sent with `sendAck: true` is answered
/// with `kdeconnect.mousepad.echo {"isAck": true}`, and
/// `kdeconnect.mousepad.keyboardstate {"state": bool}` reports whether the
/// peer's keyboard is up at all. Until either arrives, [`Self::ready`] is
/// false and a UI can show the send button as disabled instead of dropping
/// keystrokes silently.
#[derive(Debug, Default)]
pub struct RemoteKeyboardPlugin {
    /// Requests queued for the next tick.
    pending: Vec<Packet>,
    /// Whether the peer has confirmed it can receive key events.
    ready: bool,
    /// Whether the peer's keyboard service reported itself available.
    keyboard_state: Option<bool>,
}

impl RemoteKeyboardPlugin {
    /// Create a remote keyboard plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            ready: false,
            keyboard_state: None,
        }
    }

    /// Queue typed text, with modifiers held.
    ///
    /// # Errors
    ///
    /// Fails when the text is empty or oversized.
    pub fn type_text(&mut self, text: &str, modifiers: Modifiers) -> Result<(), RemoteInputError> {
        self.pending.push(key_packet(Some(text), None, modifiers, true)?);
        Ok(())
    }

    /// Queue a special key by KDE Connect code, with modifiers held.
    ///
    /// # Errors
    ///
    /// Fails when the code is zero or above [`SPECIAL_KEY_MAX`].
    pub fn press_special(
        &mut self,
        code: u8,
        modifiers: Modifiers,
    ) -> Result<(), RemoteInputError> {
        self.pending
            .push(key_packet(None, Some(code), modifiers, true)?);
        Ok(())
    }

    /// Whether the peer confirmed it can receive key events.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.ready
    }

    /// The last keyboard state the peer reported, if any.
    #[must_use]
    pub const fn keyboard_state(&self) -> Option<bool> {
        self.keyboard_state
    }

    /// Number of queued requests.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[async_trait]
impl Plugin for RemoteKeyboardPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTEKEYBOARD
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        output.packets = std::mem::take(&mut self.pending);
        Ok(output)
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::MOUSEPAD_ECHO => {
                // Upstream requires both fields before it trusts an echo.
                if packet.body.get("isAck").and_then(Value::as_bool) != Some(true) {
                    return Ok(warn("keyboard echo without isAck ignored".to_string()));
                }
                if !packet.body.contains_key("key") {
                    return Ok(warn("keyboard echo without key ignored".to_string()));
                }
                self.ready = true;
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: "peer confirmed it can receive key events".to_string(),
                }))
            }
            types::MOUSEPAD_KEYBOARDSTATE => {
                let Some(state) = packet.body.get("state").and_then(Value::as_bool) else {
                    return Ok(warn("keyboard state without a boolean ignored".to_string()));
                };
                if self.keyboard_state == Some(state) {
                    return Ok(PluginOutput::new());
                }
                self.keyboard_state = Some(state);
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: format!("peer keyboard available: {state}"),
                }))
            }
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Builds [`RemoteKeyboardPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteKeyboardFactory;

impl PluginFactory for RemoteKeyboardFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTEKEYBOARD
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteKeyboardPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    fn modifiers(ctrl: bool) -> Modifiers {
        Modifiers {
            alt: false,
            ctrl,
            shift: false,
            sup: false,
        }
    }

    async fn tick<P: Plugin>(plugin: &mut P) -> PluginOutput {
        let desktop = handfast_desktop::MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    async fn feed<P: Plugin>(plugin: &mut P, packet: &Packet) -> PluginOutput {
        let desktop = handfast_desktop::MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    #[test]
    fn the_roster_entries_are_implemented_and_match_the_wire() {
        assert_eq!(roster::REMOTECONTROL.status, PluginStatus::Implemented);
        assert_eq!(roster::REMOTECONTROL.incoming, &[] as &[&str]);
        assert_eq!(roster::REMOTECONTROL.outgoing, &[types::MOUSEPAD_REQUEST]);
        assert_eq!(roster::REMOTEKEYBOARD.status, PluginStatus::Implemented);
        assert_eq!(
            roster::REMOTEKEYBOARD.incoming,
            &[types::MOUSEPAD_ECHO, types::MOUSEPAD_KEYBOARDSTATE]
        );
        assert_eq!(roster::REMOTEKEYBOARD.outgoing, &[types::MOUSEPAD_REQUEST]);
    }

    #[test]
    fn movement_uses_dx_and_dy() {
        let packet = movement_packet(12.0, -4.0).unwrap();
        assert_eq!(packet.ptype, types::MOUSEPAD_REQUEST);
        assert_eq!(packet.body.get("dx"), Some(&json!(12.0)));
        assert_eq!(packet.body.get("dy"), Some(&json!(-4.0)));
        assert!(!packet.body.contains_key("scroll"));
    }

    #[test]
    fn a_movement_of_nothing_or_a_non_finite_delta_is_rejected() {
        assert_eq!(movement_packet(0.0, 0.0), Err(RemoteInputError::InvalidDelta));
        assert_eq!(
            movement_packet(f64::NAN, 1.0),
            Err(RemoteInputError::InvalidDelta)
        );
        assert_eq!(
            movement_packet(1.0, f64::INFINITY),
            Err(RemoteInputError::InvalidDelta)
        );
    }

    #[test]
    fn clicks_scroll_and_holds_use_the_mousepad_flags() {
        let single = pointer_packet(&PointerEvent::click(PointerButton::Left, 1)).unwrap();
        assert_eq!(single.body.get("singleclick"), Some(&json!(true)));

        let double = pointer_packet(&PointerEvent::click(PointerButton::Left, 2)).unwrap();
        assert_eq!(double.body.get("doubleclick"), Some(&json!(true)));

        let middle = pointer_packet(&PointerEvent::click(PointerButton::Middle, 1)).unwrap();
        assert_eq!(middle.body.get("middleclick"), Some(&json!(true)));

        let right = pointer_packet(&PointerEvent::click(PointerButton::Right, 1)).unwrap();
        assert_eq!(right.body.get("rightclick"), Some(&json!(true)));

        let hold = pointer_packet(&PointerEvent::button(PointerButton::Left, true)).unwrap();
        assert_eq!(hold.body.get("singlehold"), Some(&json!(true)));

        let release = pointer_packet(&PointerEvent::button(PointerButton::Left, false)).unwrap();
        assert_eq!(release.body.get("singlerelease"), Some(&json!(true)));

        let scroll = pointer_packet(&PointerEvent::scroll(0.0, 2.0)).unwrap();
        assert_eq!(scroll.body.get("scroll"), Some(&json!(true)));
        assert_eq!(scroll.body.get("dy"), Some(&json!(2.0)));
    }

    #[test]
    fn a_key_packet_carries_text_modifiers_and_the_ack_flag() {
        let packet = key_packet(Some("hello"), None, modifiers(true), true).unwrap();
        assert_eq!(packet.body.get("key"), Some(&json!("hello")));
        assert_eq!(packet.body.get("ctrl"), Some(&json!(true)));
        assert_eq!(packet.body.get("shift"), Some(&json!(false)));
        assert_eq!(packet.body.get("sendAck"), Some(&json!(true)));
        assert!(!packet.body.contains_key("specialKey"));

        // Without an ack request the field is absent, not false.
        let quiet = key_packet(Some("a"), None, modifiers(false), false).unwrap();
        assert!(!quiet.body.contains_key("sendAck"));
    }

    #[test]
    fn a_special_key_uses_the_upstream_code_range() {
        let packet = key_packet(None, Some(12), modifiers(false), true).unwrap();
        assert_eq!(packet.body.get("specialKey"), Some(&json!(12)));
        assert!(!packet.body.contains_key("key"));

        assert_eq!(
            key_packet(None, Some(0), modifiers(false), true),
            Err(RemoteInputError::InvalidSpecialKey)
        );
        assert_eq!(
            key_packet(None, Some(SPECIAL_KEY_MAX + 1), modifiers(false), true),
            Err(RemoteInputError::InvalidSpecialKey)
        );
        // The last valid code is accepted.
        assert!(key_packet(None, Some(SPECIAL_KEY_MAX), modifiers(false), true).is_ok());
    }

    #[test]
    fn bad_or_missing_text_is_rejected() {
        assert_eq!(
            key_packet(Some(""), None, modifiers(false), true),
            Err(RemoteInputError::InvalidText)
        );
        assert_eq!(
            key_packet(None, None, modifiers(false), true),
            Err(RemoteInputError::InvalidText)
        );
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            key_packet(Some(&long), None, modifiers(false), true),
            Err(RemoteInputError::InvalidText)
        );
    }

    #[tokio::test]
    async fn queued_pointer_requests_leave_on_the_next_tick() {
        let mut plugin = RemoteControlPlugin::new();
        plugin.move_cursor(1.0, 2.0).unwrap();
        plugin
            .send_pointer(&PointerEvent::click(PointerButton::Left, 1))
            .unwrap();
        assert_eq!(plugin.pending(), 2);

        let output = tick(&mut plugin).await;
        assert_eq!(plugin.pending(), 0);
        assert_eq!(output.packets.len(), 2);
        assert!(output
            .packets
            .iter()
            .all(|packet| packet.ptype == types::MOUSEPAD_REQUEST));
        assert!(tick(&mut plugin).await.packets.is_empty());
    }

    #[tokio::test]
    async fn a_rejected_pointer_request_does_not_queue() {
        let mut plugin = RemoteControlPlugin::new();
        assert!(plugin.move_cursor(0.0, 0.0).is_err());
        assert_eq!(plugin.pending(), 0);
    }

    #[tokio::test]
    async fn the_control_plugin_ignores_inbound_packets() {
        let mut plugin = RemoteControlPlugin::new();
        let packet =
            Packet::from_body(types::MOUSEPAD_REQUEST, &json!({ "dx": 1.0 })).unwrap();
        assert!(feed(&mut plugin, &packet).await.packets.is_empty());
        assert_eq!(plugin.pending(), 0);
    }

    #[tokio::test]
    async fn typed_text_and_special_keys_are_acked() {
        let mut plugin = RemoteKeyboardPlugin::new();
        plugin.type_text("hi", modifiers(false)).unwrap();
        plugin.press_special(12, modifiers(true)).unwrap();

        let output = tick(&mut plugin).await;
        assert_eq!(output.packets.len(), 2);
        assert_eq!(output.packets[0].body.get("key"), Some(&json!("hi")));
        assert_eq!(output.packets[0].body.get("sendAck"), Some(&json!(true)));
        assert_eq!(output.packets[1].body.get("specialKey"), Some(&json!(12)));
    }

    #[tokio::test]
    async fn an_ack_makes_the_keyboard_ready() {
        let mut plugin = RemoteKeyboardPlugin::new();
        assert!(!plugin.ready());

        let ack = Packet::from_body(
            types::MOUSEPAD_ECHO,
            &json!({ "isAck": true, "key": "a" }),
        )
        .unwrap();
        feed(&mut plugin, &ack).await;
        assert!(plugin.ready());
    }

    #[tokio::test]
    async fn a_malformed_echo_does_not_make_the_keyboard_ready() {
        // isAck must be exactly true, and the echo must carry a key.
        for body in [
            json!({ "isAck": false, "key": "a" }),
            json!({ "key": "a" }),
            json!({ "isAck": true }),
            json!({ "isAck": "true", "key": "a" }),
        ] {
            let mut plugin = RemoteKeyboardPlugin::new();
            let packet = Packet::from_body(types::MOUSEPAD_ECHO, &body).unwrap();
            let output = feed(&mut plugin, &packet).await;
            assert!(!plugin.ready(), "body {body} must not count as an ack");
            assert!(output.events.iter().any(|event| matches!(
                event,
                PluginEvent::Log {
                    level: LogLevel::Warn,
                    ..
                }
            )));
        }
    }

    #[tokio::test]
    async fn keyboard_state_is_tracked_and_deduplicated() {
        let mut plugin = RemoteKeyboardPlugin::new();
        assert_eq!(plugin.keyboard_state(), None);

        let available = Packet::from_body(types::MOUSEPAD_KEYBOARDSTATE, &json!({ "state": true }))
            .unwrap();
        let output = feed(&mut plugin, &available).await;
        assert_eq!(plugin.keyboard_state(), Some(true));
        assert_eq!(output.events.len(), 1);

        // The same state again is not reported twice.
        let output = feed(&mut plugin, &available).await;
        assert!(output.events.is_empty());

        let gone = Packet::from_body(types::MOUSEPAD_KEYBOARDSTATE, &json!({ "state": false }))
            .unwrap();
        feed(&mut plugin, &gone).await;
        assert_eq!(plugin.keyboard_state(), Some(false));
    }

    #[tokio::test]
    async fn a_keyboard_state_without_a_boolean_is_a_warning() {
        let mut plugin = RemoteKeyboardPlugin::new();
        let packet =
            Packet::from_body(types::MOUSEPAD_KEYBOARDSTATE, &json!({ "state": "yes" })).unwrap();
        let output = feed(&mut plugin, &packet).await;
        assert_eq!(plugin.keyboard_state(), None);
        assert_eq!(output.events.len(), 1);
    }
}
