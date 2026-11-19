//! Serve this host's address book to the phone.
//!
//! The protocol is a two-step sync, and both steps exist for a reason:
//!
//! 1. The phone sends `kdeconnect.contacts.request_all_uids_timestamps`.
//!    Handfast answers with every `(uid, timestamp)` pair it has.
//! 2. The phone compares that against its own copy and sends
//!    `kdeconnect.contacts.request_vcards_by_uid` with only the uids it needs.
//!
//! Sending every vCard on the first request would be one packet per contact on
//! every reconnect. The handshake is what keeps a periodic sync cheap.
//!
//! Timestamps are carried in the reply so the phone can decide, not so
//! Handfast can: no incremental protocol is layered on top.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory,
    PluginMeta, PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Most uids honoured from one request.
///
/// A peer asking for ten thousand vCards in one packet is either confused or
/// hostile; either way the answer is a bounded batch.
const MAX_UIDS_PER_REQUEST: usize = 256;

/// Longest accepted uid, in bytes.
const MAX_UID_BYTES: usize = 256;

/// Serves the host address book over the contacts protocol.
#[derive(Debug, Default)]
pub struct ContactsPlugin {
    /// Uids served in total, for the log line and tests.
    served: usize,
}

impl ContactsPlugin {
    /// Create a contacts plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self { served: 0 }
    }

    /// vCards served to the peer so far.
    #[must_use]
    pub const fn served(&self) -> usize {
        self.served
    }

    /// Read the requested uid list out of a request body.
    ///
    /// Upstream sends `{"uids": [...]}`; an object keyed by uid is accepted too
    /// because the phone client has used both shapes. The list is bounded and
    /// filtered here rather than at the adapter, so a hostile request cannot
    /// turn into an unbounded amount of file I/O.
    fn requested_uids(body: &serde_json::Map<String, Value>) -> Vec<String> {
        let valid = |uid: &&str| !uid.is_empty() && uid.len() <= MAX_UID_BYTES;
        let uids: Vec<String> = match body.get("uids") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .filter(valid)
                .map(str::to_string)
                .collect(),
            Some(Value::Object(map)) => map
                .keys()
                .map(String::as_str)
                .filter(valid)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        uids.into_iter().take(MAX_UIDS_PER_REQUEST).collect()
    }
}

