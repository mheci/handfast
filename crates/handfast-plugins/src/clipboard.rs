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

/// Reject peer timestamps more than five minutes ahead of local time.
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1_000;

fn now_ms() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
}

fn is_out_of_order(timestamp: Option<i64>, last_applied_ms: i64) -> bool {
    let Some(timestamp) = timestamp else {
        return false;
    };
    // A local clock error skips the future check, so a broken clock cannot drop valid packets.
    timestamp <= last_applied_ms
        || now_ms().is_some_and(|now| timestamp > now.saturating_add(MAX_CLOCK_SKEW_MS))
}

/// Applies inbound clipboard text and emits a typed event.
#[derive(Debug, Default)]
pub struct ClipboardPlugin {
    last_timestamp_ms: i64,
}

impl ClipboardPlugin {
    /// Create a clipboard plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_timestamp_ms: 0,
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
        if is_out_of_order(timestamp, self.last_timestamp_ms) {
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
}
