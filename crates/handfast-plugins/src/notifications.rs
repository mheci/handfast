//! Notifications plugin.

use async_trait::async_trait;
use handfast_desktop::Notification;
use handfast_proto::{types, DeviceId, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

const MAX_ID_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 4 * 1024;

/// A parsed `kdeconnect.notification` body.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Incoming {
    Post(Notification),
    Cancel(String),
}

fn warn(message: String) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message,
    })
}

fn string_field(packet: &Packet, key: &str) -> String {
    packet
        .body
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn validate_field(
    name: &str,
    value: &str,
    max: usize,
    allow_newline_and_tab: bool,
) -> Result<(), String> {
    if value.len() > max {
        return Err(format!("{name} is longer than {max} bytes"));
    }
    let rejected = value.chars().any(|character| {
        character.is_control() && !(allow_newline_and_tab && matches!(character, '\n' | '\t'))
    });
    if rejected {
        return Err(format!("{name} contains control characters"));
    }
    Ok(())
}

fn parse(packet: &Packet, device_id: &DeviceId) -> Result<Incoming, String> {
    let is_cancel = match packet.body.get("isCancel") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("isCancel is not a boolean".to_string()),
    };
    let id = packet
        .body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("missing id")?;
    validate_field("id", id, MAX_ID_BYTES, false)?;
    let id = format!("{device_id}|{id}");
    if is_cancel {
        return Ok(Incoming::Cancel(id));
    }
    let app_name = string_field(packet, "appName");
    let title = string_field(packet, "title");
    let text = string_field(packet, "text");
    validate_field("appName", &app_name, MAX_TEXT_BYTES, false)?;
    validate_field("title", &title, MAX_TEXT_BYTES, true)?;
    validate_field("text", &text, MAX_TEXT_BYTES, true)?;
    let reply_id = packet
        .body
        .get("requestReplyId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    if let Some(reply_id) = reply_id {
        validate_field("requestReplyId", reply_id, MAX_ID_BYTES, false)?;
    }
    let mut notification = Notification::new(app_name, title, text);
    notification.id = Some(id);
    notification.reply_id = reply_id.map(ToString::to_string);
    Ok(Incoming::Post(notification))
}

/// Largest action/reply id accepted, matching the inbound id cap.
const MAX_KEY_BYTES: usize = 256;

/// Split a Handfast notification id back into the peer's raw id.
///
/// Inbound ids are namespaced `{device_id}|{raw}` so two phones cannot collide
/// in the desktop notification daemon. An id without the prefix is passed
/// through, which keeps a locally generated id working.
fn raw_id(id: &str) -> &str {
    id.split_once('|').map_or(id, |(_, raw)| raw)
}

/// Shows inbound phone notifications and closes them on cancel or disconnect.
///
/// The control direction is the other half of the plugin: a notification on
/// this desktop can be dismissed, answered or acted on. Those packets are
/// queued and released on the next tick, because a plugin hook is the only
/// place the session sends from.
#[derive(Debug, Default)]
pub struct NotificationsPlugin {
    posted: Vec<String>,
    pending: Vec<Packet>,
}

/// Why a control action was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    /// The id or key is empty or too long.
    InvalidId,
    /// The message is empty or too long.
    InvalidMessage,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId => formatter.write_str("notification id is empty or too long"),
            Self::InvalidMessage => formatter.write_str("notification message is empty or too long"),
        }
    }
}

fn valid_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_KEY_BYTES
        && !value.chars().any(|character| character.is_control())
}

impl NotificationsPlugin {
    /// Create a notifications plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            posted: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Queue a dismissal of one of the peer's notifications.
    ///
    /// # Errors
    ///
    /// Fails when the id is unusable.
    pub fn dismiss(&mut self, id: &str) -> Result<(), ControlError> {
        if !valid_key(id) {
            return Err(ControlError::InvalidId);
        }
        self.pending
            .push(Packet::new(types::NOTIFICATION_REQUEST, json!({ "cancel": raw_id(id) }))
                .map_err(|_| ControlError::InvalidId)?);
        Ok(())
    }

    /// Queue a request for the peer's list of active notifications.
    ///
    /// # Errors
    ///
    /// Fails only if the packet cannot be built, which cannot happen for a
    /// constant type and an object body.
    pub fn request_active(&mut self) -> Result<(), ControlError> {
        self.pending
            .push(Packet::new(types::NOTIFICATION_REQUEST, json!({ "request": true }))
                .map_err(|_| ControlError::InvalidId)?);
        Ok(())
    }

