//! Notifications plugin.

use async_trait::async_trait;
use handfast_desktop::Notification;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

/// A parsed `kdeconnect.notification` body.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Incoming {
    Post(Notification),
    Cancel(String),
}

fn string_field(packet: &Packet, key: &str) -> String {
    packet
        .body
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn parse(packet: &Packet) -> Result<Incoming, &'static str> {
    let is_cancel = match packet.body.get("isCancel") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("isCancel is not a boolean"),
    };
    let id = packet
        .body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("missing id")?;
    if is_cancel {
        return Ok(Incoming::Cancel(id.to_string()));
    }
    let reply_id = packet
        .body
        .get("requestReplyId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string);
    let mut notification = Notification::new(
        string_field(packet, "appName"),
        string_field(packet, "title"),
        string_field(packet, "text"),
    );
    notification.id = Some(id.to_string());
    notification.reply_id = reply_id;
    Ok(Incoming::Post(notification))
}

/// Shows inbound phone notifications and closes them on cancel.
#[derive(Debug, Default)]
pub struct NotificationsPlugin;

impl NotificationsPlugin {
    /// Create a notifications plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    async fn apply(
        &self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        let incoming = match parse(packet) {
            Ok(incoming) => incoming,
            Err(reason) => {
                return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("discarding malformed notification: {reason}"),
                }));
            }
        };
        let notifier = ctx
            .desktop
            .notifier
            .as_ref()
            .ok_or(PluginError::UnsupportedDesktop("notifications"))?;
        match incoming {
            Incoming::Post(notification) => notifier
                .notify(&notification)
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?,
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

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn dispatch(
        plugin: &mut NotificationsPlugin,
        mock: &MockDesktop,
        packet: &Packet,
    ) -> PluginOutput {
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    fn post() -> Packet {
        Packet::from_body(
            types::NOTIFICATION,
            &json!({
                "id": "0|com.example|1|null|1000",
                "appName": "Messages",
                "title": "Ada",
                "text": "See you at six",
            }),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn new_notification_reaches_the_notifier() {
        let mock = MockDesktop::new();
        let output = dispatch(&mut NotificationsPlugin::new(), &mock, &post()).await;
        let mut expected = Notification::new("Messages", "Ada", "See you at six");
        expected.id = Some("0|com.example|1|null|1000".to_string());
        assert_eq!(mock.notifications(), vec![expected]);
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn cancel_closes_the_notification() {
        let mock = MockDesktop::new();
        let mut plugin = NotificationsPlugin::new();
        dispatch(&mut plugin, &mock, &post()).await;
        assert_eq!(mock.notifications().len(), 1);

        let cancel = Packet::from_body(
            types::NOTIFICATION,
            &json!({"isCancel": true, "id": "0|com.example|1|null|1000"}),
        )
        .unwrap();
        dispatch(&mut plugin, &mock, &cancel).await;
        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn request_action_and_reply_are_logged_as_unsupported() {
        let mock = MockDesktop::new();
        let mut plugin = NotificationsPlugin::new();
        let packets = [
            Packet::from_body(types::NOTIFICATION_REQUEST, &json!({"request": true})).unwrap(),
            Packet::from_body(
                types::NOTIFICATION_ACTION,
                &json!({"key": "0|com.example|1|null|1000", "action": "Reply"}),
            )
            .unwrap(),
            Packet::from_body(
                types::NOTIFICATION_REPLY,
                &json!({"requestReplyId": "reply-1", "message": "On my way"}),
            )
            .unwrap(),
        ];
        for packet in packets {
            let output = dispatch(&mut plugin, &mock, &packet).await;
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
        let packet = Packet::from_body(
            types::NOTIFICATION,
            &json!({"appName": "Messages", "title": "Ada"}),
        )
        .unwrap();
        let output = dispatch(&mut NotificationsPlugin::new(), &mock, &packet).await;
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
        let packet = Packet::empty(types::PING).unwrap();
        let output = dispatch(&mut NotificationsPlugin::new(), &mock, &packet).await;
        assert!(output.events.is_empty());
        assert!(mock.notifications().is_empty());
    }
}
