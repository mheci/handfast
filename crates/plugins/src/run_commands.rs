//! Run-commands plugin (`run_commands`).
//!
//! Keeps a name→command table seeded via
//! [`RunCommandsPlugin::set_commands`] and refreshed whenever the peer
//! broadcasts a `kdeconnect.runcommand` command-list sync of the form
//! `{"commandList": [{"name": "...", "command": "..."}, ...]}` (upstream's
//! extra `key` field is tolerated and ignored).
//!
//! A `kdeconnect.runcommand.request` naming a known command resolves against
//! the table and returns one same-type echo packet carrying both the name
//! and the resolved command. The daemon consumes that echo to perform the
//! actual execution in Phase 3; this plugin never runs anything itself.
//! Unknown names, malformed bodies, and foreign types are logged and dropped.

use std::collections::HashMap;

use serde_json::{json, Value};

use handfast_protocol::{Packet, TYPE_RUNCOMMAND, TYPE_RUNCOMMAND_REQUEST};

use crate::{meta, Plugin, PluginFactory, PluginMeta};

/// Resolves `kdeconnect.runcommand.request` names against a command table.
#[derive(Debug)]
pub struct RunCommandsPlugin {
    commands: HashMap<String, String>,
}

impl RunCommandsPlugin {
    /// Create a plugin instance with an empty command table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Replaces the local name→command table with the provided list.
    ///
    /// Later entries win when the same name appears more than once.
    pub fn set_commands<I>(&mut self, commands: I)
    where
        I: IntoIterator<Item = (String, String)>,
    {
        self.commands = commands.into_iter().collect();
    }

    /// Read-only view of the current name→command table.
    #[must_use]
    pub fn commands(&self) -> &HashMap<String, String> {
        &self.commands
    }

    /// Handles an execution request: resolves `body.name` against the table
    /// and returns the daemon-bound confirmation echo, if any.
    fn handle_request(&mut self, pkt: &Packet) -> Vec<Packet> {
        let Some(name) = pkt
            .body
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            tracing::debug!(
                plugin = meta::RUN_COMMANDS.name,
                "runcommand request without usable \"name\" field"
            );
            return Vec::new();
        };
        let Some(command) = self.commands.get(name) else {
            tracing::warn!(
                plugin = meta::RUN_COMMANDS.name,
                name,
                "runcommand request names unknown command"
            );
            return Vec::new();
        };
        tracing::debug!(
            plugin = meta::RUN_COMMANDS.name,
            name,
            "confirmed runcommand request for daemon execution"
        );
        vec![Packet::new(
            TYPE_RUNCOMMAND_REQUEST,
            json!({ "name": name, "command": command }),
        )]
    }

    /// Handles a peer command-list sync: replaces the local table with the
    /// usable entries from `body.commandList`.
    fn handle_sync(&mut self, pkt: &Packet) -> Vec<Packet> {
        let Some(entries) = pkt.body.get("commandList").and_then(Value::as_array) else {
            tracing::debug!(
                plugin = meta::RUN_COMMANDS.name,
                "runcommand sync without \"commandList\" array"
            );
            return Vec::new();
        };
        let mut synced: HashMap<String, String> = HashMap::new();
        for entry in entries {
            let Some((name, command)) = parse_entry(entry) else {
                tracing::debug!(
                    plugin = meta::RUN_COMMANDS.name,
                    "skipping malformed commandList entry"
                );
                continue;
            };
            synced.insert(name, command);
        }
        if !entries.is_empty() && synced.is_empty() {
            tracing::warn!(
                plugin = meta::RUN_COMMANDS.name,
                "commandList held no usable entries; keeping current table"
            );
            return Vec::new();
        }
        let count = synced.len();
        self.commands = synced;
        tracing::debug!(
            plugin = meta::RUN_COMMANDS.name,
            count,
            "updated local command table from peer sync"
        );
        Vec::new()
    }
}

/// Extracts a usable `(name, command)` pair from one `commandList` entry.
///
/// Entries are objects carrying non-empty string `name` and `command`
/// fields; unknown extra fields are ignored.
fn parse_entry(entry: &Value) -> Option<(String, String)> {
    let obj = entry.as_object()?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    Some((name.to_string(), command.to_string()))
}

impl Default for RunCommandsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for RunCommandsPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &meta::RUN_COMMANDS
    }

    fn handle(&mut self, pkt: &Packet) -> Vec<Packet> {
        match pkt.ty() {
            TYPE_RUNCOMMAND_REQUEST => self.handle_request(pkt),
            TYPE_RUNCOMMAND => self.handle_sync(pkt),
            _ => {
                tracing::debug!(
                    plugin = meta::RUN_COMMANDS.name,
                    got = pkt.ty(),
                    "ignoring foreign packet"
                );
                Vec::new()
            }
        }
    }
}

/// Builds [`RunCommandsPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunCommandsFactory;

impl PluginFactory for RunCommandsFactory {
    fn meta(&self) -> &'static PluginMeta {
        &meta::RUN_COMMANDS
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RunCommandsPlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seeded() -> RunCommandsPlugin {
        let mut plugin = RunCommandsPlugin::new();
        plugin.set_commands([
            ("screenshot".to_string(), "spectacle -b".to_string()),
            ("lock".to_string(), "loginctl lock-session".to_string()),
        ]);
        plugin
    }

    #[test]
    fn set_commands_populates_table() {
        let plugin = seeded();
        assert_eq!(plugin.commands().len(), 2);
        assert_eq!(
            plugin.commands().get("screenshot").map(String::as_str),
            Some("spectacle -b")
        );
        assert_eq!(
            plugin.commands().get("lock").map(String::as_str),
            Some("loginctl lock-session")
        );
    }

