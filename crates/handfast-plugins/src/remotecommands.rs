//! Remote commands plugin.
//!
//! The desktop keeps the command list a peer reports and sends list, run,
//! setup and stop requests back. Commands always execute on the peer; this
//! plugin never runs anything locally.

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput, RemoteCommand,
};
use serde_json::{json, Value};

use crate::roster;

const MAX_COMMANDS: usize = 64;
const MAX_KEY_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 1024;

/// Whether `key` is a command key a peer may use in a request.
#[must_use]
pub fn valid_command_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_BYTES && !key.chars().any(char::is_control)
}

fn valid_command_text(text: &str) -> bool {
    text.len() <= MAX_TEXT_BYTES && !text.chars().any(char::is_control)
}

fn parse_command(key: &str, entry: &Value) -> Option<RemoteCommand> {
    let entry = entry.as_object()?;
    let name = entry.get("name")?.as_str()?;
    let command = entry.get("command")?.as_str()?;
    if !valid_command_key(key) || !valid_command_text(name) || !valid_command_text(command) {
        return None;
    }
    Some(RemoteCommand {
        key: key.to_string(),
        name: name.to_string(),
        command: command.to_string(),
    })
}

/// Parses the `commandList` string into commands, with the skipped-entry count.
fn parse_list(text: &str) -> Result<(Vec<RemoteCommand>, usize), String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|_| "remote command list is not valid JSON".to_string())?;
    let Some(entries) = value.as_object() else {
        return Err("remote command list is not an object".to_string());
    };
    let mut commands = Vec::new();
    let mut skipped = 0usize;
    for (key, entry) in entries {
        if commands.len() >= MAX_COMMANDS {
            skipped += 1;
            continue;
        }
        match parse_command(key, entry) {
            Some(command) => commands.push(command),
            None => skipped += 1,
        }
    }
    Ok((commands, skipped))
}

fn log(level: LogLevel, message: impl Into<String>) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level,
        message: message.into(),
    })
}

/// Parses the command list a peer reports.
#[derive(Debug, Default)]
pub struct RemoteCommandsPlugin {}

impl RemoteCommandsPlugin {
    fn command_list(&mut self, packet: &Packet) -> PluginOutput {
        let Some(text) = packet.body.get("commandList").and_then(Value::as_str) else {
            return log(
                LogLevel::Warn,
                "remote command packet without a command list",
            );
        };
        let can_add = packet
            .body
            .get("canAddCommand")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match parse_list(text) {
            Ok((commands, skipped)) => {
                let mut output = PluginOutput::new()
                    .with_event(PluginEvent::RemoteCommands { commands, can_add });
                if skipped > 0 {
                    output = output.with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("skipped {skipped} invalid remote command entries"),
                    });
                }
                output
            }
            Err(message) => log(LogLevel::Warn, message),
        }
    }
}

#[async_trait]
impl Plugin for RemoteCommandsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECOMMANDS
    }

    async fn on_connect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        let packet = request_command_list()
            .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
        Ok(PluginOutput::new().with_packet(packet))
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::RUNCOMMAND {
            return Ok(PluginOutput::new());
        }
        Ok(self.command_list(packet))
    }
}

/// Creates one command reader per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoteCommandsFactory;

impl PluginFactory for RemoteCommandsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::REMOTECOMMANDS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RemoteCommandsPlugin::default())
    }
}

/// Build a request for the peer's command list.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn request_command_list() -> Result<Packet, ProtoError> {
    Packet::new(
        types::RUNCOMMAND_REQUEST,
        json!({ "requestCommandList": true }),
    )
}

/// Build a request that runs one command on the peer.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn run_command(key: &str) -> Result<Packet, ProtoError> {
    Packet::new(types::RUNCOMMAND_REQUEST, json!({ "key": key }))
}

/// Build a request that opens the peer's command editor.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn setup_request() -> Result<Packet, ProtoError> {
    Packet::new(types::RUNCOMMAND_REQUEST, json!({ "setup": true }))
}

