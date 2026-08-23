//! Share plugin (`share`).
//!
//! Classifies inbound [`TYPE_SHARE`](handfast_protocol::TYPE_SHARE) requests
//! and queues them for the daemon:
//!
//! * `{"url": "..."}` — peer asked this device to open a URL;
//! * `{"text": "..."}` — peer shared raw text (surfaced on the clipboard in
//!   Phase 3);
//! * `{"filename": "...", "fileSize": N}` — metadata announcing an upcoming
//!   file transfer; the raw-TLS byte stream itself lands in Phase 3.
//!
//! Recognized bodies are queued as [`ShareItem`] values in arrival order and
//! drained via [`SharePlugin::take_pending`]. Handling never emits reply
//! packets; malformed bodies and foreign types are logged and dropped.

use serde_json::{json, Map, Value};

use handfast_protocol::{Packet, TYPE_SHARE};

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// One queued inbound share, classified by body shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareItem {
    /// URL the peer asked this device to open.
    Url(String),
    /// Raw text the peer sent over.
    Text(String),
    /// Metadata for an announced file transfer (bytes stream out-of-band).
    FileMeta {
        /// Base name of the incoming file.
        name: String,
        /// Announced size in bytes.
        size: u64,
    },
}

impl ShareItem {
    /// Stable lowercase kind tag used in log fields.
    fn kind(&self) -> &'static str {
        match self {
            Self::Url(_) => "url",
            Self::Text(_) => "text",
            Self::FileMeta { .. } => "file_meta",
        }
    }
}

/// Queues inbound `kdeconnect.share.request` payloads for the daemon.
#[derive(Debug)]
pub struct SharePlugin {
    pending_shares: Vec<ShareItem>,
}

impl SharePlugin {
    /// Create a plugin instance with an empty pending-share queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending_shares: Vec::new(),
        }
    }

    /// Drains every share queued since the previous call, oldest first.
    #[must_use]
    pub fn take_pending(&mut self) -> Vec<ShareItem> {
        std::mem::take(&mut self.pending_shares)
    }

    /// Classifies one request body, if it matches a known share shape.
    ///
    /// Precedence mirrors upstream when several recognized fields appear at
    /// once: `url`, then `text`, then file metadata. Empty strings do not
    /// count; `fileSize` must be a JSON integer.
    fn classify(body: &Value) -> Option<ShareItem> {
        let obj = body.as_object()?;
        if let Some(url) = str_field(obj, "url") {
            return Some(ShareItem::Url(url.to_string()));
        }
        if let Some(text) = str_field(obj, "text") {
            return Some(ShareItem::Text(text.to_string()));
        }
        let name = str_field(obj, "filename")?;
        let size = obj.get("fileSize").and_then(Value::as_u64)?;
        Some(ShareItem::FileMeta {
            name: name.to_string(),
            size,
        })
    }
}

/// Reads a non-empty string field from a decoded body object.
fn str_field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

impl Default for SharePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SharePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::SHARE
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        if pkt.ty() != TYPE_SHARE {
            tracing::debug!(
                plugin = meta::SHARE.name,
                got = pkt.ty(),
                "ignoring foreign packet"
            );
            return Vec::new();
        }
        match Self::classify(&pkt.body) {
            Some(item) => {
                tracing::info!(
                    plugin = meta::SHARE.name,
                    kind = item.kind(),
                    "queued inbound share"
                );
                self.pending_shares.push(item);
            }
            None => tracing::debug!(
                plugin = meta::SHARE.name,
                "dropping unrecognized share request body"
            ),
        }
        Vec::new()
    }
}

/// Builds an outbound packet asking the peer to open `url`.
#[must_use]
pub fn send_url(url: &str) -> Packet {
    Packet::new(TYPE_SHARE, json!({ "url": url }))
}

/// Builds an outbound packet delivering raw `text` to the peer.
#[must_use]
pub fn send_text(text: &str) -> Packet {
    Packet::new(TYPE_SHARE, json!({ "text": text }))
}

/// Builds [`SharePlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShareFactory;