#[async_trait]
impl Plugin for ContactsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CONTACTS
    }


    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        let Some(contacts) = &ctx.desktop.contacts else {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "peer asked for contacts but no address book is configured".to_string(),
            }));
        };

        match packet.ptype.as_str() {
            types::CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS => {
                let list = match contacts.list().await {
                    Ok(list) => list,
                    Err(error) => {
                        // A backend failure is reported, not returned: a
                        // `PluginError` disables the plugin for the whole
                        // session, and a temporarily unreadable address book
                        // should not cost the user the plugin until reconnect.
                        return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                            level: LogLevel::Warn,
                            message: format!("address book listing failed: {error}"),
                        }));
                    }
                };
                let timestamps: Map<String, Value> = list
                    .into_iter()
                    .map(|(uid, timestamp)| (uid, json!(timestamp)))
                    .collect();
                let count = timestamps.len();
                let mut body = Map::new();
                body.insert("uids".to_string(), Value::Object(timestamps));
                Ok(PluginOutput::new()
                    .with_packet(
                        Packet::new(
                            types::CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
                            Value::Object(body),
                        )
                        .map_err(|error| PluginError::InvalidPacket(error.to_string()))?,
                    )
                    .with_event(PluginEvent::Log {
                        level: LogLevel::Info,
                        message: format!("offered {count} contact(s) to the peer"),
                    }))
            }
            types::CONTACTS_REQUEST_VCARDS_BY_UID => {
                let uids = ContactsPlugin::requested_uids(&packet.body);
                if uids.is_empty() {
                    return Ok(PluginOutput::new());
                }
                let records = match contacts.vcards(&uids).await {
                    Ok(records) => records,
                    Err(error) => {
                        return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                            level: LogLevel::Warn,
                            message: format!("vCard read failed: {error}"),
                        }));
                    }
                };
                let mut output = PluginOutput::new();
                for record in records {
                    self.served += 1;
                    output = output.with_packet(
                        Packet::new(
                            types::CONTACTS_RESPONSE_VCARDS,
                            json!({ "uid": record.uid, "vcard": record.vcard }),
                        )
                        .map_err(|error| PluginError::InvalidPacket(error.to_string()))?,
                    );
                }
                Ok(output.with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: format!("sent {} vCard(s), {} requested", self.served, uids.len()),
                }))
            }
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Builds [`ContactsPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContactsFactory;

impl PluginFactory for ContactsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CONTACTS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ContactsPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::{ContactRecord, MockDesktop};
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    fn book() -> MockDesktop {
        let mock = MockDesktop::new();
        mock.set_contacts(vec![
            ContactRecord {
                uid: "alice".to_string(),
                timestamp_ms: 1_700_000_000_000,
                vcard: "BEGIN:VCARD\nFN:Alice\nEND:VCARD\n".to_string(),
            },
            ContactRecord {
                uid: "bob".to_string(),
                timestamp_ms: 1_700_000_001_000,
                vcard: "BEGIN:VCARD\nFN:Bob\nEND:VCARD\n".to_string(),
            },
        ]);
        mock
    }

    async fn send(
        plugin: &mut ContactsPlugin,
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
        assert_eq!(roster::CONTACTS.status, PluginStatus::Implemented);
        assert_eq!(
            roster::CONTACTS.incoming,
            &[
                types::CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
                types::CONTACTS_REQUEST_VCARDS_BY_UID
            ]
        );
        assert_eq!(
            roster::CONTACTS.outgoing,
            &[
                types::CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
                types::CONTACTS_RESPONSE_VCARDS
            ]
        );
    }

    #[tokio::test]
    async fn the_uid_handshake_lists_every_contact_with_its_timestamp() {
        let mock = book();
        let mut plugin = ContactsPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
            json!({}),
        )
        .await;

        assert_eq!(output.packets.len(), 1);
        let uids = output.packets[0].body.get("uids").unwrap();
        assert_eq!(uids.get("alice"), Some(&json!(1_700_000_000_000i64)));
        assert_eq!(uids.get("bob"), Some(&json!(1_700_000_001_000i64)));
    }

    #[tokio::test]
    async fn only_the_requested_vcards_are_sent() {
        let mock = book();
        let mut plugin = ContactsPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::CONTACTS_REQUEST_VCARDS_BY_UID,
            json!({ "uids": ["bob"] }),
        )
        .await;

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body.get("uid"), Some(&json!("bob")));
        assert!(output.packets[0]
            .body
            .get("vcard")
            .and_then(Value::as_str)
            .unwrap()
            .contains("FN:Bob"));
        assert_eq!(plugin.served(), 1);
    }

    #[tokio::test]
    async fn an_object_uid_list_is_accepted() {
        let mock = book();
        let mut plugin = ContactsPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::CONTACTS_REQUEST_VCARDS_BY_UID,
            json!({ "uids": { "alice": 1 } }),
        )
        .await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body.get("uid"), Some(&json!("alice")));
    }

    #[tokio::test]
    async fn no_requested_uids_means_no_reply() {
        let mock = book();
        let mut plugin = ContactsPlugin::new();
        let output = send(
            &mut plugin,
            &mock.desktop(),
            types::CONTACTS_REQUEST_VCARDS_BY_UID,
            json!({}),
        )
        .await;
        assert!(output.packets.is_empty());
    }

    #[tokio::test]
    async fn an_unconfigured_address_book_is_reported() {
        // A desktop with no contacts adapter at all.
        let desktop = handfast_desktop::Desktop::default();
        let mut plugin = ContactsPlugin::new();
        let output = send(
            &mut plugin,
            &desktop,
            types::CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
            json!({}),
        )
        .await;
        assert!(output.packets.is_empty());
        assert_eq!(output.events.len(), 1);
    }

    #[test]
    fn the_uid_batch_is_bounded() {
        let uids: Vec<String> = (0..1000).map(|index| format!("uid{index}")).collect();
        let body = json!({ "uids": uids });
        let requested = ContactsPlugin::requested_uids(body.as_object().unwrap());
        assert_eq!(requested.len(), MAX_UIDS_PER_REQUEST);
    }
}
