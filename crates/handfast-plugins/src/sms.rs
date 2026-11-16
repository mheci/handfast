//! SMS plugin.
//!
//! The phone owns the message store. The desktop requests the conversation
//! list or one thread and can send a message.

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput, SmsMessage,
};
use serde_json::{json, Map, Value};

use crate::roster;

const MAX_MESSAGES: usize = 512;
const MAX_ADDRESS_BYTES: usize = 64;
const MAX_BODY_BYTES: usize = 2000;

/// Whether `address` is an SMS address the daemon may send to.
#[must_use]
pub fn valid_address(address: &str) -> bool {
    !address.is_empty()
        && address.len() <= MAX_ADDRESS_BYTES
        && !address.chars().any(char::is_control)
}

/// Whether `body` is an SMS body the daemon may send.
#[must_use]
pub fn valid_body(body: &str) -> bool {
    body.len() <= MAX_BODY_BYTES
        && !body
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
}

/// Asks the peer for one message per conversation.
///
/// # Errors
///
/// Fails when the packet cannot be built.
pub fn request_conversations() -> Result<Packet, ProtoError> {
    Packet::new(types::SMS_REQUEST_CONVERSATIONS, json!({}))
}

/// Asks the peer for every message in one thread.
///
/// # Errors
///
/// Fails when the packet cannot be built.
pub fn request_conversation(thread_id: i64) -> Result<Packet, ProtoError> {
    Packet::new(
        types::SMS_REQUEST_CONVERSATION,
        json!({ "threadID": thread_id }),
    )
}

/// Sends one SMS to one address.
///
/// # Errors
///
/// Fails when the packet cannot be built.
pub fn send_request(address: &str, body: &str) -> Result<Packet, ProtoError> {
    Packet::new(
        types::SMS_REQUEST,
        json!({
            "sendSms": true,
            "addresses": [{ "address": address }],
            "messageBody": body,
        }),
    )
}

fn parse_address(value: &Value) -> Option<String> {
    let address = value
        .get("address")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())?;
    let address = address.trim();
    valid_address(address).then(|| address.to_string())
}

fn parse_read(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(read)) => *read,
        Some(Value::Number(read)) => read.as_i64().unwrap_or(1) != 0,
        _ => true,
    }
}

fn parse_message(value: &Value) -> Option<SmsMessage> {
    let thread_id = value
        .get("thread_id")
        .and_then(Value::as_i64)
        .filter(|id| *id >= 0)?;
    let address = value
        .get("addresses")
        .and_then(Value::as_array)
        .and_then(|addresses| addresses.iter().find_map(parse_address))
        .or_else(|| {
            value
                .get("phoneNumber")
                .and_then(Value::as_str)
                .and_then(|address| {
                    let address = address.trim();
                    valid_address(address).then(|| address.to_string())
                })
        })?;
    let body = value
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !valid_body(body) {
        return None;
    }
    Some(SmsMessage {
        thread_id,
        address,
        body: body.to_string(),
        date: value.get("date").and_then(Value::as_i64).unwrap_or(0),
        kind: value
            .get("type")
            .and_then(Value::as_i64)
            .and_then(|kind| i32::try_from(kind).ok())
            .unwrap_or(0),
        read: parse_read(value.get("read")),
    })
}

fn parse_messages(body: &Map<String, Value>) -> (Vec<SmsMessage>, usize) {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return (Vec::new(), 0);
    };
    let mut parsed = Vec::new();
    let mut skipped = 0;
    for value in messages {
        match parse_message(value) {
            Some(message) if parsed.len() < MAX_MESSAGES => parsed.push(message),
            _ => skipped += 1,
        }
    }
    parsed.sort_by(|left, right| {
        left.date
            .cmp(&right.date)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    (parsed, skipped)
}

/// Parses the SMS messages a phone reports.
#[derive(Debug, Default)]
pub struct SmsPlugin;

#[async_trait]
impl Plugin for SmsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SMS
    }

    async fn on_connect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let packet = request_conversations()
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::SMS_MESSAGES {
            return Ok(PluginOutput::new());
        }
        let (messages, skipped) = parse_messages(&packet.body);
        let mut output = PluginOutput::new().with_event(PluginEvent::SmsMessages { messages });
        if skipped > 0 {
            output = output.with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: format!("skipped {skipped} malformed SMS messages"),
            });
        }
        Ok(output)
    }
}

/// Creates one SMS plugin per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct SmsFactory;

impl PluginFactory for SmsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SMS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SmsPlugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn send(plugin: &mut SmsPlugin, body: Value) -> PluginOutput {
        let packet = Packet::from_body(types::SMS_MESSAGES, &body).unwrap();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &handfast_desktop::Desktop::default(),
        };
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    #[tokio::test]
    async fn messages_are_parsed_in_date_order() {
        let mut plugin = SmsPlugin;
        let output = send(
            &mut plugin,
            json!({
                "version": 2,
                "messages": [
                    { "thread_id": 2, "addresses": [{ "address": "+2" }], "body": "later", "date": 20, "type": 1, "read": 0 },
                    { "thread_id": 1, "addresses": [{ "address": "+1" }], "body": "earlier", "date": 10, "type": 2, "read": 1 },
                    { "thread_id": 3, "body": "no address", "date": 30 }
                ]
            }),
        )
        .await;

        let Some(PluginEvent::SmsMessages { messages }) = output.events.first() else {
            panic!("an SMS event: {:?}", output.events);
        };
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].body, "earlier");
        assert_eq!(messages[0].kind, 2);
        assert_eq!(messages[1].thread_id, 2);
        assert!(!messages[1].read);
        assert!(matches!(
            output.events.get(1),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_message_with_a_control_character_is_skipped() {
        let mut plugin = SmsPlugin;
        let output = send(
            &mut plugin,
            json!({
                "messages": [
                    { "thread_id": 1, "addresses": [{ "address": "+1" }], "body": "bad\u{0}body", "date": 1 }
                ]
            }),
        )
        .await;

        let Some(PluginEvent::SmsMessages { messages }) = output.events.first() else {
            panic!("an SMS event");
        };
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn a_newline_in_a_body_is_allowed() {
        let mut plugin = SmsPlugin;
        let output = send(
            &mut plugin,
            json!({
                "messages": [
                    { "thread_id": 1, "addresses": [{ "address": "+1" }], "body": "line one\nline two", "date": 1 }
                ]
            }),
        )
        .await;

        let Some(PluginEvent::SmsMessages { messages }) = output.events.first() else {
            panic!("an SMS event");
        };
        assert_eq!(messages[0].body, "line one\nline two");
    }

    #[test]
    fn the_request_builders_match_the_protocol() {
        let conversations = request_conversations().unwrap();
        assert_eq!(conversations.ptype, types::SMS_REQUEST_CONVERSATIONS);
        let thread = request_conversation(7).unwrap();
        assert_eq!(thread.ptype, types::SMS_REQUEST_CONVERSATION);
        assert_eq!(thread.body["threadID"], 7);
        let send = send_request("+123", "hello").unwrap();
        assert_eq!(send.ptype, types::SMS_REQUEST);
        assert_eq!(send.body["sendSms"], true);
        assert_eq!(send.body["addresses"], json!([{ "address": "+123" }]));
        assert_eq!(send.body["messageBody"], "hello");
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(&roster::SMS, SmsPlugin.meta());
        assert_eq!(&roster::SMS, SmsFactory.meta());
        assert_eq!(
            roster::SMS.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