    #[test]
    fn set_commands_last_duplicate_name_wins() {
        let mut plugin = RunCommandsPlugin::new();
        plugin.set_commands([
            ("dup".to_string(), "first".to_string()),
            ("dup".to_string(), "second".to_string()),
        ]);
        assert_eq!(plugin.commands().len(), 1);
        assert_eq!(
            plugin.commands().get("dup").map(String::as_str),
            Some("second")
        );
    }

    #[test]
    fn known_name_request_returns_confirmation_echo() {
        let mut plugin = seeded();
        let replies = plugin.handle(&Packet::new(
            TYPE_RUNCOMMAND_REQUEST,
            json!({"name": "lock"}),
        ));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].ty(), TYPE_RUNCOMMAND_REQUEST);
        assert_eq!(
            replies[0].body,
            json!({"name": "lock", "command": "loginctl lock-session"})
        );
        assert_eq!(plugin.commands().len(), 2);
    }

    #[test]
    fn unknown_or_malformed_requests_produce_no_echo() {
        let mut plugin = seeded();
        for body in [
            json!({"name": "nope"}),
            json!({"name": ""}),
            json!({"name": 42}),
            json!({"Name": "lock"}),
            json!({}),
            Value::Null,
            json!("lock"),
            json!([]),
        ] {
            let replies = plugin.handle(&Packet::new(TYPE_RUNCOMMAND_REQUEST, body.clone()));
            assert!(replies.is_empty(), "body {body} produced an echo");
        }
        assert_eq!(plugin.commands().len(), 2);
    }

    #[test]
    fn sync_packet_replaces_local_table_and_enables_new_lookups() {
        let mut plugin = seeded();
        plugin.handle(&Packet::new(
            TYPE_RUNCOMMAND,
            json!({
                "commandList": [
                    {"key": "uuid-a", "name": "a", "command": "cmd-a"},
                    {"name": "b", "command": "cmd-b"}
                ]
            }),
        ));
        assert_eq!(plugin.commands().len(), 2);
        assert_eq!(
            plugin.commands().get("a").map(String::as_str),
            Some("cmd-a")
        );
        assert_eq!(plugin.commands().get("lock"), None);
        let replies = plugin.handle(&Packet::new(TYPE_RUNCOMMAND_REQUEST, json!({"name": "b"})));
        assert_eq!(replies[0].body, json!({"name": "b", "command": "cmd-b"}));
    }

    #[test]
    fn sync_skips_unusable_entries_and_dedups_by_name() {
        let mut plugin = RunCommandsPlugin::new();
        plugin.handle(&Packet::new(
            TYPE_RUNCOMMAND,
            json!({"commandList": [
                {"name": "", "command": "skipped-empty-name"},
                {"name": "ok1"},
                {"name": "ok1", "command": ""},
                {"name": "ok1", "command": "final"},
                {"name": 7, "command": "x"},
                ["not", "an", "object"],
                {"name": "ok2", "command": "cmd-two", "extra": [1, 2]}
            ]}),
        ));
        assert_eq!(plugin.commands().len(), 2);
        assert_eq!(
            plugin.commands().get("ok1").map(String::as_str),
            Some("final")
        );
        assert_eq!(
            plugin.commands().get("ok2").map(String::as_str),
            Some("cmd-two")
        );
    }

    #[test]
    fn sync_with_all_entries_malformed_keeps_current_table() {
        let mut plugin = seeded();
        plugin.handle(&Packet::new(
            TYPE_RUNCOMMAND,
            json!({"commandList": [{"nope": true}, {"name": 3}]}),
        ));
        assert_eq!(plugin.commands().len(), 2);
    }

    #[test]
    fn sync_with_explicitly_empty_list_clears_table() {
        let mut plugin = seeded();
        plugin.handle(&Packet::new(TYPE_RUNCOMMAND, json!({"commandList": []})));
        assert!(plugin.commands().is_empty());
    }

    #[test]
    fn sync_without_command_list_is_ignored_without_panic() {
        let mut plugin = seeded();
        for body in [
            json!({}),
            json!({"commandList": null}),
            json!({"commandList": "nope"}),
            json!({"commandList": {}}),
            Value::Null,
            json!([]),
        ] {
            let replies = plugin.handle(&Packet::new(TYPE_RUNCOMMAND, body.clone()));
            assert!(replies.is_empty(), "body {body} produced replies");
        }
        assert_eq!(plugin.commands().len(), 2);
    }

    #[test]
    fn ignores_foreign_packet_types_without_state_change() {
        let mut plugin = seeded();
        assert!(plugin
            .handle(&Packet::new("kdeconnect.ping", json!({"name": "lock"})))
            .is_empty());
        assert!(plugin
            .handle(&Packet::new(
                "kdeconnect.share.request",
                json!({"commandList": []})
            ))
            .is_empty());
        assert_eq!(plugin.commands().len(), 2);
    }

    #[test]
    fn factory_builds_fresh_instances_with_empty_tables() {
        let a = RunCommandsFactory.create();
        let b = RunCommandsFactory.create();
        assert_eq!(a.meta().name, "run_commands");
        assert_eq!(b.meta().name, "run_commands");
        // Factory-created instances are trait objects; verify freshness by
        // confirming that a valid request produces no execution echo (the
        // command table is empty so no name can resolve).
        let mut fresh = RunCommandsFactory.create();
        let pkt = Packet::new(TYPE_RUNCOMMAND_REQUEST, json!({"name": "x"}));
        assert!(fresh.handle(&pkt).is_empty());
    }
}
