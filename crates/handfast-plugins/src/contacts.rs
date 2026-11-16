//! Contacts plugin.
//!
//! The phone owns the contact data.

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    Contact, LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

const MAX_CONTACTS: usize = 1024;
const MAX_UID_BYTES: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_FIELD_BYTES: usize = 128;
const MAX_FIELDS: usize = 16;
const MAX_VCARD_BYTES: usize = 16 * 1024;

/// Asks the peer for every contact UID and timestamp.
///
/// # Errors
///
/// Fails when the packet cannot be built.
pub fn request_all_timestamps() -> Result<Packet, ProtoError> {
    Packet::new(types::CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS, json!({}))
}

/// Asks the peer for the vCards with these UIDs.
///
/// # Errors
///
/// Fails when the packet cannot be built.
pub fn request_vcards(uids: &[String]) -> Result<Packet, ProtoError> {
    Packet::new(
        types::CONTACTS_REQUEST_VCARDS_BY_UID,
        json!({ "uids": uids }),
    )
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other @ (',' | ';' | '\\')) => out.push(other),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn unfold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            out.push_str(rest);
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out
}

fn display_name(components: &str) -> String {
    let parts: Vec<&str> = components.split(';').collect();
    let given = parts.get(1).copied().unwrap_or_default().trim();
    let family = parts.first().copied().unwrap_or_default().trim();
    match (given.is_empty(), family.is_empty()) {
        (false, false) => format!("{given} {family}"),
        (false, true) => given.to_string(),
        (true, false) => family.to_string(),
        (true, true) => String::new(),
    }
}

fn parse_vcard(uid: &str, text: &str) -> Option<Contact> {
    if text.len() > MAX_VCARD_BYTES {
        return None;
    }
    let unfolded = unfold(text);
    let mut full_name = None;
    let mut structured_name = None;
    let mut phones = Vec::new();
    let mut emails = Vec::new();
    for line in unfolded.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let field = key
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_uppercase();
        match field.as_str() {
            "FN" => full_name = Some(unescape(value).trim().to_string()),
            "N" => structured_name = Some(display_name(value)),
            "TEL" => {
                let number = unescape(value).trim().to_string();
                if valid_text(&number, MAX_FIELD_BYTES) && phones.len() < MAX_FIELDS {
                    phones.push(number);
                }
            }
            "EMAIL" => {
                let email = unescape(value).trim().to_string();
                if valid_text(&email, MAX_FIELD_BYTES) && emails.len() < MAX_FIELDS {
                    emails.push(email);
                }
            }
            _ => {}
        }
    }
    let name = full_name
        .filter(|name| valid_text(name, MAX_NAME_BYTES))
        .or_else(|| structured_name.filter(|name| valid_text(name, MAX_NAME_BYTES)))?;
    Some(Contact {
        uid: uid.to_string(),
        name,
        phones,
        emails,
    })
}

fn parse_uids(body: &Map<String, Value>) -> Vec<String> {
    let Some(uids) = body.get("uids").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut list: Vec<String> = uids
        .keys()
        .filter(|uid| valid_text(uid, MAX_UID_BYTES))
        .take(MAX_CONTACTS)
        .cloned()
        .collect();
    list.sort();
    list
}

fn parse_vcards(body: &Map<String, Value>) -> (Vec<Contact>, usize) {
    let Some(vcards) = body.get("vcards").and_then(Value::as_object) else {
        return (Vec::new(), 0);
    };
    let mut contacts = Vec::new();
    let mut skipped = 0;
    for (uid, value) in vcards {
        if !valid_text(uid, MAX_UID_BYTES) {
            skipped += 1;
            continue;
        }
        let Some(text) = value.as_str() else {
            skipped += 1;
            continue;
        };
        match parse_vcard(uid, text) {
            Some(contact) if contacts.len() < MAX_CONTACTS => contacts.push(contact),
            _ => skipped += 1,
        }
    }
    contacts.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.uid.cmp(&right.uid))
    });
    (contacts, skipped)
}

/// Parses the contact list a phone reports.
#[derive(Debug, Default)]
pub struct ContactsPlugin;

#[async_trait]
impl Plugin for ContactsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CONTACTS
    }

    async fn on_connect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let packet = request_all_timestamps()
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::CONTACTS_RESPONSE_UIDS_TIMESTAMPS => {
                let uids = parse_uids(&packet.body);
                if uids.is_empty() {
                    return Ok(PluginOutput::new().with_event(PluginEvent::Contacts {
                        contacts: Vec::new(),
                    }));
                }
                let packet = request_vcards(&uids)
                    .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
                Ok(PluginOutput::new().with_packet(packet))
            }
            types::CONTACTS_RESPONSE_VCARDS => {
                let (contacts, skipped) = parse_vcards(&packet.body);
                let mut output = PluginOutput::new().with_event(PluginEvent::Contacts { contacts });
                if skipped > 0 {
                    output = output.with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("skipped {skipped} malformed contacts"),
                    });
                }
                Ok(output)
            }
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Creates one contacts plugin per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContactsFactory;

