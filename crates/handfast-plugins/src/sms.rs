//! Read this host's text messages from the phone.
//!
//! The most request-heavy plugin in the roster: upstream defines four request
//! types and two reply types, and every one of them is mirrored here.
//!
//! | in                                                        | out                              |
//! |-----------------------------------------------------------|----------------------------------|
//! | `kdeconnect.sms.request_conversations`                     | `kdeconnect.sms.messages`        |
//! | `kdeconnect.sms.request_conversation`                      | `kdeconnect.sms.messages`        |
//! | `kdeconnect.sms.request`                                   | `kdeconnect.sms.messages`        |
//! | `kdeconnect.sms.request_attachment`                        | `kdeconnect.sms.attachment_file` |
//!
//! Handfast has no SMS store of its own — a Linux desktop has no messages to
//! read — so this plugin runs in the reverse direction from the phone's: it
//! *requests* conversations and renders whatever comes back. That is the same
//! code path the KDE desktop client uses when its messaging app queries a
//! phone, and it makes the plugin useful without inventing a message database.
//!
//! Attachment requests are bounded. A conversation can reference a hundred
//! attachments, and turning one request into a hundred packets is how a plugin
//! becomes a denial of service against its own transport.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    DesktopRequirement, LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory,
    PluginMeta, PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Most messages accepted in one reply.
const MAX_MESSAGES: usize = 512;

/// Most attachment ids honoured from one request.
const MAX_ATTACHMENTS: usize = 32;

/// Longest accepted message body, in bytes.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// One conversation summary, as a UI needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    /// Thread identifier the phone uses.
    pub thread_id: i64,
    /// Highest message id in the thread, from the phone.
    pub last_message_id: Option<i64>,
    /// Display snippet the phone chose.
    pub snippet: Option<String>,
    /// Number of unread messages, when reported.
    pub unread: Option<i64>,
}

/// One message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Phone-side message id, used for deduplication.
    pub id: Option<i64>,
    /// Thread it belongs to.
    pub thread_id: i64,
    /// Sender or recipient address.
    pub address: Option<String>,
    /// Message body.
    pub body: String,
    /// Milliseconds since the Unix epoch.
    pub date: Option<i64>,
    /// Whether it was sent rather than received.
    pub sent: bool,
    /// Attachment ids referenced by the message.
    pub attachments: Vec<String>,
}

/// Outgoing request queued for the next tick.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    Conversations,
    Conversation { thread_id: i64 },
    Messages { thread_id: Option<i64> },
    Attachment { part_id: String },
}

/// Mirrors the SMS request/reply protocol.
#[derive(Debug, Default)]
pub struct SmsPlugin {
    conversations: Vec<Conversation>,
    messages: Vec<Message>,
    pending: Vec<Request>,
    /// Replies received, for the log and tests.
    replies: usize,
}

