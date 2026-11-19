//! Clipboard plugin.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput};
use serde_json::{json, Value};

use crate::roster;

const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1_000;

fn now_ms() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
}

fn is_out_of_order(timestamp: Option<i64>, last_applied_ms: i64, now: Option<i64>) -> bool {
    let Some(timestamp) = timestamp else {
        return false;
    };
    if timestamp <= last_applied_ms {
        return true;
    }
    let Some(now) = now else {
        return true;
    };
    timestamp > now.saturating_add(MAX_CLOCK_SKEW_MS)
}

/// Applies inbound clipboard text and emits a typed event.
///
/// Clipboard sync is two-way: a change on either side becomes a packet on the
/// other. To keep the two hosts from bouncing the same text forever, the plugin
/// remembers what it last sent and what it last received and never publishes
/// either. There is no desktop clipboard-change notification here, so the local
/// clipboard is sampled on connect and once per plugin tick.
#[derive(Debug, Default)]
pub struct ClipboardPlugin {
    last_timestamp_ms: i64,
    /// The text most recently published to the peer.
    last_sent: Option<String>,
    /// The text most recently applied from the peer.
    last_applied: Option<String>,
}

impl ClipboardPlugin {
    /// Create a clipboard plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_timestamp_ms: 0,
            last_sent: None,
            last_applied: None,
        }
    }

    /// Whether `text` is worth publishing.
    ///
    /// Empty text is not published: an empty selection is not a clipboard, and
    /// both official clients ignore it. Text that came from the peer, or that
    /// was already sent, is skipped so a link cannot echo.
    fn should_publish(&self, text: Option<&str>) -> bool {
        let Some(text) = text else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        if Some(text) == self.last_sent.as_deref() || Some(text) == self.last_applied.as_deref() {
            return false;
        }
        true
    }

    /// Read the local clipboard and build a publishable packet, if any.
    async fn poll(&mut self, ctx: &mut PluginContext<'_>) -> Result<Option<Packet>, PluginError> {
        let Some(clipboard) = ctx.desktop.clipboard.as_ref() else {
            return Ok(None);
        };
        let text = clipboard
            .read_text()
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        if !self.should_publish(text.as_deref()) {
            return Ok(None);
        }
        let text = text.unwrap_or_default();
        self.last_sent = Some(text.clone());
        Ok(Some(clipboard_packet(&text).map_err(|error| {
            PluginError::InvalidPacket(error.to_string())
        })?))
    }

    /// Last applied peer timestamp in milliseconds.
    #[must_use]
    pub const fn last_timestamp_ms(&self) -> i64 {
        self.last_timestamp_ms
    }
}

#[async_trait]
impl Plugin for ClipboardPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CLIPBOARD
    }

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let Some(clipboard) = ctx.desktop.clipboard.as_ref() else {
            return Ok(PluginOutput::new());
        };
        let text = clipboard
            .read_text()
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return Ok(PluginOutput::new());
        };
        self.last_sent = Some(text.clone());
        let timestamp = now_ms().unwrap_or(0);
        let packet = clipboard_connect_packet(&text, timestamp)
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_tick(&mut self, ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        match self.poll(ctx).await? {
            Some(packet) => Ok(PluginOutput::new().with_packet(packet)),
            None => Ok(PluginOutput::new()),
        }
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if !matches!(
            packet.ptype.as_str(),
            types::CLIPBOARD | types::CLIPBOARD_CONNECT
        ) {
            return Ok(PluginOutput::new());
        }

        let timestamp = packet.body.get("timestamp").and_then(Value::as_i64);
        if is_out_of_order(timestamp, self.last_timestamp_ms, now_ms()) {
            return Ok(PluginOutput::new());
        }

        let Some(content) = packet.body.get("content").and_then(Value::as_str) else {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "clipboard packet without content".to_string(),
            }));
        };

        let Some(clipboard) = ctx.desktop.clipboard.as_ref() else {
            return Err(PluginError::UnsupportedDesktop("clipboard"));
        };
        clipboard
            .write_text(content)
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?;

        if let Some(value) = timestamp {
            self.last_timestamp_ms = self.last_timestamp_ms.max(value);
        }
        // Remember the inbound text so the tick does not send it straight back.
        self.last_applied = Some(content.to_string());

        Ok(PluginOutput::new().with_event(PluginEvent::Clipboard {
            text: content.to_string(),
            timestamp,
        }))
    }
}

/// Builds clipboard plugin instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClipboardFactory;

impl PluginFactory for ClipboardFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::CLIPBOARD
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ClipboardPlugin::new())
    }
}

/// Build a clipboard update packet.
///
/// # Errors
///
/// Fails only if the packet body cannot be constructed.
pub fn clipboard_packet(text: &str) -> Result<Packet, ProtoError> {
    Packet::new(types::CLIPBOARD, json!({ "content": text }))
}

