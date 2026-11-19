//! Use this desktop as an input device for the phone.
//!
//! The mirror of [`crate::shareinputdevices`]. Pressing the phone's share
//! toggle sends `kdeconnect.shareinputdevices.request {"share": true}` from the
//! desktop; once the phone acknowledges on `kdeconnect.shareinputdevices`,
//! pointer and key events go out on the mousepad request channel.
//!
//! Two states matter, and they are tracked separately:
//!
//! - **requested**: the user asked to share.
//! - **granted**: the phone said yes.
//!
//! Events are only sent while granted. Sending them while merely requested
//! would look like it worked — the packets would leave — while the phone
//! ignored every one, which is the sort of silent failure that makes a feature
//! seem broken rather than refused.

use async_trait::async_trait;
use handfast_desktop::{KeyEvent, PointerAction, PointerButton, PointerEvent};
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

/// Largest delta forwarded in one packet.
const MAX_DELTA: f64 = 2000.0;

/// Whether the desktop is sharing input with the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShareState {
    /// Not sharing.
    #[default]
    Idle,
    /// Asked the phone; waiting for its acknowledgement.
    Requested,
    /// The phone accepted and is receiving events.
    Granted,
}

/// Sends desktop input to the phone.
#[derive(Debug, Default)]
pub struct ShareInputDevicesRemotePlugin {
    state: ShareState,
    pending: Vec<Packet>,
}

impl ShareInputDevicesRemotePlugin {
    /// Create a share input devices remote plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: ShareState::Idle,
            pending: Vec::new(),
        }
    }

    /// Current sharing state.
    #[must_use]
    pub const fn state(&self) -> ShareState {
        self.state
    }

    /// Number of queued packets.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Ask the phone to start or stop accepting this desktop's input.
    pub fn request_share(&mut self, share: bool) {
        self.state = if share {
            ShareState::Requested
        } else {
            ShareState::Idle
        };
        if let Ok(packet) =
            Packet::new(types::SHAREINPUTDEVICES_REQUEST, json!({ "share": share }))
        {
            self.pending.push(packet);
        }
    }

    /// Queue a pointer move.
    ///
    /// Returns whether it was accepted. Movement outside `Granted` is dropped
    /// rather than queued, because a queued move would fire later against a
    /// stale cursor position.
    pub fn move_pointer(&mut self, dx: f64, dy: f64) -> bool {
        if self.state != ShareState::Granted {
            return false;
        }
        let dx = dx.clamp(-MAX_DELTA, MAX_DELTA);
        let dy = dy.clamp(-MAX_DELTA, MAX_DELTA);
        if dx == 0.0 && dy == 0.0 {
            return false;
        }
        match Packet::new(types::MOUSEPAD_REQUEST, json!({ "dx": dx, "dy": dy })) {
            Ok(packet) => {
                self.pending.push(packet);
                true
            }
            Err(_) => false,
        }
    }

    /// Queue a click.
    pub fn click(&mut self, button: PointerButton) -> bool {
        if self.state != ShareState::Granted {
            return false;
        }
        let field = match button {
            PointerButton::Left => "leftclick",
            PointerButton::Middle => "middleclick",
            PointerButton::Right => "rightclick",
        };
        let mut body = serde_json::Map::new();
        body.insert(field.to_string(), Value::Bool(true));
        match Packet::new(types::MOUSEPAD_REQUEST, Value::Object(body)) {
            Ok(packet) => {
                self.pending.push(packet);
                true
            }
            Err(_) => false,
        }
    }

    /// Queue typed text.
    pub fn type_text(&mut self, text: &str) -> bool {
        if self.state != ShareState::Granted || text.is_empty() {
            return false;
        }
        match Packet::new(types::MOUSEPAD_REQUEST, json!({ "key": text })) {
            Ok(packet) => {
                self.pending.push(packet);
                true
            }
            Err(_) => false,
        }
    }
}

#[async_trait]
impl Plugin for ShareInputDevicesRemotePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHAREINPUTDEVICESREMOTE
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::SHAREINPUTDEVICES {
            return Ok(PluginOutput::new());
        }
        let Some(share) = packet.body.get("share").and_then(Value::as_bool) else {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "peer answered a share request without a share flag".to_string(),
            }));
        };
        self.state = if share {
            ShareState::Granted
        } else {
            ShareState::Idle
        };
        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message: if share {
                "peer accepted desktop input sharing".to_string()
            } else {
                "peer declined or stopped input sharing".to_string()
            },
        }))
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        output.packets = std::mem::take(&mut self.pending);
        Ok(output)
    }
}

/// Builds [`ShareInputDevicesRemotePlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShareInputDevicesRemoteFactory;

impl PluginFactory for ShareInputDevicesRemoteFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHAREINPUTDEVICESREMOTE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ShareInputDevicesRemotePlugin::new())
    }
}

/// Convert a desktop pointer event into the mousepad body the phone expects.
///
/// Exposed so a UI layer can drive the plugin from real input events without
/// duplicating the mapping.
#[must_use]
pub fn body_for_pointer(event: PointerEvent) -> Option<Value> {
    match event.action {
        PointerAction::Move | PointerAction::Scroll => {
            Some(json!({ "dx": event.dx, "dy": event.dy }))
        }
        PointerAction::Click { button, .. } => {
            let field = match button {
                PointerButton::Left => "leftclick",
                PointerButton::Middle => "middleclick",
                PointerButton::Right => "rightclick",
            };
            Some(json!({ field: true }))
        }
        PointerAction::Press(PointerButton::Left) => Some(json!({ "singlehold": true })),
        PointerAction::Release(PointerButton::Left) => Some(json!({ "singlerelease": true })),
        _ => None,
    }
}

