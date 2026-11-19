//! Treat the phone as an extra input device for this desktop.
//!
//! This is the newer, explicit form of what `mousepad` does implicitly. The
//! phone sends `kdeconnect.shareinputdevices.request {"share": true}` to start
//! sharing and `{"share": false}` to stop; while sharing, pointer and key
//! events arrive on the mousepad channels, which this plugin forwards to the
//! desktop's [`handfast_desktop::Input`] adapter.
//!
//! The gate is the point. `mousepad` accepts input whenever the plugin is
//! enabled; here nothing is injected until the peer has explicitly asked to
//! share. A phone in a pocket cannot type into a desktop because a packet
//! arrived — only after the user started sharing from the phone's own UI, and
//! a disconnect stops it.
//!
//! The reply echoes `share` so the phone can show the toggle state.

use async_trait::async_trait;
use handfast_desktop::{KeyEvent, Modifiers, PointerAction, PointerButton, PointerEvent};
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

/// Largest accepted pointer delta, in pixels.
///
/// A phone can send an arbitrarily large delta; clamping keeps a bogus sample
/// from throwing the cursor to a screen corner in one step.
const MAX_DELTA: f64 = 2000.0;

/// Mousepad button fields that mean a click.
const CLICK_FIELDS: [(&str, PointerButton); 3] = [
    ("leftclick", PointerButton::Left),
    ("middleclick", PointerButton::Middle),
    ("rightclick", PointerButton::Right),
];

/// Forwards phone input to the desktop, but only while sharing is on.
#[derive(Debug, Default)]
pub struct ShareInputDevicesPlugin {
    sharing: bool,
    /// Pointer events injected, for tests and the log.
    injected: usize,
    /// Events dropped because sharing was off.
    ignored: usize,
}

impl ShareInputDevicesPlugin {
    /// Create a share input devices plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sharing: false,
            injected: 0,
            ignored: 0,
        }
    }

    /// Whether the peer is currently sharing input.
    #[must_use]
    pub const fn is_sharing(&self) -> bool {
        self.sharing
    }

    /// Events injected while sharing.
    #[must_use]
    pub const fn injected(&self) -> usize {
        self.injected
    }

    /// Events dropped because sharing was off.
    #[must_use]
    pub const fn ignored(&self) -> usize {
        self.ignored
    }

    /// Stop sharing, without sending a reply.
    ///
    /// Called when the session ends: a reconnect must not resume injection
    /// from a peer that never asked again.
    pub fn disconnect(&mut self) {
        self.sharing = false;
    }

    fn reply(sharing: bool) -> Option<Packet> {
        Packet::new(types::SHAREINPUTDEVICES, json!({ "share": sharing })).ok()
    }

    /// Convert a mousepad pointer body into a desktop pointer event.
    fn pointer_event(body: &serde_json::Map<String, Value>) -> Option<PointerEvent> {
        if let Some(button) = CLICK_FIELDS
            .iter()
            .find(|(field, _)| body.get(*field).and_then(Value::as_bool).unwrap_or(false))
        {
            return Some(PointerEvent::click(button.1, 1));
        }
        if body
            .get("singlehold")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Some(PointerEvent::button(PointerButton::Left, true));
        }
        if body
            .get("singlerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Some(PointerEvent::button(PointerButton::Left, false));
        }
        let dx = body.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
        let dy = body.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        let dx = dx.clamp(-MAX_DELTA, MAX_DELTA);
        let dy = dy.clamp(-MAX_DELTA, MAX_DELTA);
        Some(PointerEvent {
            dx,
            dy,
            scroll: false,
            action: PointerAction::Move,
        })
    }
}