    /// Queue a reply to a notification that asked for one.
    ///
    /// # Errors
    ///
    /// Fails when the reply id or the message is unusable.
    pub fn reply(&mut self, request_reply_id: &str, message: &str) -> Result<(), ControlError> {
        if !valid_key(request_reply_id) {
            return Err(ControlError::InvalidId);
        }
        if message.is_empty() || message.len() > MAX_TEXT_BYTES {
            return Err(ControlError::InvalidMessage);
        }
        self.pending.push(
            Packet::new(
                types::NOTIFICATION_REPLY,
                json!({
                    "requestReplyId": raw_id(request_reply_id),
                    "message": message,
                }),
            )
            .map_err(|_| ControlError::InvalidMessage)?,
        );
        Ok(())
    }

    /// Queue an action on a notification.
    ///
    /// # Errors
    ///
    /// Fails when the key or the action label is unusable.
    pub fn action(&mut self, key: &str, action: &str) -> Result<(), ControlError> {
        if !valid_key(key) {
            return Err(ControlError::InvalidId);
        }
        if action.is_empty() || action.len() > MAX_TEXT_BYTES {
            return Err(ControlError::InvalidMessage);
        }
        self.pending.push(
            Packet::new(
                types::NOTIFICATION_ACTION,
                json!({ "key": raw_id(key), "action": action }),
            )
            .map_err(|_| ControlError::InvalidMessage)?,
        );
        Ok(())
    }

    /// How many control packets are waiting for the next tick.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The queued control packets, for inspection in tests.
    #[must_use]
    pub fn pending_packets(&self) -> &[Packet] {
        &self.pending
    }

    async fn apply(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        let incoming = match parse(packet, &ctx.device.id) {
            Ok(incoming) => incoming,
            Err(reason) => {
                return Ok(warn(format!("discarding malformed notification: {reason}")));
            }
        };
        let notifier = ctx
            .desktop
            .notifier
            .as_ref()
            .ok_or(PluginError::UnsupportedDesktop("notifications"))?;
        let mut accepted_replies: Vec<String> = Vec::new();
        match incoming {
            Incoming::Post(notification) => {
                notifier
                    .notify(&notification)
                    .await
                    .map_err(|error| PluginError::Backend(error.to_string()))?;
                if let Some(id) = notification.id.clone() {
                    self.posted.push(id);
                }
                if notification.reply_id.is_some() {
                    // The notifier shows a reply prompt for this notification;
                    // the plugin answers it through [`Self::reply`].
                    accepted_replies.push(notification.id.clone().unwrap_or_default());
                }
            }
            Incoming::Cancel(id) => notifier
                .close(&id)
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?,
        }
        let mut output = PluginOutput::new();
        for id in accepted_replies {
            output = output.with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!("notification {id} accepts a reply"),
            });
        }
        Ok(output)
    }
}

#[async_trait]
impl Plugin for NotificationsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::NOTIFICATIONS
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
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::NOTIFICATION => self.apply(ctx, packet).await,
            // These travel from the desktop to the phone; a peer sending them
            // is confused rather than hostile, so note it at debug level.
            types::NOTIFICATION_REQUEST
            | types::NOTIFICATION_ACTION
            | types::NOTIFICATION_REPLY => Ok(PluginOutput::new()),
            _ => Ok(PluginOutput::new()),
        }
    }

    async fn on_disconnect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let posted = std::mem::take(&mut self.posted);
        if posted.is_empty() {
            return Ok(PluginOutput::new());
        }
        let Some(notifier) = ctx.desktop.notifier.as_ref() else {
            return Ok(warn("notification adapter unavailable".to_string()));
        };
        let mut output = PluginOutput::new();
        for id in posted {
            if let Err(error) = notifier.close(&id).await {
                output = output.with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("failed to close notification: {error}"),
                });
            }
        }
        Ok(output)
    }
}

/// Builds notifications plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct NotificationsFactory;

