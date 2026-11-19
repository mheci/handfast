//! Ask the phone to run one of its configured commands.
//!
//! The mirror of [`crate::runcommand`], and far less interesting: nothing runs
//! on this host. The phone advertises its own allowlist as
//! `kdeconnect.runcommand` with `commandList`, this plugin caches it so a
//! desktop UI can show the buttons, and a press sends
//! `kdeconnect.runcommand.request {"key": ...}`.
//!
//! Upstream's `remotecommands` reuses the same output packet type, so progress
//! from the phone arrives as `kdeconnect.runcommand.output` with
//! `commandStarted`, `commandOutput` or `commandFinished`. Those are surfaced
//! as log events rather than silently dropped, because "I pressed it and
//! nothing happened" is the worst possible outcome for a remote action.

use std::collections::BTreeMap;

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};

use crate::roster;

/// Longest accepted command key, in bytes.
const MAX_KEY_BYTES: usize = 128;

/// One command the phone offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommand {
    /// Key the phone expects back.
    pub key: String,
    /// Display name the phone chose.
    pub name: String,
}

/// Why a request was rejected before it queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCommandError {
    /// The key is empty, oversized or holds control characters.
    InvalidKey,
}

impl std::fmt::Display for RemoteCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unusable command key")
    }
}

impl std::error::Error for RemoteCommandError {}

/// Parses the phone's `commandList`, which is a JSON document encoded as a
/// string in the packet body.
///
/// Upstream sends `{"key": {"name": "...", "command": "..."}}`. A body that is
/// already an object is accepted too, because peers differ on whether they
/// stringify it.
#[must_use]
pub fn parse_command_list(body: &Value) -> BTreeMap<String, RemoteCommand> {
    let document = match body {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        other => Some(other.clone()),
    };
    let Some(Value::Object(map)) = document else {
        return BTreeMap::new();
    };
    let mut commands = BTreeMap::new();
    for (key, entry) in map {
        if key.is_empty() || key.len() > MAX_KEY_BYTES || key.chars().any(char::is_control) {
            continue;
        }
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or(&key)
            .to_string();
        commands.insert(key.clone(), RemoteCommand { key, name });
    }
    commands
}

/// Caches the phone's command list and sends run requests.
#[derive(Debug, Default)]
pub struct RemoteCommandsPlugin {
    commands: BTreeMap<String, RemoteCommand>,
    pending: Vec<String>,
}

impl RemoteCommandsPlugin {
    /// Create a remote commands plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commands: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    /// Commands the phone offers, sorted by key.
    #[must_use]
    pub fn commands(&self) -> Vec<RemoteCommand> {
        self.commands.values().cloned().collect()
    }

    /// Queue a run request for one key.
    ///
    /// The key is checked against the phone's advertised list, so a stale UI
    /// button cannot send a request the phone will ignore.
    ///
    /// # Errors
    ///
    /// Fails when the key is not one the phone advertised.
    pub fn run(&mut self, key: &str) -> Result<(), RemoteCommandError> {
        if !self.commands.contains_key(key) {
            return Err(RemoteCommandError::InvalidKey);
        }
        self.pending.push(key.to_string());
        Ok(())
    }

    /// Number of queued requests.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[async_trait]
impl Plugin for RemoteCommandsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECOMMANDS
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.pending.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        for key in std::mem::take(&mut self.pending) {
            output = output.with_packet(
                Packet::new(types::RUNCOMMAND_REQUEST, json!({ "key": key }))
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
            types::RUNCOMMAND => {
                let commands =
                    parse_command_list(packet.body.get("commandList").unwrap_or(&Value::Null));
                let count = commands.len();
                self.commands = commands;
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: format!("peer offers {count} command(s)"),
                }))
            }
            types::RUNCOMMAND_OUTPUT => {
                // Surface the peer's progress instead of dropping it: a desktop
                // that asked for a command needs to know it finished.
                let message = if packet.body.contains_key("commandFinished") {
                    let success = packet
                        .body
                        .get("success")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let code = packet
                        .body
                        .get("exitCode")
                        .and_then(Value::as_i64)
                        .unwrap_or(-1);
                    format!("peer command finished: success={success} exit={code}")
                } else if packet.body.contains_key("commandStarted") {
                    "peer command started".to_string()
                } else {
                    "peer command produced output".to_string()
                };
                Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message,
                }))
            }
            _ => Ok(PluginOutput::new()),
        }
    }
}

