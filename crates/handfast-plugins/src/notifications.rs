//! Notifications plugin.

use async_trait::async_trait;
use handfast_desktop::Notification;
use handfast_proto::{types, DeviceId, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

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

/// Reject values a desktop notification daemon should never receive.
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

/// Shows inbound phone notifications and closes them on cancel or disconnect.
#[derive(Debug, Default)]
pub struct NotificationsPlugin {
    posted: Vec<String>,
}

impl NotificationsPlugin {
    /// Create a notifications plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { posted: Vec::new() }
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
        match incoming {
            Incoming::Post(notification) => {
                notifier
                    .notify(&notification)
                    .await
                    .map_err(|error| PluginError::Backend(error.to_string()))?;
                if let Some(id) = notification.id {
                    self.posted.push(id);
                }
            }
            Incoming::Cancel(id) => notifier
                .close(&id)
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?,
        }
        Ok(PluginOutput::new())
    }
}

#[async_trait]
impl Plugin for NotificationsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::NOTIFICATIONS
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::NOTIFICATION => self.apply(ctx, packet).await,
            types::NOTIFICATION_REQUEST
            | types::NOTIFICATION_ACTION
            | types::NOTIFICATION_REPLY => Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!("{} is not supported yet", packet.ptype),
            })),
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
    use handfast_runtime::{Device, PluginStatus};
    use serde_json::json;

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

    async fn disconnect(
        plugin: &mut NotificationsPlugin,
        device: &Device,
        desktop: &handfast_desktop::Desktop,
    ) -> PluginOutput {
        let mut ctx = PluginContext { device, desktop };
        plugin.on_disconnect(&mut ctx).await.unwrap()
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
    async fn request_action_and_reply_are_logged_as_unsupported() {
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
            assert!(matches!(
                output.events.as_slice(),
                [PluginEvent::Log {
                    level: LogLevel::Info,
                    message,
                }] if message.contains("not supported")
            ));
        }
        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn malformed_notification_is_a_warning() {
        let mock = MockDesktop::new();
        let device = device();
        let packet = Packet::from_body(
            types::NOTIFICATION,
            &json!({"appName": "Messages", "title": "Ada"}),
        )
        .unwrap();
        let output = dispatch(&mut NotificationsPlugin::new(), &device, &mock, &packet).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
        assert!(mock.notifications().is_empty());
    }

    #[test]
    fn meta_matches_the_roster() {
        let factory = NotificationsFactory;
        assert_eq!(factory.meta(), &roster::NOTIFICATIONS);
        assert_eq!(factory.create().meta(), &roster::NOTIFICATIONS);
        assert_eq!(factory.meta().name, "notifications");
        assert_eq!(factory.meta().status, PluginStatus::Implemented);
        assert_eq!(factory.meta().incoming, [types::NOTIFICATION].as_slice());
    }

    #[tokio::test]
    async fn missing_notifier_is_an_error() {
        let desktop = handfast_desktop::Desktop::default();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        assert!(NotificationsPlugin::new()
            .on_packet(&mut ctx, &post())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn foreign_packets_are_ignored() {
        let mock = MockDesktop::new();
        let device = device();
        let packet = Packet::empty(types::PING).unwrap();
        let output = dispatch(&mut NotificationsPlugin::new(), &device, &mock, &packet).await;
        assert!(output.events.is_empty());
        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn two_devices_with_the_same_raw_id_do_not_collide() {
        let mock = MockDesktop::new();
        let first = device();
        let second = device();
        let mut first_plugin = NotificationsPlugin::new();
        let mut second_plugin = NotificationsPlugin::new();
        dispatch(&mut first_plugin, &first, &mock, &post()).await;
        dispatch(&mut second_plugin, &second, &mock, &post()).await;
        assert_eq!(mock.notifications().len(), 2);

        dispatch(&mut first_plugin, &first, &mock, &cancel(RAW_ID)).await;
        let remaining = mock.notifications();
        let expected = namespaced(&second, RAW_ID);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id.as_deref(), Some(expected.as_str()));

        dispatch(&mut second_plugin, &second, &mock, &cancel(RAW_ID)).await;
        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn oversized_id_is_rejected_with_a_warning() {
        let mock = MockDesktop::new();
        let device = device();
        let packet = post_with(&"a".repeat(MAX_ID_BYTES + 1));
        let output = dispatch(&mut NotificationsPlugin::new(), &device, &mock, &packet).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                message,
            }] if message.contains("id is longer")
        ));
        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn control_characters_are_rejected_with_a_warning() {
        let mock = MockDesktop::new();
        let device = device();
        let packet = Packet::from_body(
            types::NOTIFICATION,
            &json!({
                "id": RAW_ID,
                "appName": "Messages",
                "title": "Ada\u{0}",
                "text": "See you at six",
            }),
        )
        .unwrap();
        let output = dispatch(&mut NotificationsPlugin::new(), &device, &mock, &packet).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                message,
            }] if message.contains("control characters")
        ));
        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn newline_and_tab_are_allowed_in_title_and_text() {
        let mock = MockDesktop::new();
        let device = device();
        let packet = Packet::from_body(
            types::NOTIFICATION,
            &json!({
                "id": RAW_ID,
                "appName": "Messages",
                "title": "Ada\nReyes",
                "text": "\tSee you at six\nBring the keys",
            }),
        )
        .unwrap();
        let output = dispatch(&mut NotificationsPlugin::new(), &device, &mock, &packet).await;
        assert!(output.events.is_empty());
        let notifications = mock.notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].title, "Ada\nReyes");
        assert_eq!(notifications[0].text, "\tSee you at six\nBring the keys");
    }

    #[tokio::test]
    async fn disconnect_closes_exactly_the_posted_notifications() {
        let mock = MockDesktop::new();
        let device = device();
        let mut plugin = NotificationsPlugin::new();
        dispatch(&mut plugin, &device, &mock, &post()).await;
        dispatch(&mut plugin, &device, &mock, &post_with("second")).await;

        let mut unrelated = Notification::new("Other", "Title", "Text");
        unrelated.id = Some("unrelated".to_string());
        mock.desktop()
            .notifier
            .as_ref()
            .unwrap()
            .notify(&unrelated)
            .await
            .unwrap();

        let desktop = mock.desktop();
        let output = disconnect(&mut plugin, &device, &desktop).await;
        assert!(output.events.is_empty());
        assert_eq!(mock.notifications(), vec![unrelated.clone()]);

        let output = disconnect(&mut plugin, &device, &desktop).await;
        assert!(output.events.is_empty());
        assert_eq!(mock.notifications(), vec![unrelated]);
    }

    #[tokio::test]
    async fn disconnect_without_a_notifier_is_not_fatal() {
        let mock = MockDesktop::new();
        let device = device();
        let mut plugin = NotificationsPlugin::new();
        dispatch(&mut plugin, &device, &mock, &post()).await;

        let desktop = handfast_desktop::Desktop::default();
        let output = disconnect(&mut plugin, &device, &desktop).await;
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
        assert_eq!(mock.notifications().len(), 1);

        let desktop = mock.desktop();
        let output = disconnect(&mut plugin, &device, &desktop).await;
        assert!(output.events.is_empty());
        assert_eq!(mock.notifications().len(), 1);
    }
}
