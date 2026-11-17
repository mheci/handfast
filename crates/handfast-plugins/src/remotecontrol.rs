//! Remote mouse and keyboard control for a peer phone.
//!
//! The desktop sends `kdeconnect.mousepad.request` packets. The phone injects
//! the input; nothing is injected locally.

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    Plugin, PluginContext, PluginError, PluginFactory, PluginMeta, PluginOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::roster;

/// Highest special key code the protocol carries.
pub const MAX_SPECIAL_KEY: u8 = 32;

/// A mouse action sent to a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteClick {
    /// Left click.
    Single,
    /// Left double click.
    Double,
    /// Middle click.
    Middle,
    /// Right click.
    Right,
    /// Left button press.
    Hold,
    /// Left button release.
    Release,
}

impl RemoteClick {
    fn field(self) -> &'static str {
        match self {
            Self::Single => "singleclick",
            Self::Double => "doubleclick",
            Self::Middle => "middleclick",
            Self::Right => "rightclick",
            Self::Hold => "singlehold",
            Self::Release => "singlerelease",
        }
    }
}

/// Keyboard modifiers sent with a key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteInputModifiers {
    /// Alt is held.
    pub alt: bool,
    /// Control is held.
    pub ctrl: bool,
    /// Shift is held.
    pub shift: bool,
    /// Super is held.
    pub sup: bool,
}

fn with_modifiers(mut body: Map<String, Value>, modifiers: RemoteInputModifiers) -> Value {
    body.insert("alt".to_string(), Value::Bool(modifiers.alt));
    body.insert("ctrl".to_string(), Value::Bool(modifiers.ctrl));
    body.insert("shift".to_string(), Value::Bool(modifiers.shift));
    body.insert("super".to_string(), Value::Bool(modifiers.sup));
    Value::Object(body)
}

/// A relative pointer move.
pub fn move_request(dx: f64, dy: f64) -> Result<Packet, ProtoError> {
    Packet::new(types::MOUSEPAD_REQUEST, json!({ "dx": dx, "dy": dy }))
}

/// A scroll step.
pub fn scroll_request(dx: f64, dy: f64) -> Result<Packet, ProtoError> {
    Packet::new(
        types::MOUSEPAD_REQUEST,
        json!({ "dx": dx, "dy": dy, "scroll": true }),
    )
}

/// One mouse click or button action.
pub fn click_request(click: RemoteClick) -> Result<Packet, ProtoError> {
    let mut body = Map::new();
    body.insert(click.field().to_string(), Value::Bool(true));
    Packet::new(types::MOUSEPAD_REQUEST, Value::Object(body))
}

/// Text typed on the peer.
pub fn key_request(text: &str, modifiers: RemoteInputModifiers) -> Result<Packet, ProtoError> {
    let mut body = Map::new();
    body.insert("key".to_string(), Value::String(text.to_string()));
    body.insert("specialKey".to_string(), json!(0));
    body.insert("sendAck".to_string(), Value::Bool(false));
    Packet::new(types::MOUSEPAD_REQUEST, with_modifiers(body, modifiers))
}

/// One special key press.
pub fn special_key_request(
    code: u8,
    modifiers: RemoteInputModifiers,
) -> Result<Packet, ProtoError> {
    let mut body = Map::new();
    body.insert("key".to_string(), Value::String(String::new()));
    body.insert("specialKey".to_string(), json!(code));
    body.insert("sendAck".to_string(), Value::Bool(false));
    Packet::new(types::MOUSEPAD_REQUEST, with_modifiers(body, modifiers))
}

/// Sends pointer and click input to a peer.
#[derive(Debug, Default)]
pub struct RemoteControlPlugin;

/// Sends keyboard input to a peer.
#[derive(Debug, Default)]
pub struct RemoteKeyboardPlugin;

#[async_trait]
impl Plugin for RemoteControlPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECONTROL
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        _packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        Ok(PluginOutput::new())
    }
}

#[async_trait]
impl Plugin for RemoteKeyboardPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTEKEYBOARD
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        _packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        Ok(PluginOutput::new())
    }
}

/// Creates one remote control plugin per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteControlFactory;

/// Creates one remote keyboard plugin per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteKeyboardFactory;

impl PluginFactory for RemoteControlFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECONTROL
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteControlPlugin)
    }
}

impl PluginFactory for RemoteKeyboardFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTEKEYBOARD
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteKeyboardPlugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers() -> RemoteInputModifiers {
        RemoteInputModifiers {
            alt: true,
            ctrl: false,
            shift: true,
            sup: false,
        }
    }

    #[test]
    fn a_move_carries_the_delta() {
        let packet = move_request(-1.5, 2.0).unwrap();
        assert_eq!(packet.ptype, types::MOUSEPAD_REQUEST);
        assert_eq!(packet.body["dx"], -1.5);
        assert_eq!(packet.body["dy"], 2.0);
        assert!(packet.body.get("scroll").is_none());
    }

    #[test]
    fn a_scroll_sets_the_flag() {
        let packet = scroll_request(0.0, -3.0).unwrap();
        assert_eq!(packet.body["scroll"], true);
        assert_eq!(packet.body["dy"], -3.0);
    }

    #[test]
    fn every_click_maps_to_its_field() {
        for (click, field) in [
            (RemoteClick::Single, "singleclick"),
            (RemoteClick::Double, "doubleclick"),
            (RemoteClick::Middle, "middleclick"),
            (RemoteClick::Right, "rightclick"),
            (RemoteClick::Hold, "singlehold"),
            (RemoteClick::Release, "singlerelease"),
        ] {
            let packet = click_request(click).unwrap();
            assert_eq!(packet.body.get(field), Some(&Value::Bool(true)), "{field}");
            assert_eq!(packet.body.len(), 1, "{field}");
        }
    }

    #[test]
    fn a_key_carries_the_text_and_modifiers() {
        let packet = key_request("hello", modifiers()).unwrap();
        assert_eq!(packet.body["key"], "hello");
        assert_eq!(packet.body["specialKey"], 0);
        assert_eq!(packet.body["sendAck"], false);
        assert_eq!(packet.body["alt"], true);
        assert_eq!(packet.body["shift"], true);
        assert_eq!(packet.body["ctrl"], false);
        assert_eq!(packet.body["super"], false);
    }

    #[test]
    fn a_special_key_carries_the_code() {
        let packet = special_key_request(4, RemoteInputModifiers::default()).unwrap();
        assert_eq!(packet.body["key"], "");
        assert_eq!(packet.body["specialKey"], 4);
        assert_eq!(packet.body["alt"], false);
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(&roster::REMOTECONTROL, RemoteControlPlugin.meta());
        assert_eq!(&roster::REMOTEKEYBOARD, RemoteKeyboardPlugin.meta());
        assert_eq!(
            roster::REMOTECONTROL.status,
            handfast_runtime::PluginStatus::Implemented
        );
        assert_eq!(
            roster::REMOTEKEYBOARD.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