/// Build a request that closes the peer's command editor.
///
/// # Errors
///
/// Fails when the packet type is malformed.
pub fn stop_request() -> Result<Packet, ProtoError> {
    Packet::new(types::RUNCOMMAND_REQUEST, json!({ "stop": true }))
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

    async fn run(plugin: &mut RemoteCommandsPlugin, packet: Packet) -> PluginOutput {
        let device = device();
        let desktop = handfast_desktop::Desktop::default();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    async fn connect(plugin: &mut RemoteCommandsPlugin) -> PluginOutput {
        let device = device();
        let desktop = handfast_desktop::Desktop::default();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        plugin.on_connect(&mut ctx).await.unwrap()
    }

    fn packet(body: Value) -> Packet {
        Packet::from_body(types::RUNCOMMAND, &body).unwrap()
    }

    fn commands(output: &PluginOutput) -> &[RemoteCommand] {
        output
            .events
            .iter()
            .find_map(|event| match event {
                PluginEvent::RemoteCommands { commands, .. } => Some(commands.as_slice()),
                _ => None,
            })
            .expect("a command event")
    }

    fn can_add(output: &PluginOutput) -> bool {
        output
            .events
            .iter()
            .find_map(|event| match event {
                PluginEvent::RemoteCommands { can_add, .. } => Some(*can_add),
                _ => None,
            })
            .expect("a command event")
    }

    fn list_body(entries: Value, can_add: bool) -> Value {
        json!({
            "commandList": serde_json::to_string(&entries).expect("serialize the list"),
            "canAddCommand": can_add,
        })
    }

    #[tokio::test]
    async fn connect_requests_the_command_list() {
        let mut plugin = RemoteCommandsPlugin::default();
        let output = connect(&mut plugin).await;

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::RUNCOMMAND_REQUEST);
        assert_eq!(output.packets[0].body["requestCommandList"], true);
    }

    #[tokio::test]
    async fn a_command_list_is_parsed() {
        let mut plugin = RemoteCommandsPlugin::default();
        let output = run(
            &mut plugin,
            packet(list_body(
                json!({
                    "abc": { "name": "List files", "command": "ls -l" },
                    "def": { "name": "Date", "command": "date" },
                }),
                true,
            )),
        )
        .await;

        let commands = commands(&output);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].key, "abc");
        assert_eq!(commands[0].name, "List files");
        assert_eq!(commands[0].command, "ls -l");
        assert_eq!(commands[1].key, "def");
        assert!(can_add(&output));
    }

    #[tokio::test]
    async fn invalid_json_is_a_warning() {
        let mut plugin = RemoteCommandsPlugin::default();
        let output = run(&mut plugin, packet(json!({"commandList": "not json"}))).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_malformed_entry_is_skipped_with_a_warning() {
        let mut plugin = RemoteCommandsPlugin::default();
        let output = run(
            &mut plugin,
            packet(list_body(
                json!({
                    "no-name": { "command": "ls" },
                    "bad\u{0}key": { "name": "Bad", "command": "ls" },
                    "long": { "name": "Bad", "command": "x".repeat(MAX_TEXT_BYTES + 1) },
                    "good": { "name": "List files", "command": "ls -l" },
                }),
                false,
            )),
        )
        .await;

        assert_eq!(commands(&output).len(), 1);
        assert_eq!(commands(&output)[0].key, "good");
        assert!(!can_add(&output));
        assert!(matches!(
            output.events.last(),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_command_list_is_capped() {
        let mut plugin = RemoteCommandsPlugin::default();
        let mut entries = serde_json::Map::new();
        for index in 0..MAX_COMMANDS + 1 {
            entries.insert(
                format!("key-{index}"),
                json!({ "name": "Name", "command": "ls" }),
            );
        }
        let output = run(
            &mut plugin,
            packet(list_body(Value::Object(entries), false)),
        )
        .await;

        assert_eq!(commands(&output).len(), MAX_COMMANDS);
        assert!(matches!(
            output.events.last(),
            Some(PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_packet_without_a_command_list_is_a_warning() {
        let mut plugin = RemoteCommandsPlugin::default();
        let output = run(&mut plugin, packet(json!({"canAddCommand": true}))).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[test]
    fn requests_carry_the_fields() {
        let list = request_command_list().unwrap();
        assert_eq!(list.ptype, types::RUNCOMMAND_REQUEST);
        assert_eq!(list.body["requestCommandList"], true);

        let run = run_command("abc").unwrap();
        assert_eq!(run.ptype, types::RUNCOMMAND_REQUEST);
        assert_eq!(run.body["key"], "abc");

        let setup = setup_request().unwrap();
        assert_eq!(setup.ptype, types::RUNCOMMAND_REQUEST);
        assert_eq!(setup.body["setup"], true);

        let stop = stop_request().unwrap();
        assert_eq!(stop.ptype, types::RUNCOMMAND_REQUEST);
        assert_eq!(stop.body["stop"], true);
    }

    #[test]
    fn command_keys_are_validated() {
        assert!(valid_command_key("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!valid_command_key(""));
        assert!(!valid_command_key("bad\nkey"));
        assert!(!valid_command_key(&"a".repeat(MAX_KEY_BYTES + 1)));
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(
            &roster::REMOTECOMMANDS,
            RemoteCommandsPlugin::default().meta()
        );
        assert_eq!(&roster::REMOTECOMMANDS, RemoteCommandsFactory.meta());
        assert_eq!(
            roster::REMOTECOMMANDS.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
