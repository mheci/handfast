//! Send this desktop's notifications to the phone.
//!
//! The desktop client listens on the freedesktop notification bus and forwards
//! each notification as `kdeconnect.notification`. Handfast cannot listen on
//! another application's bus connection without owning the bus name, so the
//! outbound path is driven by the desktop's own notifier: anything Handfast
//! shows can be mirrored to the phone.
//!
//! Field names follow `dbusnotificationslistener.cpp` exactly: `id`, `appName`,
//! `ticker`, `isClearable`, `title`, `silent`, and an optional `text`. The
//! phone uses `id` to cancel or replace, so ids are namespaced with the device
//! id the same way the inbound notifications plugin does.
//!
//! Filtering is deliberate and off by default. A phone is a very different
//! privacy context from a desktop session, so nothing is forwarded until an
//! allowlist names the application.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Longest accepted field, in bytes. Matches the inbound notifications cap.
const MAX_TEXT_BYTES: usize = 4 * 1024;

/// One notification to mirror to the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingNotification {
    /// Application that raised it, used for filtering and shown on the phone.
    pub app_name: String,
    /// Summary line.
    pub title: String,
    /// Body text, when the notification has one.
    pub text: Option<String>,
    /// Whether the phone should be able to dismiss it.
    pub is_clearable: bool,
}

impl OutgoingNotification {
    /// Build a notification with no body.
    #[must_use]
    pub fn new(app_name: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            title: title.into(),
            text: None,
            is_clearable: true,
        }
    }

    /// Attach a body.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

/// Forwards desktop notifications to the peer's phone.
#[derive(Debug, Default)]
pub struct SendNotificationsPlugin {
    /// Notifications queued for the next tick.
    pending: Vec<OutgoingNotification>,
    /// Next local id, so each forwarded notification can be cancelled.
    next_id: u64,
    /// Applications the peer should see, from the plugin config.
    allowed: Option<std::collections::BTreeSet<String>>,
    /// How many notifications were dropped by the filter, for the log line.
    filtered: usize,
}

impl SendNotificationsPlugin {
    /// Create a send notifications plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            next_id: 1,
            allowed: None,
            filtered: 0,
        }
    }

    /// Whether an application's notifications should be forwarded.
    ///
    /// An empty or absent allowlist forwards nothing. That is the safe
    /// direction: a phone in a pocket should not receive a desktop's password
    /// manager popups because a config file was never written.
    fn allows(&mut self, ctx: &PluginContext<'_>, app_name: &str) -> bool {
        let allowed = self.allowed.get_or_insert_with(|| {
            ctx.config.get_list(roster::SENDNOTIFICATIONS.name, "apps")
        });
        allowed.contains(app_name)
    }

    /// Queue one notification for the peer.
    ///
    /// Returns whether it was accepted. A rejected notification is counted and
    /// reported on the next tick rather than logged per call, so a chatty
    /// application cannot flood the event channel.
    pub fn notify(&mut self, ctx: &PluginContext<'_>, notification: OutgoingNotification) -> bool {
        if !self.allows(ctx, &notification.app_name) {
            self.filtered = self.filtered.saturating_add(1);
            return false;
        }
        if notification.app_name.len() > MAX_TEXT_BYTES
            || notification.title.len() > MAX_TEXT_BYTES
            || notification
                .text
                .as_ref()
                .is_some_and(|text| text.len() > MAX_TEXT_BYTES)
        {
            self.filtered = self.filtered.saturating_add(1);
            return false;
        }
        self.pending.push(notification);
        true
    }

    /// Number of queued notifications.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Render one notification as the packet upstream sends.
    fn packet(&mut self, device_id: &str, notification: &OutgoingNotification) -> Option<Packet> {
        let id = format!("{device_id}|{}", self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let mut body = Map::new();
        body.insert("id".to_string(), Value::String(id));
        body.insert("appName".to_string(), json!(notification.app_name));
        let mut ticker = notification.title.clone();
        if let Some(text) = &notification.text {
            if !text.is_empty() {
                ticker.push_str(": ");
                ticker.push_str(text);
            }
        }
        body.insert("ticker".to_string(), json!(ticker));
        body.insert("isClearable".to_string(), json!(notification.is_clearable));
        body.insert("title".to_string(), json!(notification.title));
        body.insert("silent".to_string(), json!(false));
        if let Some(text) = &notification.text {
            if !text.is_empty() {
                body.insert("text".to_string(), json!(text));
            }
        }
        Packet::new(types::NOTIFICATION, Value::Object(body)).ok()
    }
}

#[async_trait]
impl Plugin for SendNotificationsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SENDNOTIFICATIONS
    }

    async fn on_tick(&mut self, ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        let filtered = std::mem::take(&mut self.filtered);
        let mut output = PluginOutput::new();
        if filtered > 0 {
            output = output.with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "dropped {filtered} notification(s) not named in the \
                     sendnotifications allowlist"
                ),
            });
        }
        if self.pending.is_empty() {
            return Ok(output);
        }
        let notifications = std::mem::take(&mut self.pending);
        let device_id = ctx.device.id.to_string();
        for notification in &notifications {
            if let Some(packet) = self.packet(&device_id, notification) {
                output.packets.push(packet);
            }
        }
        Ok(output)
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::NOTIFICATION_REQUEST {
            return Ok(PluginOutput::new());
        }
        // The phone asks the desktop to resend its active list. Handfast has no
        // persistent view of what it forwarded, so it answers with a note
        // rather than pretending to have one.
        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message: "peer requested the notification list; Handfast forwards live \
                      notifications only"
                .to_string(),
        }))
    }
}

