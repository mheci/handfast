//! Clipboard plugin.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1_000;

/// Larger selections are skipped so a clipboard push cannot crowd the link.
const MAX_OUTBOUND_BYTES: usize = 256 * 1024;

fn log_event(level: LogLevel, message: impl Into<String>) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level,
        message: message.into(),
    })
}

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

/// Applies inbound clipboard text and pushes local changes to the peer.
#[derive(Debug, Default)]
pub struct ClipboardPlugin {
    last_timestamp_ms: i64,
    last_text: Option<String>,
    warned: bool,
}

impl ClipboardPlugin {
    /// Create a clipboard plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_timestamp_ms: 0,
            last_text: None,
            warned: false,
        }
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
        let text = match clipboard.read_text().await {
            Ok(Some(text)) => text,
            Ok(None) => {
                self.last_text = None;
                return Ok(PluginOutput::new());
            }
            Err(error) => {
                return Ok(log_event(
                    LogLevel::Warn,
                    format!("failed to read the clipboard: {error}"),
                ));
            }
        };
        self.last_text = Some(text.clone());
        if text.is_empty() || text.len() > MAX_OUTBOUND_BYTES {
            return Ok(PluginOutput::new());
        }
        let Some(timestamp) = now_ms() else {
            return Ok(PluginOutput::new());
        };
        let packet = clipboard_connect_packet(&text, timestamp)
            .map_err(|error| PluginError::Backend(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_tick(&mut self, ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        let Some(clipboard) = ctx.desktop.clipboard.as_ref() else {
            return Ok(PluginOutput::new());
        };
        let text = match clipboard.read_text().await {
            Ok(text) => {
                self.warned = false;
                text
            }
            Err(error) => {
                if self.warned {
                    return Ok(PluginOutput::new());
                }
                self.warned = true;
                return Ok(log_event(
                    LogLevel::Warn,
                    format!("failed to read the clipboard: {error}"),
                ));
            }
        };
        let Some(text) = text else {
            self.last_text = None;
            return Ok(PluginOutput::new());
        };
        if self.last_text.as_deref() == Some(text.as_str()) {
            return Ok(PluginOutput::new());
        }
        self.last_text = Some(text.clone());
        if text.is_empty() {
            return Ok(PluginOutput::new());
        }
        if text.len() > MAX_OUTBOUND_BYTES {
            return Ok(log_event(
                LogLevel::Warn,
                format!(
                    "clipboard selection is {} bytes; the limit is {MAX_OUTBOUND_BYTES}",
                    text.len()
                ),
            ));
        }
        let packet =
            clipboard_packet(&text).map_err(|error| PluginError::Backend(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
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
        self.last_text = Some(content.to_string());

        if let Some(value) = timestamp {
            self.last_timestamp_ms = self.last_timestamp_ms.max(value);
        }

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
    use handfast_runtime::Device;

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
        };
        let packet = clipboard_packet("x").unwrap();
        assert!(ClipboardPlugin::new()
            .on_packet(&mut ctx, &packet)
            .await
            .is_err());
    }

    #[test]
    fn helpers_build_expected_packets() {
        let packet = clipboard_packet("x").unwrap();
        assert_eq!(packet.ptype, types::CLIPBOARD);
        let packet = clipboard_connect_packet("y", 42).unwrap();
        assert_eq!(packet.ptype, types::CLIPBOARD_CONNECT);
        assert_eq!(packet.body["timestamp"], json!(42));
    }

    async fn write_clipboard(desktop: &handfast_desktop::Desktop, text: &str) {
        desktop
            .clipboard
            .as_ref()
            .expect("the mock clipboard")
            .write_text(text)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn connect_sends_the_current_clipboard() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        write_clipboard(&desktop, "hello").await;
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = ClipboardPlugin::new();

        let output = plugin.on_connect(&mut ctx).await.unwrap();

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::CLIPBOARD_CONNECT);
        assert_eq!(output.packets[0].body["content"], json!("hello"));
        assert!(output.packets[0].body["timestamp"].as_i64().unwrap() > 0);
    }

    #[tokio::test]
    async fn tick_sends_a_changed_clipboard_once() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        write_clipboard(&desktop, "before").await;
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = ClipboardPlugin::new();
        plugin.on_connect(&mut ctx).await.unwrap();

        write_clipboard(&desktop, "after").await;
        let output = plugin.on_tick(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::CLIPBOARD);
        assert_eq!(output.packets[0].body["content"], json!("after"));

        let output = plugin.on_tick(&mut ctx).await.unwrap();
        assert!(output.packets.is_empty());
    }

    #[tokio::test]
    async fn inbound_clipboard_is_not_echoed() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = ClipboardPlugin::new();

        plugin
            .on_packet(&mut ctx, &clipboard_packet("copied").unwrap())
            .await
            .unwrap();
        assert_eq!(mock.clipboard().as_deref(), Some("copied"));

        assert!(plugin.on_tick(&mut ctx).await.unwrap().packets.is_empty());
        assert!(plugin.on_tick(&mut ctx).await.unwrap().packets.is_empty());
    }

    #[tokio::test]
    async fn local_change_before_first_tick_after_inbound_is_sent() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = ClipboardPlugin::new();
        plugin
            .on_packet(&mut ctx, &clipboard_packet("remote").unwrap())
            .await
            .unwrap();
        write_clipboard(&desktop, "new local text").await;
        let output = plugin.on_tick(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body["content"], json!("new local text"));
        assert!(plugin.on_tick(&mut ctx).await.unwrap().packets.is_empty());
    }

    #[tokio::test]
    async fn returning_to_previous_text_after_empty_selection_is_sent() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        write_clipboard(&desktop, "original").await;
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = ClipboardPlugin::new();
        plugin.on_connect(&mut ctx).await.unwrap();
        write_clipboard(&desktop, "").await;
        assert!(plugin.on_tick(&mut ctx).await.unwrap().packets.is_empty());
        write_clipboard(&desktop, "original").await;
        let output = plugin.on_tick(&mut ctx).await.unwrap();
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body["content"], json!("original"));
    }

    #[tokio::test]
    async fn a_large_selection_is_not_sent() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        write_clipboard(&desktop, "small").await;
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = ClipboardPlugin::new();
        plugin.on_connect(&mut ctx).await.unwrap();

        write_clipboard(&desktop, &"x".repeat(MAX_OUTBOUND_BYTES + 1)).await;
        let output = plugin.on_tick(&mut ctx).await.unwrap();

        assert!(output.packets.is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                message,
            }] if message.contains("the limit is")
        ));
    }

    #[tokio::test]
    async fn a_missing_clipboard_adapter_is_ignored_by_connect_and_tick() {
        let desktop = handfast_desktop::Desktop::default();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let mut plugin = ClipboardPlugin::new();

        assert!(plugin
            .on_connect(&mut ctx)
            .await
            .unwrap()
            .packets
            .is_empty());
        assert!(plugin.on_tick(&mut ctx).await.unwrap().packets.is_empty());
    }
}
