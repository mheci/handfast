//! Telephony plugin.

use async_trait::async_trait;
use handfast_desktop::Notification;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::Value;

use crate::roster;

const MAX_FIELD_BYTES: usize = 4 * 1024;

/// Asks the phone to mute its ringer.
///
/// # Errors
///
/// Fails when the packet cannot be built.
pub fn mute_request() -> Result<Packet, ProtoError> {
    Packet::empty(types::TELEPHONY_REQUEST_MUTE)
}

fn field(packet: &Packet, name: &str) -> Result<Option<String>, ()> {
    match packet.body.get(name) {
        None => Ok(None),
        Some(Value::String(text)) if valid_field(text) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

fn valid_field(text: &str) -> bool {
    text.len() <= MAX_FIELD_BYTES
        && !text
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
}

fn caller(packet: &Packet) -> Result<String, ()> {
    let number = field(packet, "phoneNumber")?.unwrap_or_default();
    let contact = field(packet, "contactName")?.unwrap_or_default();
    Ok(match (contact.is_empty(), number.is_empty()) {
        (false, false) => format!("{contact} ({number})"),
        (false, true) => contact,
        (true, false) => number,
        (true, true) => "Unknown number".to_string(),
    })
}

fn notification_id(packet: &Packet) -> String {
    let number = packet
        .body
        .get("phoneNumber")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("telephony-{number}")
}

fn log(level: LogLevel, message: impl Into<String>) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level,
        message: message.into(),
    })
}

/// Shows incoming, missed and active call state from the phone.
#[derive(Debug, Default)]
pub struct TelephonyPlugin;

#[async_trait]
impl Plugin for TelephonyPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::TELEPHONY
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::TELEPHONY {
            return Ok(PluginOutput::new());
        }
        let Some(notifier) = ctx.desktop.notifier.as_ref() else {
            return Ok(log(
                LogLevel::Warn,
                "telephony packet without a notifications adapter",
            ));
        };
        let id = notification_id(packet);
        if packet
            .body
            .get("isCancel")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            notifier
                .close(&id)
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?;
            return Ok(log(LogLevel::Info, "telephony notification closed"));
        }
        let event = match field(packet, "event") {
            Ok(Some(event)) => event,
            Ok(None) => {
                return Ok(log(LogLevel::Warn, "telephony packet without an event"));
            }
            Err(()) => {
                return Ok(log(
                    LogLevel::Warn,
                    "telephony packet with a malformed event",
                ));
            }
        };
        let text = match caller(packet) {
            Ok(text) => text,
            Err(()) => {
                return Ok(log(
                    LogLevel::Warn,
                    "telephony packet with a malformed caller",
                ));
            }
        };
        let title = match event.as_str() {
            "ringing" => "Incoming call",
            "missedCall" => "Missed call",
            "talking" => "Call in progress",
            "sms" => return Ok(PluginOutput::new()),
            _ => {
                return Ok(log(
                    LogLevel::Warn,
                    format!("telephony event {event:?} is not supported"),
                ));
            }
        };
        let mut notification = Notification::new("Handfast", title, text);
        notification.id = Some(id);
        notifier
            .notify(&notification)
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        Ok(log(LogLevel::Info, format!("telephony {event} notified")))
    }
}

/// Builds telephony plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct TelephonyFactory;

impl PluginFactory for TelephonyFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::TELEPHONY
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(TelephonyPlugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;
    use serde_json::json;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run(desktop: &handfast_desktop::Desktop, packet: &Packet) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        TelephonyPlugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    fn packet(body: serde_json::Value) -> Packet {
        Packet::from_body(types::TELEPHONY, &body).unwrap()
    }

    #[tokio::test]
    async fn ringing_notifies_with_the_contact() {
        let mock = MockDesktop::new();
        let output = run(
            &mock.desktop(),
            &packet(json!({
                "event": "ringing",
                "phoneNumber": "+15550100",
                "contactName": "Ada",
            })),
        )
        .await;

        let notifications = mock.notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].title, "Incoming call");
        assert_eq!(notifications[0].text, "Ada (+15550100)");
        assert_eq!(notifications[0].id.as_deref(), Some("telephony-+15550100"));
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Info,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn missed_call_notifies_with_the_number() {
        let mock = MockDesktop::new();
        run(
            &mock.desktop(),
            &packet(json!({"event": "missedCall", "phoneNumber": "+15550100"})),
        )
        .await;

        assert_eq!(mock.notifications()[0].title, "Missed call");
        assert_eq!(mock.notifications()[0].text, "+15550100");
    }

    #[tokio::test]
    async fn talking_notifies() {
        let mock = MockDesktop::new();
        run(&mock.desktop(), &packet(json!({"event": "talking"}))).await;

        assert_eq!(mock.notifications()[0].title, "Call in progress");
        assert_eq!(mock.notifications()[0].text, "Unknown number");
    }

    #[tokio::test]
    async fn cancel_closes_the_notification() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        run(
            &desktop,
            &packet(json!({"event": "ringing", "phoneNumber": "+15550100"})),
        )
        .await;
        assert_eq!(mock.notifications().len(), 1);

        run(
            &desktop,
            &packet(json!({"event": "ringing", "phoneNumber": "+15550100", "isCancel": true})),
        )
        .await;

        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn sms_event_is_left_to_the_sms_plugin() {
        let mock = MockDesktop::new();
        run(&mock.desktop(), &packet(json!({"event": "sms"}))).await;

        assert!(mock.notifications().is_empty());
    }

    #[tokio::test]
    async fn unknown_event_is_a_warning() {
        let mock = MockDesktop::new();
        let output = run(&mock.desktop(), &packet(json!({"event": "hold"}))).await;

        assert!(mock.notifications().is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn malformed_caller_is_a_warning() {
        let mock = MockDesktop::new();
        let output = run(
            &mock.desktop(),
            &packet(json!({"event": "ringing", "phoneNumber": 42})),
        )
        .await;

        assert!(mock.notifications().is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn missing_notifier_is_a_warning() {
        let desktop = handfast_desktop::Desktop::default();
        let output = run(&desktop, &packet(json!({"event": "ringing"}))).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[test]
    fn a_mute_request_matches_the_protocol() {
        let packet = mute_request().unwrap();
        assert_eq!(packet.ptype, types::TELEPHONY_REQUEST_MUTE);
        assert!(packet.body.is_empty());
    }

    #[test]
    fn meta_matches_roster() {
        assert_eq!(&roster::TELEPHONY, TelephonyPlugin.meta());
        assert_eq!(&roster::TELEPHONY, TelephonyFactory.meta());
        assert_eq!(
            roster::TELEPHONY.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
