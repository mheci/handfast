//! Control the phone's system volume from the desktop.
//!
//! This is the mirror of [`crate::systemvolume`]. The phone publishes its sinks
//! as a `sinkList` array inside `kdeconnect.systemvolume`, and accepts
//! `kdeconnect.systemvolume.request` packets carrying `name` plus either
//! `volume` (0-100) or `muted`.
//!
//! The phone owns the sinks, so this plugin keeps no adapter: unlike
//! `systemvolume` it never touches `wpctl`. It caches what the phone reported
//! so a UI can draw the sliders, and forwards user changes back.

use std::collections::BTreeMap;

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

/// Longest accepted sink name, in bytes.
const MAX_NAME_BYTES: usize = 128;

/// What the phone reported about one sink.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RemoteSink {
    /// Human-readable description, when the phone sent one.
    description: Option<String>,
    /// Current volume, 0-100.
    volume: Option<u8>,
    /// Whether the sink is muted.
    muted: Option<bool>,
    /// Whether the phone says the sink can be controlled.
    enabled: Option<bool>,
}

/// Why a volume request was rejected before it queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteVolumeError {
    /// The sink name was empty, oversized, or held control characters.
    InvalidName,
    /// A request carried neither a volume nor a mute change.
    NothingToDo,
}

impl std::fmt::Display for RemoteVolumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName => f.write_str("unusable sink name"),
            Self::NothingToDo => f.write_str("no volume or mute change"),
        }
    }
}

impl std::error::Error for RemoteVolumeError {}

fn valid_name(name: &str) -> Result<(), RemoteVolumeError> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(RemoteVolumeError::InvalidName);
    }
    Ok(())
}

/// A queued change for one remote sink.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outgoing {
    Volume { name: String, volume: u8 },
    Muted { name: String, muted: bool },
}

/// Caches the phone's sinks and forwards volume changes.
#[derive(Debug, Default)]
pub struct RemoteSystemVolumePlugin {
    sinks: BTreeMap<String, RemoteSink>,
    pending: Vec<Outgoing>,
}

impl RemoteSystemVolumePlugin {
    /// Create a remote system volume plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sinks: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    /// Sink names the phone has reported, in sorted order.
    #[must_use]
    pub fn sinks(&self) -> Vec<String> {
        self.sinks.keys().cloned().collect()
    }

    /// Current volume the phone reported for one sink.
    #[must_use]
    pub fn volume(&self, name: &str) -> Option<u8> {
        self.sinks.get(name).and_then(|sink| sink.volume)
    }

    /// Whether the phone reported one sink as muted.
    #[must_use]
    pub fn muted(&self, name: &str) -> Option<bool> {
        self.sinks.get(name).and_then(|sink| sink.muted)
    }

    /// Queue a volume change, 0-100.
    ///
    /// # Errors
    ///
    /// Fails for an unusable sink name.
    pub fn set_volume(&mut self, name: &str, volume: u8) -> Result<(), RemoteVolumeError> {
        valid_name(name)?;
        self.pending.push(Outgoing::Volume {
            name: name.to_string(),
            volume: volume.min(100),
        });
        Ok(())
    }

    /// Queue a mute change.
    ///
    /// # Errors
    ///
    /// Fails for an unusable sink name.
    pub fn set_muted(&mut self, name: &str, muted: bool) -> Result<(), RemoteVolumeError> {
        valid_name(name)?;
        self.pending.push(Outgoing::Muted {
            name: name.to_string(),
            muted,
        });
        Ok(())
    }

    /// Number of queued changes.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// Read a 0-100 value that may arrive as an integer or a float.
fn percentage(value: &Value) -> Option<u8> {
    let number = value.as_f64()?;
    if !number.is_finite() {
        return None;
    }
    Some(number.clamp(0.0, 100.0).round() as u8)
}

#[async_trait]
impl Plugin for RemoteSystemVolumePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTESYSTEMVOLUME
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        for change in std::mem::take(&mut self.pending) {
            let body = match change {
                Outgoing::Volume { name, volume } => json!({ "name": name, "volume": volume }),
                Outgoing::Muted { name, muted } => json!({ "name": name, "muted": muted }),
            };
            output = output.with_packet(
                Packet::new(types::SYSTEMVOLUME_REQUEST, body)
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
        if packet.ptype != types::SYSTEMVOLUME {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();

        if let Some(list) = packet.body.get("sinkList").and_then(Value::as_array) {
            let mut seen = Vec::new();
            for entry in list {
                let Some(name) = entry.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if valid_name(name).is_err() {
                    continue;
                }
                let sink = self.sinks.entry(name.to_string()).or_default();
                if let Some(description) = entry.get("description").and_then(Value::as_str) {
                    sink.description = Some(description.to_string());
                }
                if let Some(volume) = entry.get("volume").and_then(percentage) {
                    sink.volume = Some(volume);
                }
                if let Some(muted) = entry.get("muted").and_then(Value::as_bool) {
                    sink.muted = Some(muted);
                }
                if let Some(enabled) = entry.get("enabled").and_then(Value::as_bool) {
                    sink.enabled = Some(enabled);
                }
                seen.push(name.to_string());
            }
            // A sink list is authoritative: sinks missing from it are gone.
            self.sinks.retain(|name, _| seen.contains(name));
            output = output.with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!("phone reports {} sink(s)", seen.len()),
            });
        }

        // A single-sink update carries `name` at the top level.
        if let Some(name) = packet.body.get("name").and_then(Value::as_str) {
            if valid_name(name).is_ok() {
                let sink = self.sinks.entry(name.to_string()).or_default();
                if let Some(volume) = packet.body.get("volume").and_then(percentage) {
                    sink.volume = Some(volume);
                }
                if let Some(muted) = packet.body.get("muted").and_then(Value::as_bool) {
                    sink.muted = Some(muted);
                }
            }
        }

        Ok(output)
    }
}