impl PluginFactory for NotificationsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::NOTIFICATIONS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(NotificationsPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;

    const RAW_ID: &str = "0|com.example|1|null|1000";

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn dispatch(
        plugin: &mut NotificationsPlugin,
        device: &Device,
        mock: &MockDesktop,
        packet: &Packet,
    ) -> PluginOutput {
        let desktop = mock.desktop();
        let mut ctx = PluginContext {
            device,
            desktop: &desktop,
        };
        plugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    fn post_with(id: &str) -> Packet {
        Packet::from_body(
            types::NOTIFICATION,
            &json!({
                "id": id,
                "appName": "Messages",
                "title": "Ada",
                "text": "See you at six",
            }),
        )
        .unwrap()
    }

    fn post() -> Packet {
        post_with(RAW_ID)
    }

    fn cancel(id: &str) -> Packet {
        Packet::from_body(types::NOTIFICATION, &json!({"isCancel": true, "id": id})).unwrap()
    }

    fn namespaced(device: &Device, raw_id: &str) -> String {
        format!("{}|{raw_id}", device.id)
    }

    #[tokio::test]
    async fn new_notification_reaches_the_notifier() {
        let mock = MockDesktop::new();
        let device = device();
        let output = dispatch(&mut NotificationsPlugin::new(), &device, &mock, &post()).await;
        let mut expected = Notification::new("Messages", "Ada", "See you at six");
        expected.id = Some(namespaced(&device, RAW_ID));
        assert_eq!(mock.notifications(), vec![expected]);
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn cancel_closes_the_notification() {
        let mock = MockDesktop::new();
        let device = device();
        let mut plugin = NotificationsPlugin::new();
        dispatch(&mut plugin, &device, &mock, &post()).await;
        assert_eq!(mock.notifications().len(), 1);

        dispatch(&mut plugin, &device, &mock, &cancel(RAW_ID)).await;
        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn control_packets_from_the_peer_are_ignored() {
        let mock = MockDesktop::new();
        let device = device();
        let mut plugin = NotificationsPlugin::new();
        let packets = [
            Packet::from_body(types::NOTIFICATION_REQUEST, &json!({"request": true})).unwrap(),
            Packet::from_body(
                types::NOTIFICATION_ACTION,
                &json!({"key": RAW_ID, "action": "Reply"}),
            )
            .unwrap(),
            Packet::from_body(
                types::NOTIFICATION_REPLY,
                &json!({"requestReplyId": "reply-1", "message": "On my way"}),
            )
            .unwrap(),
        ];
        for packet in packets {
            let output = dispatch(&mut plugin, &device, &mock, &packet).await;
            assert!(
                output.events.is_empty(),
                "a control packet from the peer is not an event"
            );
            assert!(output.packets.is_empty());
        }
        assert!(mock.notifications().is_empty());
    }

    #[test]
    fn a_dismissal_targets_the_raw_id() {
        let mut plugin = NotificationsPlugin::new();
        plugin.dismiss("device|0|com.example|1|null|1000").unwrap();
        let packet = &plugin.pending_packets()[0];
        assert_eq!(packet.ptype, types::NOTIFICATION_REQUEST);
        assert_eq!(packet.body["cancel"], json!("0|com.example|1|null|1000"));
    }

    #[test]
    fn a_reply_targets_the_raw_reply_id() {
        let mut plugin = NotificationsPlugin::new();
        plugin.reply("device|reply-7", "on my way").unwrap();
        let packet = &plugin.pending_packets()[0];
        assert_eq!(packet.ptype, types::NOTIFICATION_REPLY);
        assert_eq!(packet.body["requestReplyId"], json!("reply-7"));
        assert_eq!(packet.body["message"], json!("on my way"));
    }

    #[test]
    fn an_action_carries_the_key_and_label() {
        let mut plugin = NotificationsPlugin::new();
        plugin.action("device|key-1", "Mark as read").unwrap();
        let packet = &plugin.pending_packets()[0];
        assert_eq!(packet.ptype, types::NOTIFICATION_ACTION);
        assert_eq!(packet.body["key"], json!("key-1"));
        assert_eq!(packet.body["action"], json!("Mark as read"));
    }

    #[test]
    fn an_empty_control_field_is_rejected() {
        let mut plugin = NotificationsPlugin::new();
        assert_eq!(plugin.dismiss(""), Err(ControlError::InvalidId));
        assert_eq!(
            plugin.reply("id", ""),
            Err(ControlError::InvalidMessage)
        );
        assert_eq!(plugin.action("id", ""), Err(ControlError::InvalidMessage));
        assert_eq!(plugin.pending(), 0, "nothing was queued");
    }

    #[tokio::test]
    async fn queued_controls_are_released_on_the_next_tick() {
        let mock = MockDesktop::new();
        let device = device();
        let desktop = mock.desktop();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = NotificationsPlugin::new();
        plugin.request_active().unwrap();
        plugin.dismiss("device|id").unwrap();
        assert_eq!(plugin.pending(), 2);
        let output = plugin.on_tick(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 2);
        assert_eq!(plugin.pending(), 0);
        let again = plugin.on_tick(&mut ctx).await.unwrap();
        assert!(again.packets.is_empty());
    }
}