impl PluginFactory for ContactsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CONTACTS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ContactsPlugin)
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

    async fn send(plugin: &mut ContactsPlugin, body: Value) -> PluginOutput {
        let packet = Packet::from_body(types::CONTACTS_RESPONSE_VCARDS, &body).unwrap();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &handfast_desktop::Desktop::default(),
        };
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    #[test]
    fn a_vcard_is_parsed() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Doe;Jane;;;\r\nFN:Jane Doe\r\nTEL;TYPE=CELL:+123\r\nEMAIL:jane@example.com\r\nEND:VCARD\r\n";
        let contact = parse_vcard("uid-1", text).unwrap();
        assert_eq!(contact.name, "Jane Doe");
        assert_eq!(contact.phones, vec!["+123"]);
        assert_eq!(contact.emails, vec!["jane@example.com"]);
    }

    #[test]
    fn a_folded_line_is_joined() {
        let text = "BEGIN:VCARD\nFN:Jane\n  Doe\nTEL:+1\nEND:VCARD\n";
        let contact = parse_vcard("uid-1", text).unwrap();
        assert_eq!(contact.name, "Jane Doe");
    }

    #[test]
    fn a_structured_name_is_a_fallback() {
        let text = "BEGIN:VCARD\nN:Doe;Jane;;;\nEND:VCARD\n";
        let contact = parse_vcard("uid-1", text).unwrap();
        assert_eq!(contact.name, "Jane Doe");
    }

    #[test]
    fn a_nameless_vcard_is_skipped() {
        let text = "BEGIN:VCARD\nTEL:+1\nEND:VCARD\n";
        assert!(parse_vcard("uid-1", text).is_none());
    }

    #[tokio::test]
    async fn a_timestamps_response_requests_the_vcards() {
        let mut plugin = ContactsPlugin;
        let packet = Packet::from_body(
            types::CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
            &json!({ "uids": { "b": 1, "a": 2, "bad\nuid": 3 } }),
        )
        .unwrap();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &handfast_desktop::Desktop::default(),
        };

        let output = plugin.on_packet(&mut ctx, &packet).await.unwrap();

        assert_eq!(output.packets.len(), 1);
        assert_eq!(
            output.packets[0].ptype,
            types::CONTACTS_REQUEST_VCARDS_BY_UID
        );
        assert_eq!(output.packets[0].body["uids"], json!(["a", "b"]));
    }

    #[tokio::test]
    async fn vcards_are_sorted_and_malformed_entries_are_skipped() {
        let mut plugin = ContactsPlugin;
        let output = send(
            &mut plugin,
            json!({
                "vcards": {
                    "2": "BEGIN:VCARD\nFN:Zoe\nEND:VCARD\n",
                    "1": "BEGIN:VCARD\nFN:Alice\nTEL:+1\nEND:VCARD\n",
                    "3": "BEGIN:VCARD\nTEL:+2\nEND:VCARD\n"
                }
            }),
        )
        .await;

        let Some(PluginEvent::Contacts { contacts }) = output.events.first() else {
            panic!("a contacts event: {:?}", output.events);
        };
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].name, "Alice");
        assert_eq!(contacts[1].name, "Zoe");
        assert!(matches!(
            output.events.get(1),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_empty_timestamp_list_clears_the_model() {
        let mut plugin = ContactsPlugin;
        let packet = Packet::from_body(
            types::CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
            &json!({ "uids": {} }),
        )
        .unwrap();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &handfast_desktop::Desktop::default(),
        };

        let output = plugin.on_packet(&mut ctx, &packet).await.unwrap();

        assert!(output.packets.is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Contacts { contacts }] if contacts.is_empty()
        ));
    }

    #[test]
    fn the_request_builders_match_the_protocol() {
        let all = request_all_timestamps().unwrap();
        assert_eq!(all.ptype, types::CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS);
        let uids = request_vcards(&["a".to_string()]).unwrap();
        assert_eq!(uids.ptype, types::CONTACTS_REQUEST_VCARDS_BY_UID);
        assert_eq!(uids.body["uids"], json!(["a"]));
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(&roster::CONTACTS, ContactsPlugin.meta());
        assert_eq!(&roster::CONTACTS, ContactsFactory.meta());
        assert_eq!(
            roster::CONTACTS.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