#[async_trait]
impl Plugin for ShareInputDevicesPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHAREINPUTDEVICES
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::SHAREINPUTDEVICES_REQUEST => {
                let Some(share) = packet.body.get("share").and_then(Value::as_bool) else {
                    return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: "share input request without a share flag".to_string(),
                    }));
                };
                self.sharing = share;
                Ok(PluginOutput::new()
                    .with_optional_packet(ShareInputDevicesPlugin::reply(share))
                    .with_event(PluginEvent::Log {
                        level: LogLevel::Info,
                        message: if share {
                            "peer started sharing input".to_string()
                        } else {
                            "peer stopped sharing input".to_string()
                        },
                    }))
            }
            types::MOUSEPAD_REQUEST => {
                if !self.sharing {
                    self.ignored += 1;
                    return Ok(PluginOutput::new());
                }
                let Some(input) = &ctx.desktop.input else {
                    return Ok(PluginOutput::new());
                };
                if let Some(event) = ShareInputDevicesPlugin::pointer_event(&packet.body) {
                    if let Err(error) = input.pointer(event).await {
                        return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                            level: LogLevel::Warn,
                            message: format!("pointer injection failed: {error}"),
                        }));
                    }
                    self.injected += 1;
                    return Ok(PluginOutput::new());
                }
                if let Some(text) = packet.body.get("key").and_then(Value::as_str) {
                    let event = KeyEvent::Text {
                        text: text.to_string(),
                        modifiers: Modifiers::default(),
                    };
                    if let Err(error) = input.key(event).await {
                        return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                            level: LogLevel::Warn,
                            message: format!("key injection failed: {error}"),
                        }));
                    }
                    self.injected += 1;
                }
                Ok(PluginOutput::new())
            }
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Builds [`ShareInputDevicesPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShareInputDevicesFactory;

impl PluginFactory for ShareInputDevicesFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHAREINPUTDEVICES
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ShareInputDevicesPlugin::new())
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
        plugin: &mut ShareInputDevicesPlugin,
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

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::SHAREINPUTDEVICES.status, PluginStatus::Implemented);
        assert_eq!(
            roster::SHAREINPUTDEVICES.incoming,
            &[types::SHAREINPUTDEVICES_REQUEST, types::MOUSEPAD_REQUEST]
        );
        assert_eq!(
            roster::SHAREINPUTDEVICES.outgoing,
            &[types::SHAREINPUTDEVICES]
        );
    }

    #[tokio::test]
    async fn nothing_is_injected_before_sharing_starts() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();

        send(
            &mut plugin,
            &mock.desktop(),
            types::MOUSEPAD_REQUEST,
            json!({ "dx": 10.0, "dy": 5.0 }),
        )
        .await;

        assert!(mock.input_events().is_empty());
        assert_eq!(plugin.ignored(), 1);
        assert_eq!(plugin.injected(), 0);
    }

    #[tokio::test]
    async fn the_share_request_is_acknowledged() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();

        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::SHAREINPUTDEVICES_REQUEST,
            json!({ "share": true }),
        )
        .await;
        assert!(plugin.is_sharing());
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::SHAREINPUTDEVICES);
        assert_eq!(output.packets[0].body.get("share"), Some(&json!(true)));
    }

    #[tokio::test]
    async fn movement_is_injected_while_sharing() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::SHAREINPUTDEVICES_REQUEST,
            json!({ "share": true }),
        )
        .await;

        send(
            &mut plugin,
            &mock.desktop(),
            types::MOUSEPAD_REQUEST,
            json!({ "dx": 10.0, "dy": -5.0 }),
        )
        .await;

        assert_eq!(plugin.injected(), 1);
        match &mock.input_events()[0] {
            handfast_desktop::mock::InputEvent::Pointer(pointer) => {
                assert_eq!(pointer.action, PointerAction::Move);
                assert!((pointer.dx - 10.0).abs() < 1e-9);
                assert!((pointer.dy + 5.0).abs() < 1e-9);
            }
            other => panic!("expected a pointer event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_huge_delta_is_clamped() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::SHAREINPUTDEVICES_REQUEST,
            json!({ "share": true }),
        )
        .await;
        send(
            &mut plugin,
            &mock.desktop(),
            types::MOUSEPAD_REQUEST,
            json!({ "dx": 1e12, "dy": 0.0 }),
        )
        .await;

        match &mock.input_events()[0] {
            handfast_desktop::mock::InputEvent::Pointer(pointer) => {
                assert!((pointer.dx - MAX_DELTA).abs() < 1e-9);
            }
            other => panic!("expected a pointer event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clicks_and_held_buttons_reach_the_desktop() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::SHAREINPUTDEVICES_REQUEST,
            json!({ "share": true }),
        )
        .await;
        for body in [
            json!({ "rightclick": true }),
            json!({ "singlehold": true }),
            json!({ "singlerelease": true }),
        ] {
            send(&mut plugin, &mock.desktop(), types::MOUSEPAD_REQUEST, body).await;
        }

        let events = mock.input_events();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            handfast_desktop::mock::InputEvent::Pointer(PointerEvent {
                action: PointerAction::Click {
                    button: PointerButton::Right,
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            events[1],
            handfast_desktop::mock::InputEvent::Pointer(PointerEvent {
                action: PointerAction::Press(PointerButton::Left),
                ..
            })
        ));
        assert!(matches!(
            events[2],
            handfast_desktop::mock::InputEvent::Pointer(PointerEvent {
                action: PointerAction::Release(PointerButton::Left),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn text_keys_are_typed() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();
        send(
            &mut plugin,
            &mock.desktop(),
            types::SHAREINPUTDEVICES_REQUEST,
            json!({ "share": true }),
        )
        .await;
        send(
            &mut plugin,
            &mock.desktop(),
            types::MOUSEPAD_REQUEST,
            json!({ "key": "hello" }),
        )
        .await;

        match &mock.input_events()[0] {
            handfast_desktop::mock::InputEvent::Key(KeyEvent::Text { text, .. }) => {
                assert_eq!(text, "hello");
            }
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stopping_sharing_blocks_injection_again() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();
        for share in [true, false] {
            send(
                &mut plugin,
                &mock.desktop(),
                types::SHAREINPUTDEVICES_REQUEST,
                json!({ "share": share }),
            )
            .await;
        }
        send(
            &mut plugin,
            &mock.desktop(),
            types::MOUSEPAD_REQUEST,
            json!({ "dx": 10.0, "dy": 10.0 }),
        )
        .await;

        assert!(mock.input_events().is_empty());
        assert!(!plugin.is_sharing());
    }

    #[test]
    fn a_disconnect_stops_sharing() {
        let mut plugin = ShareInputDevicesPlugin::new();
        plugin.sharing = true;
        plugin.disconnect();
        assert!(!plugin.is_sharing());
    }

    #[tokio::test]
    async fn a_request_without_the_flag_is_reported() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::SHAREINPUTDEVICES_REQUEST,
            json!({}),
        )
        .await;
        assert!(output.packets.is_empty());
        assert_eq!(output.events.len(), 1);
        assert!(!plugin.is_sharing());
    }
}