/// Builds [`RemoteSystemVolumePlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteSystemVolumeFactory;

impl PluginFactory for RemoteSystemVolumeFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTESYSTEMVOLUME
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteSystemVolumePlugin::new())
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

    fn ctx_parts() -> (handfast_desktop::Desktop, Device) {
        (MockDesktop::new().desktop(), device())
    }

    async fn feed(plugin: &mut RemoteSystemVolumePlugin, body: Value) -> PluginOutput {
        let (desktop, device) = ctx_parts();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        let packet = Packet::from_body(types::SYSTEMVOLUME, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    async fn tick(plugin: &mut RemoteSystemVolumePlugin) -> PluginOutput {
        let (desktop, device) = ctx_parts();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::REMOTESYSTEMVOLUME.status, PluginStatus::Implemented);
        assert_eq!(roster::REMOTESYSTEMVOLUME.incoming, &[types::SYSTEMVOLUME]);
        assert_eq!(
            roster::REMOTESYSTEMVOLUME.outgoing,
            &[types::SYSTEMVOLUME_REQUEST]
        );
    }

    #[tokio::test]
    async fn a_sink_list_is_cached_and_replaces_older_entries() {
        let mut plugin = RemoteSystemVolumePlugin::new();
        feed(
            &mut plugin,
            json!({
                "sinkList": [
                    {"name": "speaker", "volume": 40, "muted": false, "description": "Speakers"},
                    {"name": "headset", "volume": 80.0, "muted": true},
                ]
            }),
        )
        .await;

        assert_eq!(plugin.sinks(), vec!["headset", "speaker"]);
        assert_eq!(plugin.volume("speaker"), Some(40));
        // A float volume rounds into the 0-100 byte.
        assert_eq!(plugin.volume("headset"), Some(80));
        assert_eq!(plugin.muted("headset"), Some(true));

        // A later, shorter list drops the missing sink.
        feed(&mut plugin, json!({ "sinkList": [{"name": "speaker"}] })).await;
        assert_eq!(plugin.sinks(), vec!["speaker"]);
        // The retained sink keeps the volume it last reported.
        assert_eq!(plugin.volume("speaker"), Some(40));
    }

    #[tokio::test]
    async fn a_single_sink_update_merges() {
        let mut plugin = RemoteSystemVolumePlugin::new();
        feed(&mut plugin, json!({ "name": "speaker", "volume": 10 })).await;
        feed(&mut plugin, json!({ "name": "speaker", "muted": true })).await;
        assert_eq!(plugin.volume("speaker"), Some(10));
        assert_eq!(plugin.muted("speaker"), Some(true));
    }

    #[tokio::test]
    async fn unusable_sink_names_are_ignored() {
        let mut plugin = RemoteSystemVolumePlugin::new();
        feed(
            &mut plugin,
            json!({
                "sinkList": [
                    {"name": ""},
                    {"name": "bad\u{7}"},
                    {"name": "x".repeat(MAX_NAME_BYTES + 1)},
                    {"name": "ok"},
                ]
            }),
        )
        .await;
        assert_eq!(plugin.sinks(), vec!["ok"]);
    }

    #[tokio::test]
    async fn changes_become_requests_on_the_next_tick() {
        let mut plugin = RemoteSystemVolumePlugin::new();
        plugin.set_volume("speaker", 150).unwrap();
        plugin.set_muted("headset", true).unwrap();
        assert_eq!(plugin.pending(), 2);

        let output = tick(&mut plugin).await;
        assert_eq!(plugin.pending(), 0);
        assert_eq!(output.packets.len(), 2);
        assert!(output
            .packets
            .iter()
            .all(|packet| packet.ptype == types::SYSTEMVOLUME_REQUEST));
        // Out-of-range volume is clamped to the wire range.
        assert_eq!(output.packets[0].body.get("volume"), Some(&json!(100)));
        assert_eq!(output.packets[0].body.get("name"), Some(&json!("speaker")));
        assert_eq!(output.packets[1].body.get("muted"), Some(&json!(true)));
    }

    #[test]
    fn bad_names_are_rejected_before_they_queue() {
        let mut plugin = RemoteSystemVolumePlugin::new();
        assert_eq!(
            plugin.set_volume("", 10),
            Err(RemoteVolumeError::InvalidName)
        );
        assert_eq!(
            plugin.set_muted("bad\u{7}", true),
            Err(RemoteVolumeError::InvalidName)
        );
        assert_eq!(plugin.pending(), 0);
    }
}
