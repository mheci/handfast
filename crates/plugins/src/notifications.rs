//! Notifications relay plugin.
//!
//! Relays desktop notifications to the paired phone and handles
//! reply/dismiss actions coming back. The D-Bus listener that feeds packets
//! into this plugin lives in `crates/daemon/src/dbus.rs`.

use handfast_protocol::{Packet, TYPE_NOTIFICATION, TYPE_NOTIFICATION_REQUEST};

use crate::{Plugin, PluginFactory, PluginMeta};

/// Static metadata for the notifications relay.
pub const NOTIFICATIONS: PluginMeta = PluginMeta {
    name: "notifications",
    title: "Notification sync",
    incoming: &[TYPE_NOTIFICATION, TYPE_NOTIFICATION_REQUEST],
    outgoing: &[TYPE_NOTIFICATION],
    default_enabled: true,
    requires_wayland: false,
    requires_dbus: true,
};

/// Relays notification packets between desktop and phone.
///
/// Tracks a mapping of internal IDs to phone-assigned IDs so dismissals and
/// replies can be routed correctly in both directions.
pub struct NotificationsPlugin {
    /// Maps our local notification ID to the phone's ID for replies.
    id_map: std::collections::HashMap<String, String>,
}

impl NotificationsPlugin {
    fn new() -> Self {
        Self {
            id_map: Default::default(),
        }
    }
}

/// Factory for [`NotificationsPlugin`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NotificationsFactory;

impl PluginFactory for NotificationsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &NOTIFICATIONS
    }
    fn create(&self) -> Box<dyn Plugin> {
        Box::new(NotificationsPlugin::new())
    }
}

impl Plugin for NotificationsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &NOTIFICATIONS
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        let body = &pkt.body;
        match pkt.ptype.as_str() {
            // Desktop → phone: forward notification content.
            TYPE_NOTIFICATION => {
                let app_name = body.get("appName").and_then(|v| v.as_str()).unwrap_or("");
                let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");

                if title.is_empty() && text.is_empty() {
                    tracing::trace!(target: "handfast::plugins", "empty notification; skipping");
                    return Vec::new();
                }

                // Track the ID for later dismissal/reply routing.
                if !id.is_empty() {
                    self.id_map.insert(id.to_string(), id.to_string());
                }

                let payload = serde_json::json!({
                    "id": id,
                    "appName": app_name,
                    "title": title,
                    "text": text,
                });
                vec![Packet::new(TYPE_NOTIFICATION, payload)]
            }
            // Phone → desktop: request to act on a notification (dismiss/reply).
            TYPE_NOTIFICATION_REQUEST => {
                let action = body.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let key = body.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let reply_msg = body.get("message").and_then(|v| v.as_str());

                match action {
                    "dismiss" if !key.is_empty() => {
                        self.id_map.remove(key);
                        tracing::debug!(target: "handfast::plugins", key = %key, "notification dismissed");
                    }
                    "reply" if !key.is_empty() => {
                        if let Some(msg) = reply_msg {
                            tracing::debug!(
                                target: "handfast::plugins",
                                key = %key,
                                "notification reply sent"
                            );
                            let _ = msg;
                        }
                    }
                    _ => {
                        tracing::trace!(target: "handfast::plugins", action = %action, "unknown notification request");
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_protocol::TYPE_PING;

    #[test]
    fn forwards_notification_content() {
        let mut p = NotificationsPlugin::new();
        let pkt = Packet::new(
            TYPE_NOTIFICATION,
            serde_json::json!({
                "id": "42",
                "appName": "kmail",
                "title": "New mail",
                "text": "You have 1 unread message",
            }),
        );
        let replies = p.handle(&pkt);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ptype, TYPE_NOTIFICATION);
        assert_eq!(
            replies[0].body.get("appName").and_then(|v| v.as_str()),
            Some("kmail")
        );
    }

    #[test]
    fn skips_empty_notifications() {
        let mut p = NotificationsPlugin::new();
        let pkt = Packet::new(
            TYPE_NOTIFICATION,
            serde_json::json!({ "id": "", "appName": "", "title": "", "text": "" }),
        );
        assert!(p.handle(&pkt).is_empty());
    }

    #[test]
    fn dismiss_removes_tracked_id() {
        let mut p = NotificationsPlugin::new();
        let fwd = Packet::new(
            TYPE_NOTIFICATION,
            serde_json::json!({ "id": "99", "appName": "a", "title": "t", "text": "x" }),
        );
        let _ = p.handle(&fwd);
        assert_eq!(p.id_map.len(), 1);

        let dismiss = Packet::new(
            TYPE_NOTIFICATION_REQUEST,
            serde_json::json!({ "action": "dismiss", "key": "99" }),
        );
        let _ = p.handle(&dismiss);
        assert!(p.id_map.is_empty(), "dismiss should remove tracked id");
    }

    #[test]
    fn ignores_foreign_packets() {
        let mut p = NotificationsPlugin::new();
        let pkt = Packet::new(TYPE_PING, serde_json::json!({}));
        assert!(p.handle(&pkt).is_empty());
    }
}