impl PluginFactory for ShareFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::SHARE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SharePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn handled_body(body: Value) -> Vec<ShareItem> {
        let mut plugin = SharePlugin::new();
        let replies = plugin.handle(&Packet::new(TYPE_SHARE, body));
        assert!(replies.is_empty(), "share handling must not emit replies");
        plugin.take_pending()
    }

    #[test]
    fn url_body_queues_url_share() {
        let items = handled_body(json!({"url": "https://example.com/page?q=1"}));
        assert_eq!(
            items,
            vec![ShareItem::Url("https://example.com/page?q=1".to_string())]
        );
    }

    #[test]
    fn text_body_queues_text_share() {
        let items = handled_body(json!({"text": "hello from the phone"}));
        assert_eq!(
            items,
            vec![ShareItem::Text("hello from the phone".to_string())]
        );
    }

    #[test]
    fn file_metadata_body_queues_file_meta_and_acknowledges_without_replies() {
        let items = handled_body(json!({"filename": "report.pdf", "fileSize": 123_456_789u64}));
        assert_eq!(
            items,
            vec![ShareItem::FileMeta {
                name: "report.pdf".to_string(),
                size: 123_456_789,
            }]
        );
    }

    #[test]
    fn url_takes_precedence_over_other_recognized_fields() {
        let items = handled_body(json!({"url": "https://a.example", "text": "t"}));
        assert_eq!(items, vec![ShareItem::Url("https://a.example".to_string())]);
        let items = handled_body(json!({
            "text": "t",
            "filename": "f.bin",
            "fileSize": 1
        }));
        assert_eq!(items, vec![ShareItem::Text("t".to_string())]);
    }

    #[test]
    fn shares_queue_in_arrival_order_and_drain_exactly_once() {
        let mut plugin = SharePlugin::new();
        plugin.handle(&Packet::new(TYPE_SHARE, json!({"text": "one"})));
        plugin.handle(&Packet::new(TYPE_SHARE, json!({"url": "https://two"})));
        plugin.handle(&Packet::new(
            TYPE_SHARE,
            json!({"filename": "three.bin", "fileSize": 3}),
        ));
        assert_eq!(
            plugin.take_pending(),
            vec![
                ShareItem::Text("one".to_string()),
                ShareItem::Url("https://two".to_string()),
                ShareItem::FileMeta {
                    name: "three.bin".to_string(),
                    size: 3,
                },
            ]
        );
        assert!(plugin.take_pending().is_empty());
    }

    #[test]
    fn malformed_bodies_are_dropped_without_panicking() {
        let bodies = vec![
            Value::Null,
            json!(0),
            json!(-1),
            json!(1.5),
            json!("https://not-an-object"),
            json!([]),
            json!({}),
            json!({"url": null}),
            json!({"url": 42}),
            json!({"url": ""}),
            json!({"text": ""}),
            json!({"text": true}),
            json!({"filename": "f.bin"}),
            json!({"filename": "", "fileSize": 10}),
            json!({"filename": "f.bin", "fileSize": -7}),
            json!({"filename": "f.bin", "fileSize": 12.5}),
            json!({"filename": "f.bin", "fileSize": "10"}),
            json!({"fileSize": 10}),
        ];
        let mut plugin = SharePlugin::new();
        for body in bodies {
            let replies = plugin.handle(&Packet::new(TYPE_SHARE, body.clone()));
            assert!(replies.is_empty(), "body {body} produced replies");
        }
        assert!(plugin.take_pending().is_empty());
    }

    #[test]
    fn ignores_foreign_packet_types_without_state_change() {
        let mut plugin = SharePlugin::new();
        let bodies = [
            json!({"url": "https://x"}),
            json!({"text": "x"}),
            json!({"filename": "f.bin", "fileSize": 1}),
        ];
        for body in bodies {
            assert!(plugin
                .handle(&Packet::new("kdeconnect.ping", body.clone()))
                .is_empty());
            assert!(plugin
                .handle(&Packet::new("kdeconnect.runcommand.request", body))
                .is_empty());
        }
        assert!(plugin.take_pending().is_empty());
    }

    #[test]
    fn send_url_builds_share_packet_with_url_body() {
        let pkt = send_url("https://example.com/give?a=b");
        assert_eq!(pkt.ty(), TYPE_SHARE);
        assert_eq!(pkt.body, json!({"url": "https://example.com/give?a=b"}));
    }

    #[test]
    fn send_text_builds_share_packet_with_text_body() {
        let pkt = send_text("see attached");
        assert_eq!(pkt.ty(), TYPE_SHARE);
        assert_eq!(pkt.body, json!({"text": "see attached"}));
    }

    #[test]
    fn instances_are_independent_and_start_empty() {
        assert!(SharePlugin::default().take_pending().is_empty());
        let a = ShareFactory.create();
        let b = ShareFactory.create();
        assert_eq!(a.meta().name, "share");
        assert_eq!(b.meta().name, "share");
    }
}