/// Convert a text key event into the mousepad body.
#[must_use]
pub fn body_for_key(event: &KeyEvent) -> Option<Value> {
    match event {
        KeyEvent::Text { text, .. } if !text.is_empty() => Some(json!({ "key": text })),
        KeyEvent::Text { .. } | KeyEvent::Special { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::{Modifiers, PointerEvent};
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

    async fn tick(plugin: &mut ShareInputDevicesRemotePlugin) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    async fn grant(plugin: &mut ShareInputDevicesRemotePlugin, share: bool) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(types::SHAREINPUTDEVICES, &json!({ "share": share })).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(
            roster::SHAREINPUTDEVICESREMOTE.status,
            PluginStatus::Implemented
        );
        assert_eq!(
            roster::SHAREINPUTDEVICESREMOTE.incoming,
            &[types::SHAREINPUTDEVICES]
        );
        assert_eq!(
            roster::SHAREINPUTDEVICESREMOTE.outgoing,
            &[types::SHAREINPUTDEVICES_REQUEST, types::MOUSEPAD_REQUEST]
        );
    }

    #[tokio::test]
    async fn a_request_is_sent_and_waits_for_the_peer() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        plugin.request_share(true);
        assert_eq!(plugin.state(), ShareState::Requested);

        let output = tick(&mut plugin).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::SHAREINPUTDEVICES_REQUEST);
        assert_eq!(output.packets[0].body.get("share"), Some(&json!(true)));
    }

    #[tokio::test]
    async fn input_is_refused_while_only_requested() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        plugin.request_share(true);
        assert!(!plugin.move_pointer(10.0, 10.0));
        assert!(!plugin.click(PointerButton::Left));
        assert!(!plugin.type_text("hello"));
        assert_eq!(plugin.pending(), 1, "only the request itself is queued");
    }

    #[tokio::test]
    async fn input_flows_once_the_peer_grants() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        plugin.request_share(true);
        let _ = tick(&mut plugin).await;
        grant(&mut plugin, true).await;
        assert_eq!(plugin.state(), ShareState::Granted);

        assert!(plugin.move_pointer(5.0, -5.0));
        assert!(plugin.click(PointerButton::Right));
        assert!(plugin.type_text("hi"));

        let output = tick(&mut plugin).await;
        assert_eq!(output.packets.len(), 3);
        assert_eq!(output.packets[0].ptype, types::MOUSEPAD_REQUEST);
        assert_eq!(output.packets[0].body.get("dx"), Some(&json!(5.0)));
        assert_eq!(output.packets[1].body.get("rightclick"), Some(&json!(true)));
        assert_eq!(output.packets[2].body.get("key"), Some(&json!("hi")));
    }

    #[tokio::test]
    async fn a_declined_request_leaves_sharing_idle() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        plugin.request_share(true);
        let _ = tick(&mut plugin).await;
        grant(&mut plugin, false).await;
        assert_eq!(plugin.state(), ShareState::Idle);
        assert!(!plugin.move_pointer(1.0, 1.0));
    }

    #[tokio::test]
    async fn stopping_sharing_stops_the_flow() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        grant(&mut plugin, true).await;
        plugin.request_share(false);
        assert_eq!(plugin.state(), ShareState::Idle);
        assert!(!plugin.move_pointer(1.0, 1.0));
    }

    #[tokio::test]
    async fn a_zero_move_is_not_sent() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        grant(&mut plugin, true).await;
        assert!(!plugin.move_pointer(0.0, 0.0));
        assert!(!plugin.type_text(""));
        assert_eq!(plugin.pending(), 0);
    }

    #[tokio::test]
    async fn a_huge_move_is_clamped() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        grant(&mut plugin, true).await;
        plugin.move_pointer(1e12, -1e12);
        let output = tick(&mut plugin).await;
        assert_eq!(output.packets[0].body.get("dx"), Some(&json!(MAX_DELTA)));
        assert_eq!(output.packets[0].body.get("dy"), Some(&json!(-MAX_DELTA)));
    }

    #[tokio::test]
    async fn an_answer_without_the_flag_is_reported() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(types::SHAREINPUTDEVICES, &json!({})).unwrap();
        let output = plugin.on_packet(&mut ctx, &packet).await.unwrap();
        assert_eq!(output.events.len(), 1);
        assert_eq!(plugin.state(), ShareState::Idle);
    }

    #[test]
    fn pointer_bodies_match_the_mousepad_fields() {
        assert_eq!(
            body_for_pointer(PointerEvent::movement(3.0, 4.0)),
            Some(json!({ "dx": 3.0, "dy": 4.0 }))
        );
        assert_eq!(
            body_for_pointer(PointerEvent::click(PointerButton::Middle, 1)),
            Some(json!({ "middleclick": true }))
        );
        assert_eq!(
            body_for_pointer(PointerEvent::button(PointerButton::Left, true)),
            Some(json!({ "singlehold": true }))
        );
        assert_eq!(
            body_for_pointer(PointerEvent::button(PointerButton::Left, false)),
            Some(json!({ "singlerelease": true }))
        );
        // A held right button has no mousepad equivalent.
        assert_eq!(
            body_for_pointer(PointerEvent::button(PointerButton::Right, true)),
            None
        );
    }

    #[test]
    fn key_bodies_only_cover_typed_text() {
        let text = KeyEvent::Text {
            text: "hi".to_string(),
            modifiers: Modifiers::default(),
        };
        assert_eq!(body_for_key(&text), Some(json!({ "key": "hi" })));
        let empty = KeyEvent::Text {
            text: String::new(),
            modifiers: Modifiers::default(),
        };
        assert_eq!(body_for_key(&empty), None);
        assert_eq!(
            body_for_key(&KeyEvent::Special {
                code: 1,
                modifiers: Modifiers::default()
            }),
            None
        );
    }
}