/// Builds [`RemoteCommandsPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteCommandsFactory;

impl PluginFactory for RemoteCommandsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECOMMANDS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteCommandsPlugin::new())
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

    async fn feed(plugin: &mut RemoteCommandsPlugin, packet: &Packet) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        plugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    async fn tick(plugin: &mut RemoteCommandsPlugin) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config: PluginConfig::empty(),
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    fn list_packet(body: Value) -> Packet {
        Packet::from_body(types::RUNCOMMAND, &body).unwrap()
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::REMOTECOMMANDS.status, PluginStatus::Implemented);
        assert_eq!(
            roster::REMOTECOMMANDS.incoming,
            &[types::RUNCOMMAND, types::RUNCOMMAND_OUTPUT]
        );
        assert_eq!(
            roster::REMOTECOMMANDS.outgoing,
            &[types::RUNCOMMAND_REQUEST]
        );
    }

    #[tokio::test]
    async fn a_stringified_command_list_is_parsed() {
        let mut plugin = RemoteCommandsPlugin::new();
        let list = json!({ "flash": { "name": "Flashlight", "command": "x" } }).to_string();
        feed(&mut plugin, &list_packet(json!({ "commandList": list }))).await;
        let commands = plugin.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].key, "flash");
        assert_eq!(commands[0].name, "Flashlight");
    }

    #[tokio::test]
    async fn an_object_command_list_is_also_accepted() {
        let mut plugin = RemoteCommandsPlugin::new();
        feed(
            &mut plugin,
            &list_packet(json!({ "commandList": { "ring": {"name": "Ring"} } })),
        )
        .await;
        assert_eq!(plugin.commands()[0].name, "Ring");
    }

    #[tokio::test]
    async fn a_nameless_command_falls_back_to_its_key() {
        let mut plugin = RemoteCommandsPlugin::new();
        feed(
            &mut plugin,
            &list_packet(json!({ "commandList": { "beep": {} } })),
        )
        .await;
        assert_eq!(plugin.commands()[0].name, "beep");
    }

    #[tokio::test]
    async fn unusable_keys_are_dropped_from_the_list() {
        let mut plugin = RemoteCommandsPlugin::new();
        feed(
            &mut plugin,
            &list_packet(json!({
                "commandList": { "": {}, "bad\u{7}": {}, "good": {} }
            })),
        )
        .await;
        assert_eq!(plugin.commands().len(), 1);
        assert_eq!(plugin.commands()[0].key, "good");
    }

    #[tokio::test]
    async fn a_known_key_becomes_a_request() {
        let mut plugin = RemoteCommandsPlugin::new();
        feed(
            &mut plugin,
            &list_packet(json!({ "commandList": { "flash": {} } })),
        )
        .await;
        plugin.run("flash").unwrap();
        assert_eq!(plugin.pending(), 1);

        let output = tick(&mut plugin).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::RUNCOMMAND_REQUEST);
        assert_eq!(output.packets[0].body.get("key"), Some(&json!("flash")));
    }

    #[tokio::test]
    async fn an_unknown_key_is_refused_locally() {
        let mut plugin = RemoteCommandsPlugin::new();
        assert!(plugin.run("nope").is_err());
        assert_eq!(plugin.pending(), 0);
    }

    #[tokio::test]
    async fn the_peers_progress_is_surfaced() {
        let mut plugin = RemoteCommandsPlugin::new();
        for body in [
            json!({ "commandStarted": true, "id": 1 }),
            json!({ "commandOutput": true, "stdout": ["a"], "id": 1 }),
            json!({ "commandFinished": true, "success": true, "exitCode": 0, "id": 1 }),
        ] {
            let packet = Packet::from_body(types::RUNCOMMAND_OUTPUT, &body).unwrap();
            let output = feed(&mut plugin, &packet).await;
            assert_eq!(output.events.len(), 1, "body {body} is reported");
        }
    }
}