/// Build a clipboard connect packet with a millisecond timestamp.
///
/// # Errors
///
/// Fails only if the packet body cannot be constructed.
pub fn clipboard_connect_packet(text: &str, timestamp_ms: i64) -> Result<Packet, ProtoError> {
    Packet::new(
        types::CLIPBOARD_CONNECT,
        json!({ "content": text, "timestamp": timestamp_ms }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Desktop,
        )
    }

    #[tokio::test]
    async fn inbound_clipboard_is_applied() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let mut plugin = ClipboardPlugin::new();
        let packet = clipboard_packet("copied").unwrap();
        let output = plugin.on_packet(&mut ctx, &packet).await.unwrap();
        assert_eq!(mock.clipboard().as_deref(), Some("copied"));
        assert_eq!(
            output.events,
            vec![PluginEvent::Clipboard {
                text: "copied".to_string(),
                timestamp: None,
            }]
        );
    }

    #[tokio::test]
    async fn older_connect_state_is_ignored() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let mut plugin = ClipboardPlugin::new();

        let newer = clipboard_connect_packet("new", 2_000).unwrap();
        plugin.on_packet(&mut ctx, &newer).await.unwrap();
        let older = clipboard_connect_packet("old", 1_000).unwrap();
        let output = plugin.on_packet(&mut ctx, &older).await.unwrap();

        assert_eq!(mock.clipboard().as_deref(), Some("new"));
        assert!(output.events.is_empty());
        assert_eq!(plugin.last_timestamp_ms(), 2_000);
    }

    #[tokio::test]
    async fn stale_clipboard_packet_is_ignored() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let mut plugin = ClipboardPlugin::new();

        let newer = clipboard_connect_packet("new", 2_000).unwrap();
        plugin.on_packet(&mut ctx, &newer).await.unwrap();
        let stale = Packet::from_body(
            types::CLIPBOARD,
            &json!({"content": "old", "timestamp": 1_000}),
        )
        .unwrap();
        let output = plugin.on_packet(&mut ctx, &stale).await.unwrap();

        assert_eq!(mock.clipboard().as_deref(), Some("new"));
        assert!(output.events.is_empty());
        assert_eq!(plugin.last_timestamp_ms(), 2_000);
    }

    #[tokio::test]
    async fn future_timestamp_is_ignored_and_does_not_poison_later_packets() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let mut plugin = ClipboardPlugin::new();

        let poisoned = clipboard_connect_packet("poison", i64::MAX).unwrap();
        let output = plugin.on_packet(&mut ctx, &poisoned).await.unwrap();
        assert!(output.events.is_empty());
        assert!(mock.clipboard().is_none());
        assert_eq!(plugin.last_timestamp_ms(), 0);

        let later = clipboard_connect_packet("later", 1_000).unwrap();
        plugin.on_packet(&mut ctx, &later).await.unwrap();
        assert_eq!(mock.clipboard().as_deref(), Some("later"));
        assert_eq!(plugin.last_timestamp_ms(), 1_000);
    }

    #[tokio::test]
    async fn missing_clock_rejects_future_timestamps() {
        assert!(is_out_of_order(Some(i64::MAX), 0, None));

        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let mut plugin = ClipboardPlugin::new();

        let poisoned = clipboard_connect_packet("poison", i64::MAX).unwrap();
        let output = plugin.on_packet(&mut ctx, &poisoned).await.unwrap();
        assert!(output.events.is_empty());
        assert!(mock.clipboard().is_none());
        assert_eq!(plugin.last_timestamp_ms(), 0);

        let later = clipboard_connect_packet("later", 1_000).unwrap();
        plugin.on_packet(&mut ctx, &later).await.unwrap();
        assert_eq!(mock.clipboard().as_deref(), Some("later"));
        assert_eq!(plugin.last_timestamp_ms(), 1_000);
    }

    #[tokio::test]
    async fn missing_content_is_logged() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::empty(types::CLIPBOARD).unwrap();
        let output = ClipboardPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .unwrap();
        assert!(mock.clipboard().is_none());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn missing_desktop_adapter_is_an_error() {
        let desktop = handfast_desktop::Desktop::default();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let packet = clipboard_packet("x").unwrap();
        assert!(ClipboardPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn connect_publishes_the_local_clipboard() {
        let mock = MockDesktop::new();
        mock.desktop()
            .clipboard
            .as_ref()
            .unwrap()
            .write_text("local")
            .await
            .unwrap();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let output = ClipboardPlugin::new().on_connect(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::CLIPBOARD_CONNECT);
        assert_eq!(output.packets[0].body["content"], json!("local"));
    }

    #[tokio::test]
    async fn a_local_change_is_published_once() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let mut plugin = ClipboardPlugin::new();

        let quiet = plugin.on_tick(&mut ctx).await.unwrap();
        assert!(quiet.packets.is_empty(), "an empty clipboard is not sent");

        mock.desktop()
            .clipboard
            .as_ref()
            .unwrap()
            .write_text("local")
            .await
            .unwrap();
        let sent = plugin.on_tick(&mut ctx).await.unwrap();
        assert_eq!(sent.packets.len(), 1);
        assert_eq!(sent.packets[0].ptype, types::CLIPBOARD);
        assert_eq!(sent.packets[0].body["content"], json!("local"));

        let repeat = plugin.on_tick(&mut ctx).await.unwrap();
        assert!(
            repeat.packets.is_empty(),
            "an unchanged clipboard is not resent"
        );
    }

    #[tokio::test]
    async fn inbound_text_is_not_echoed_back() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let mut plugin = ClipboardPlugin::new();
        plugin
            .on_packet(&mut ctx, &clipboard_packet("from peer").unwrap())
            .await
            .unwrap();

        let output = plugin.on_tick(&mut ctx).await.unwrap();
        assert!(output.packets.is_empty(), "the applied text is not echoed");
    }

    #[test]
    fn helpers_build_expected_packets() {
        let packet = clipboard_packet("x").unwrap();
        assert_eq!(packet.ptype, types::CLIPBOARD);
        let packet = clipboard_connect_packet("y", 42).unwrap();
        assert_eq!(packet.ptype, types::CLIPBOARD_CONNECT);
        assert_eq!(packet.body["timestamp"], json!(42));
    }
}