/// Builds [`SendNotificationsPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendNotificationsFactory;

impl PluginFactory for SendNotificationsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SENDNOTIFICATIONS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SendNotificationsPlugin::new())
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

    struct Harness {
        plugin: SendNotificationsPlugin,
        desktop: handfast_desktop::Desktop,
        device: Device,
        config: PluginConfig,
    }

    impl Harness {
        fn new(config: PluginConfig) -> Self {
            Self {
                plugin: SendNotificationsPlugin::new(),
                desktop: MockDesktop::new().desktop(),
                device: device(),
                config,
            }
        }

        fn ctx(&self) -> PluginContext<'_> {
            PluginContext {
                device: &self.device,
                desktop: &self.desktop,
                config: &self.config,
            }
        }

        async fn tick(&mut self) -> PluginOutput {
            self.plugin.on_tick(&mut self.ctx()).await.unwrap()
        }
    }

    fn configured(apps: &str) -> PluginConfig {
        let mut config = PluginConfig::new();
        config.insert("sendnotifications", "apps", apps);
        config
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::SENDNOTIFICATIONS.status, PluginStatus::Implemented);
        assert_eq!(
            roster::SENDNOTIFICATIONS.incoming,
            &[types::NOTIFICATION_REQUEST]
        );
        assert_eq!(roster::SENDNOTIFICATIONS.outgoing, &[types::NOTIFICATION]);
    }

    #[test]
    fn nothing_is_forwarded_without_an_allowlist() {
        let mut harness = Harness::new(PluginConfig::new());
        let accepted = {
            let ctx = harness.ctx();
            harness
                .plugin
                .notify(&ctx, OutgoingNotification::new("firefox", "Hello"))
        };
        assert!(!accepted, "an empty allowlist must forward nothing");
        assert_eq!(harness.plugin.pending(), 0);
    }

    #[tokio::test]
    async fn an_allowed_application_is_forwarded_with_the_upstream_fields() {
        let mut harness = Harness::new(configured("firefox,kmail"));
        let accepted = {
            let ctx = harness.ctx();
            harness.plugin.notify(
                &ctx,
                OutgoingNotification::new("firefox", "Download done").with_text("report.pdf"),
            )
        };
        assert!(accepted);

        let output = harness.tick().await;
        assert_eq!(output.packets.len(), 1);
        let body = &output.packets[0].body;
        assert_eq!(output.packets[0].ptype, types::NOTIFICATION);
        assert_eq!(body.get("appName"), Some(&json!("firefox")));
        assert_eq!(body.get("title"), Some(&json!("Download done")));
        assert_eq!(body.get("text"), Some(&json!("report.pdf")));
        // ticker is "title: text" upstream.
        assert_eq!(body.get("ticker"), Some(&json!("Download done: report.pdf")));
        assert_eq!(body.get("silent"), Some(&json!(false)));
        assert_eq!(body.get("isClearable"), Some(&json!(true)));
        let id = body.get("id").and_then(Value::as_str).unwrap();
        assert!(id.starts_with(&harness.device.id.to_string()));
    }

    #[tokio::test]
    async fn a_disallowed_application_is_counted_and_reported_once() {
        let mut harness = Harness::new(configured("firefox"));
        {
            let ctx = harness.ctx();
            assert!(!harness
                .plugin
                .notify(&ctx, OutgoingNotification::new("keepassxc", "Unlock")));
            assert!(!harness
                .plugin
                .notify(&ctx, OutgoingNotification::new("keepassxc", "Again")));
        }

        let output = harness.tick().await;
        assert!(output.packets.is_empty());
        assert_eq!(output.events.len(), 1);
        // Cleared after reporting, so it does not repeat forever.
        assert!(harness.tick().await.events.is_empty());
    }

    #[tokio::test]
    async fn a_notification_without_a_body_omits_text() {
        let mut harness = Harness::new(configured("firefox"));
        {
            let ctx = harness.ctx();
            harness
                .plugin
                .notify(&ctx, OutgoingNotification::new("firefox", "Just a title"));
        }
        let output = harness.tick().await;
        let body = &output.packets[0].body;
        assert!(!body.contains_key("text"));
        assert_eq!(body.get("ticker"), Some(&json!("Just a title")));
    }

    #[tokio::test]
    async fn ids_increase_so_the_phone_can_tell_notifications_apart() {
        let mut harness = Harness::new(configured("firefox"));
        for title in ["one", "two"] {
            let ctx = harness.ctx();
            harness
                .plugin
                .notify(&ctx, OutgoingNotification::new("firefox", title));
        }
        let output = harness.tick().await;
        let first = output.packets[0].body.get("id").unwrap();
        let second = output.packets[1].body.get("id").unwrap();
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn an_oversized_field_is_rejected() {
        let mut harness = Harness::new(configured("firefox"));
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        let accepted = {
            let ctx = harness.ctx();
            harness
                .plugin
                .notify(&ctx, OutgoingNotification::new("firefox", long))
        };
        assert!(!accepted);
        assert_eq!(harness.plugin.pending(), 0);
    }

    #[tokio::test]
    async fn a_list_request_is_acknowledged_in_the_log() {
        let mut harness = Harness::new(configured("firefox"));
        let packet = Packet::from_body(types::NOTIFICATION_REQUEST, &json!({ "request": true }))
            .unwrap();
        let output = harness
            .plugin
            .on_packet(&mut harness.ctx(), &packet)
            .await
            .unwrap();
        assert!(output.packets.is_empty());
        assert_eq!(output.events.len(), 1);
    }
}