impl SmsPlugin {
    /// Create an SMS plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            conversations: Vec::new(),
            messages: Vec::new(),
            pending: Vec::new(),
            replies: 0,
        }
    }

    /// Conversations the phone has reported.
    #[must_use]
    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    /// Messages the phone has reported.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Replies received.
    #[must_use]
    pub const fn replies(&self) -> usize {
        self.replies
    }

    /// Ask the phone for its conversation list.
    pub fn request_conversations(&mut self) {
        self.pending.push(Request::Conversations);
    }

    /// Ask the phone for one conversation's messages.
    pub fn request_conversation(&mut self, thread_id: i64) {
        self.pending.push(Request::Conversation { thread_id });
    }

    /// Ask the phone for all messages, or for one thread when `thread_id` is
    /// given.
    pub fn request_messages(&mut self, thread_id: Option<i64>) {
        self.pending.push(Request::Messages { thread_id });
    }

    /// Ask the phone for one attachment.
    ///
    /// # Errors
    ///
    /// Fails for an empty or oversized part id.
    pub fn request_attachment(&mut self, part_id: &str) -> Result<(), &'static str> {
        if part_id.is_empty() || part_id.len() > 256 {
            return Err("unusable attachment id");
        }
        self.pending.push(Request::Attachment {
            part_id: part_id.to_string(),
        });
        Ok(())
    }

    /// Number of queued requests.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Merge a conversation list into the cache.
    fn merge_conversations(&mut self, body: &Value) {
        let Some(list) = body.get("conversations").and_then(Value::as_array) else {
            return;
        };
        self.conversations = list
            .iter()
            .take(MAX_MESSAGES)
            .filter_map(|entry| {
                Some(Conversation {
                    thread_id: entry.get("thread_id")?.as_i64()?,
                    last_message_id: entry.get("last_message_id").and_then(Value::as_i64),
                    snippet: entry.get("snippet").and_then(Value::as_str).map(str::to_string),
                    unread: entry.get("unread").and_then(Value::as_i64),
                })
            })
            .collect();
    }

    /// Merge a message list into the cache, newest id winning.
    fn merge_messages(&mut self, body: &Value) {
        let Some(list) = body.get("messages").and_then(Value::as_array) else {
            return;
        };
        for entry in list.iter().take(MAX_MESSAGES) {
            let Some(body_text) = entry.get("body").and_then(Value::as_str) else {
                continue;
            };
            let Some(thread_id) = entry.get("thread_id").and_then(Value::as_i64) else {
                continue;
            };
            if body_text.len() > MAX_BODY_BYTES {
                continue;
            }
            let message = Message {
                id: entry.get("_id").and_then(Value::as_i64),
                thread_id,
                address: entry.get("address").and_then(Value::as_str).map(str::to_string),
                body: body_text.to_string(),
                date: entry.get("date").and_then(Value::as_i64),
                sent: entry
                    .get("type")
                    .and_then(Value::as_i64)
                    .is_some_and(|kind| kind == 2),
                attachments: entry
                    .get("attachments")
                    .and_then(Value::as_str)
                    .map(|joined| {
                        joined
                            .split(',')
                            .map(str::trim)
                            .filter(|part| !part.is_empty())
                            .take(MAX_ATTACHMENTS)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            // Replace a message with the same id instead of duplicating it.
            if let Some(index) = self
                .messages
                .iter()
                .position(|known| known.id.is_some() && known.id == message.id)
            {
                self.messages[index] = message;
            } else {
                self.messages.push(message);
            }
        }
        self.messages.truncate(MAX_MESSAGES);
    }
}

#[async_trait]
impl Plugin for SmsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SMS
    }

    fn requirements(&self) -> &'static [DesktopRequirement] {
        // The plugin is a client of the phone's store, not of a local one, so
        // it needs no desktop adapter but does need a peer that speaks SMS.
        &[]
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        for request in std::mem::take(&mut self.pending) {
            let (ptype, body) = match request {
                Request::Conversations => (
                    types::SMS_REQUEST_CONVERSATIONS,
                    Value::Object(Map::new()),
                ),
                Request::Conversation { thread_id } => (
                    types::SMS_REQUEST_CONVERSATION,
                    json!({ "thread_id": thread_id }),
                ),
                Request::Messages { thread_id } => (
                    types::SMS_REQUEST,
                    match thread_id {
                        Some(thread_id) => json!({ "thread_id": thread_id }),
                        None => json!({}),
                    },
                ),
                Request::Attachment { part_id } => {
                    (types::SMS_REQUEST_ATTACHMENT, json!({ "part_id": part_id }))
                }
            };
            output = output.with_packet(
                Packet::new(ptype, body)
                    .map_err(|error| PluginError::InvalidPacket(error.to_string()))?,
            );
        }
        Ok(output)
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::SMS_MESSAGES => {
                self.replies += 1;
                self.merge_conversations(&packet.body);
                self.merge_messages(&packet.body);
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "phone reported {} conversation(s) and {} message(s)",
                        self.conversations.len(),
                        self.messages.len()
                    ),
                }))
            }
            types::SMS_ATTACHMENT_FILE => {
                self.replies += 1;
                let size = packet
                    .body
                    .get("size")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: format!("phone offered an attachment of {size} bytes"),
                }))
            }
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Builds [`SmsPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct SmsFactory;

impl PluginFactory for SmsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SMS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SmsPlugin::new())
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

    async fn tick(plugin: &mut SmsPlugin) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    async fn feed(plugin: &mut SmsPlugin, ptype: &str, body: Value) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(ptype, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    #[test]
    fn the_roster_entry_covers_every_sms_type() {
        assert_eq!(roster::SMS.status, PluginStatus::Implemented);
        for ptype in [
            types::SMS_MESSAGES,
            types::SMS_ATTACHMENT_FILE,
            types::SMS_REQUEST,
            types::SMS_REQUEST_CONVERSATION,
            types::SMS_REQUEST_CONVERSATIONS,
            types::SMS_REQUEST_ATTACHMENT,
        ] {
            assert!(
                roster::SMS.incoming.contains(&ptype) || roster::SMS.outgoing.contains(&ptype),
                "{ptype} is missing from the roster"
            );
        }
    }

    #[tokio::test]
    async fn every_request_kind_becomes_a_packet() {
        let mut plugin = SmsPlugin::new();
        plugin.request_conversations();
        plugin.request_conversation(7);
        plugin.request_messages(Some(7));
        plugin.request_messages(None);
        plugin.request_attachment("part-1").unwrap();
        assert_eq!(plugin.pending(), 5);

        let output = tick(&mut plugin).await;
        assert_eq!(plugin.pending(), 0);
        let types_sent: Vec<&str> = output
            .packets
            .iter()
            .map(|packet| packet.ptype.as_str())
            .collect();
        assert_eq!(
            types_sent,
            vec![
                types::SMS_REQUEST_CONVERSATIONS,
                types::SMS_REQUEST_CONVERSATION,
                types::SMS_REQUEST,
                types::SMS_REQUEST,
                types::SMS_REQUEST_ATTACHMENT,
            ]
        );
        assert_eq!(output.packets[1].body.get("thread_id"), Some(&json!(7)));
        // A global request carries no thread.
        assert!(output.packets[3].body.is_empty());
        assert_eq!(output.packets[4].body.get("part_id"), Some(&json!("part-1")));
    }

    #[tokio::test]
    async fn a_messages_reply_fills_both_caches() {
        let mut plugin = SmsPlugin::new();
        let output = feed(
            &mut plugin,
            types::SMS_MESSAGES,
            json!({
                "conversations": [
                    {"thread_id": 7, "last_message_id": 42, "snippet": "see you", "unread": 2}
                ],
                "messages": [
                    {"_id": 42, "thread_id": 7, "address": "+15551234", "body": "see you",
                     "date": 1_700_000_000_000i64, "type": 1, "attachments": "a,b"},
                    {"_id": 43, "thread_id": 7, "body": "on my way", "type": 2}
                ]
            }),
        )
        .await;

        assert_eq!(output.events.len(), 1);
        assert_eq!(plugin.conversations().len(), 1);
        assert_eq!(plugin.conversations()[0].thread_id, 7);
        assert_eq!(plugin.conversations()[0].unread, Some(2));
        assert_eq!(plugin.messages().len(), 2);
        assert_eq!(plugin.messages()[0].attachments, vec!["a", "b"]);
        // type 2 is an outgoing message.
        assert!(!plugin.messages()[0].sent);
        assert!(plugin.messages()[1].sent);
        assert_eq!(plugin.replies(), 1);
    }

    #[tokio::test]
    async fn a_repeated_message_id_replaces_rather_than_duplicates() {
        let mut plugin = SmsPlugin::new();
        for body_text in ["first", "edited"] {
            feed(
                &mut plugin,
                types::SMS_MESSAGES,
                json!({ "messages": [{"_id": 1, "thread_id": 1, "body": body_text}] }),
            )
            .await;
        }
        assert_eq!(plugin.messages().len(), 1);
        assert_eq!(plugin.messages()[0].body, "edited");
    }

    #[tokio::test]
    async fn incomplete_messages_are_skipped() {
        let mut plugin = SmsPlugin::new();
        feed(
            &mut plugin,
            types::SMS_MESSAGES,
            json!({
                "messages": [
                    {"_id": 1, "thread_id": 1},
                    {"_id": 2, "body": "no thread"},
                    {"_id": 3, "thread_id": 1, "body": "kept"}
                ]
            }),
        )
        .await;
        assert_eq!(plugin.messages().len(), 1);
        assert_eq!(plugin.messages()[0].body, "kept");
    }

    #[tokio::test]
    async fn a_messages_reply_with_a_bad_shape_is_ignored() {
        let mut plugin = SmsPlugin::new();
        feed(&mut plugin, types::SMS_MESSAGES, json!({ "conversations": "nope" })).await;
        assert!(plugin.messages().is_empty());
        assert!(plugin.conversations().is_empty());
    }

    #[tokio::test]
    async fn an_attachment_reply_is_reported() {
        let mut plugin = SmsPlugin::new();
        let output = feed(
            &mut plugin,
            types::SMS_ATTACHMENT_FILE,
            json!({ "filename": "cat.jpg", "size": 2048 }),
        )
        .await;
        assert_eq!(output.events.len(), 1);
        assert_eq!(plugin.replies(), 1);
    }

    #[test]
    fn bad_attachment_ids_are_refused() {
        let mut plugin = SmsPlugin::new();
        assert!(plugin.request_attachment("").is_err());
        assert!(plugin.request_attachment(&"x".repeat(300)).is_err());
        assert_eq!(plugin.pending(), 0);
    }
}
