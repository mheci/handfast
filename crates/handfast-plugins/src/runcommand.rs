//! Execute configured commands when the paired phone asks.
//!
//! This is the only plugin in the roster that can change the host by running a
//! program, so its defaults are the point of it. A phone that is paired is
//! still a device in a pocket: it should not be able to name an arbitrary
//! command and have it run.
//!
//! Upstream stores a JSON map of `key -> {"name": ..., "command": ...}` in the
//! plugin config and executes the command for a matching key. Handfast keeps
//! the same wire format — `kdeconnect.runcommand` carries `commandList`, and
//! `kdeconnect.runcommand.request` carries `key` — but the allowlist lives in
//! `plugins.toml`:
//!
//! ```toml
//! [runcommand]
//! commands = "lock:loginctl lock-session,music:playerctl play-pause"
//! ```
//!
//! Each entry is `key:command`. The command is **not** run through a shell: it
//! is split on whitespace and executed directly, so a key can never become a
//! command-injection vector even if the allowlist is written loosely. A
//! missing or empty `commands` key means nothing is executable.
//!
//! Output is streamed back as `kdeconnect.runcommand.output` in the three
//! shapes upstream sends: `commandStarted`, `commandOutput` (batches of five
//! lines) and `commandFinished` with an exit code.

use std::collections::BTreeMap;
use std::process::Stdio;

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::roster;

/// Lines per `commandOutput` packet. Upstream batches five at a time.
const OUTPUT_BATCH: usize = 5;

/// Largest accepted command key, in bytes.
const MAX_KEY_BYTES: usize = 128;

/// Largest command line the allowlist may hold, in bytes.
const MAX_COMMAND_BYTES: usize = 4096;

/// Longest single output line forwarded to the phone, in bytes.
///
/// A command that prints a megabyte on one line would otherwise produce a
/// packet the peer cannot decode.
const MAX_LINE_BYTES: usize = 1024;

/// One configured command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredCommand {
    /// Stable key the phone sends to trigger it.
    pub key: String,
    /// Human-readable name shown on the phone.
    pub name: String,
    /// Program and arguments, already split.
    program: String,
    args: Vec<String>,
}

/// Why a command request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCommandError {
    /// The key is not in the allowlist.
    NotAllowed,
    /// The key was empty or oversized.
    InvalidKey,
    /// The configured command could not be parsed into a program and arguments.
    InvalidCommand,
}

impl std::fmt::Display for RunCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAllowed => f.write_str("command key is not in the allowlist"),
            Self::InvalidKey => f.write_str("unusable command key"),
            Self::InvalidCommand => f.write_str("configured command could not be parsed"),
        }
    }
}

impl std::error::Error for RunCommandError {}

/// Parse the `commands` config value into an ordered allowlist.
///
/// The syntax is `key:command` entries separated by commas; the key may also
/// carry a display name as `key:name:command`, which is how a desktop UI can
/// show something friendlier than `lock`. An entry that does not split into a
/// usable program is skipped rather than executed.
#[must_use]
pub fn parse_commands(configured: &str) -> Vec<ConfiguredCommand> {
    let mut commands = Vec::new();
    for entry in configured.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.splitn(3, ':');
        let Some(key) = parts.next().map(str::trim) else {
            continue;
        };
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            continue;
        }
        let (name, command) = match (parts.next(), parts.next()) {
            // key:name:command
            (Some(name), Some(command)) => (name.trim(), command.trim()),
            // key:command
            (Some(command), None) => (key, command.trim()),
            _ => continue,
        };
        if command.is_empty() || command.len() > MAX_COMMAND_BYTES {
            continue;
        }
        let mut words = command.split_whitespace();
        let Some(program) = words.next() else {
            continue;
        };
        commands.push(ConfiguredCommand {
            key: key.to_string(),
            name: if name.is_empty() {
                key.to_string()
            } else {
                name.to_string()
            },
            program: program.to_string(),
            args: words.map(str::to_string).collect(),
        });
    }
    commands
}

/// The `commandList` payload: a JSON object rendered as a string, exactly as
/// upstream's KConfig-backed plugin sends it.
fn command_list(commands: &[ConfiguredCommand]) -> Result<String, PluginError> {
    let mut map = Map::new();
    for command in commands {
        map.insert(
            command.key.clone(),
            json!({
                "name": command.name,
                "command": format!("{} {}", command.program, command.args.join(" "))
                    .trim()
                    .to_string(),
            }),
        );
    }
    serde_json::to_string(&Value::Object(map))
        .map_err(|error| PluginError::InvalidPacket(error.to_string()))
}

/// One running command, tracked so `stop` can terminate it.
struct Running {
    child: tokio::process::Child,
}

/// Runs allowlisted commands on request.
pub struct RunCommandPlugin {
    /// Allowlist, loaded once on connect.
    commands: BTreeMap<String, ConfiguredCommand>,
    /// Whether the allowlist has been loaded from the config yet.
    loaded: bool,
    /// Running children, by the id sent in `commandOutput`.
    running: Vec<(u64, Running)>,
    /// Next command id.
    next_id: u64,
    /// Output packets queued for the next tick.
    queued: Vec<Packet>,
    /// Receives streamed output from every running child.
    ///
    /// A shared channel rather than a drain at spawn time: the child writes
    /// after `on_packet` returns, so anything read eagerly would be lost.
    output: mpsc::Receiver<Packet>,
    /// The sending half, cloned into each output task.
    sink: mpsc::Sender<Packet>,
}

impl std::fmt::Debug for RunCommandPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCommandPlugin")
            .field("allowlist", &self.commands.keys().collect::<Vec<_>>())
            .field("running", &self.running.len())
            .finish()
    }
}

impl Default for RunCommandPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl RunCommandPlugin {
    /// Create a run command plugin with an empty allowlist.
    #[must_use]
    pub fn new() -> Self {
        let (sink, output) = mpsc::channel(64);
        Self {
            commands: BTreeMap::new(),
            loaded: false,
            running: Vec::new(),
            next_id: 1,
            queued: Vec::new(),
            output,
            sink,
        }
    }

    /// Load the allowlist from the host config, once.
    fn ensure_loaded(&mut self, ctx: &PluginContext<'_>) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let configured = ctx
            .config
            .get(roster::RUNCOMMAND.name, "commands")
            .unwrap_or_default();
        self.commands = parse_commands(configured)
            .into_iter()
            .map(|command| (command.key.clone(), command))
            .collect();
    }

    /// Command keys that are executable, in sorted order.
    #[must_use]
    pub fn allowed_keys(&self) -> Vec<String> {
        self.commands.keys().cloned().collect()
    }

    /// Number of running children.
    #[must_use]
    pub fn running(&self) -> usize {
        self.running.len()
    }

    fn output_packet(id: u64, stdout: Vec<String>, stderr: Vec<String>) -> Option<Packet> {
        packet(
            types::RUNCOMMAND_OUTPUT,
            json!({
                "commandOutput": true,
                "stdout": stdout,
                "stderr": stderr,
                "id": id,
            }),
        )
    }

    fn finished_packet(id: u64, exit_code: i32) -> Option<Packet> {
        packet(
            types::RUNCOMMAND_OUTPUT,
            json!({
                "commandFinished": true,
                "success": exit_code == 0,
                "exitCode": exit_code,
                "id": id,
            }),
        )
    }
}

/// Build a packet, or `None` when the type or body is unusable.
///
/// Every packet built here has a constant type and an object body, so `None`
/// is unreachable in practice. It is still returned rather than unwrapped:
/// a protocol constant that drifted must drop one packet, not panic a device
/// session.
fn packet(ptype: &str, body: Value) -> Option<Packet> {
    Packet::new(ptype, body).ok()
}

/// Read a command's output, batching lines into packets.
async fn stream_output(
    stream: impl tokio::io::AsyncRead + Unpin,
    is_stderr: bool,
    id: u64,
    tx: mpsc::Sender<Packet>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut lines = BufReader::new(stream).lines();
    let mut batch: Vec<String> = Vec::new();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = if line.len() > MAX_LINE_BYTES {
                    line.chars().take(MAX_LINE_BYTES).collect()
                } else {
                    line
                };
                batch.push(line);
                if batch.len() == OUTPUT_BATCH {
                    let taken = std::mem::take(&mut batch);
                    let packet = if is_stderr {
                        RunCommandPlugin::output_packet(id, Vec::new(), taken)
                    } else {
                        RunCommandPlugin::output_packet(id, taken, Vec::new())
                    };
                    match packet {
                        Some(packet) => {
                            if tx.send(packet).await.is_err() {
                                return;
                            }
                        }
                        // A malformed constant would silently truncate output;
                        // stop instead of looping over unusable packets.
                        None => return,
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    if !batch.is_empty() {
        let packet = if is_stderr {
            RunCommandPlugin::output_packet(id, Vec::new(), batch)
        } else {
            RunCommandPlugin::output_packet(id, batch, Vec::new())
        };
        if let Some(packet) = packet {
            let _ = tx.send(packet).await;
        }
    }
}

#[async_trait]
impl Plugin for RunCommandPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::RUNCOMMAND
    }

    async fn on_connect(
        &mut self,
        ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        self.ensure_loaded(ctx);
        let list = command_list(&self.commands.values().cloned().collect::<Vec<_>>())?;
        let mut body = Map::new();
        body.insert("commandList".to_string(), Value::String(list));
        // Handfast cannot accept commands added from the phone, because the
        // allowlist is a file the user edits. Advertising the capability would
        // make the phone offer an editor that cannot work.
        body.insert("canAddCommand".to_string(), Value::Bool(false));
        Ok(PluginOutput::new()
            .with_optional_packet(packet(types::RUNCOMMAND, Value::Object(body)))
            .with_event(PluginEvent::Log {
                level: LogLevel::Info,
                message: format!("{} command(s) available to the peer", self.commands.len()),
            }))
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        // Reap finished children and emit their exit codes.
        let mut finished = Vec::new();
        let mut index = 0;
        while index < self.running.len() {
            match self.running[index].1.child.try_wait() {
                Ok(Some(status)) => {
                    let (id, _running) = self.running.remove(index);
                    finished.push((id, status.code().unwrap_or(-1)));
                }
                Ok(None) => index += 1,
                Err(_) => {
                    let (id, _) = self.running.remove(index);
                    finished.push((id, -1));
                }
            }
        }
        for (id, code) in finished {
            if let Some(packet) = RunCommandPlugin::finished_packet(id, code) {
                self.queued.push(packet);
            }
        }
        // Drain whatever the child wrote since the last tick.
        while let Ok(packet) = self.output.try_recv() {
            self.queued.push(packet);
        }
        if self.queued.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut output = PluginOutput::new();
        output.packets = std::mem::take(&mut self.queued);
        Ok(output)
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet_in: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet_in.ptype != types::RUNCOMMAND_REQUEST {
            return Ok(PluginOutput::new());
        }
        self.ensure_loaded(ctx);

        // The phone asks for the list again.
        if packet_in
            .body
            .get("requestCommandList")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return self.on_connect(ctx).await;
        }

        // The phone asks the desktop to stop everything running.
        if packet_in
            .body
            .get("stop")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let count = self.running.len();
            for (_, mut running) in self.running.drain(..) {
                let _ = running.child.start_kill();
            }
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: format!("stopped {count} command(s) at the peer's request"),
            }));
        }

        let Some(key) = packet_in.body.get("key").and_then(Value::as_str) else {
            return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                level: LogLevel::Warn,
                message: "runcommand request without a key".to_string(),
            }));
        };
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Ok(refuse("runcommand key is unusable"));
        }
        let Some(command) = self.commands.get(key).cloned() else {
            // This is the security boundary: an unknown key is logged loudly
            // and executed never.
            return Ok(refuse(&format!(
                "refused runcommand key {key:?}: not in the allowlist"
            )));
        };

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let mut child = match Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                if let Some(packet) = RunCommandPlugin::output_packet(
                    id,
                    Vec::new(),
                    vec![format!("could not start {}: {error}", command.program)],
                ) {
                    self.queued.push(packet);
                }
                if let Some(packet) = RunCommandPlugin::finished_packet(id, -1) {
                    self.queued.push(packet);
                }
                return Ok(PluginOutput::new().with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("command {key:?} failed to start: {error}"),
                }));
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if let Some(stdout) = stdout {
            tokio::spawn(stream_output(stdout, false, id, self.sink.clone()));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(stream_output(stderr, true, id, self.sink.clone()));
        }

        self.running.push((id, Running { child }));
        if let Some(packet) = packet(
            types::RUNCOMMAND_OUTPUT,
            json!({ "commandStarted": true, "command": command.name, "id": id }),
        ) {
            self.queued.push(packet);
        }

        Ok(PluginOutput::new().with_event(PluginEvent::Log {
            level: LogLevel::Info,
            message: format!("started allowed command {key:?}"),
        }))
    }
}

fn refuse(reason: &str) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: reason.to_string(),
    })
}

/// Builds [`RunCommandPlugin`] instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunCommandFactory;

impl PluginFactory for RunCommandFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::RUNCOMMAND
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RunCommandPlugin::new())
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
        plugin: RunCommandPlugin,
        desktop: handfast_desktop::Desktop,
        device: Device,
        config: PluginConfig,
    }

    impl Harness {
        fn new(commands: &str) -> Self {
            let mut config = PluginConfig::new();
            if !commands.is_empty() {
                config.insert("runcommand", "commands", commands);
            }
            Self {
                plugin: RunCommandPlugin::new(),
                desktop: MockDesktop::new().desktop(),
                device: device(),
                config,
            }
        }

        /// Run `body` through the plugin with a context borrowed from `self`.
        ///
        /// The destructure keeps the context borrow distinct from the mutable
        /// borrow of `plugin` that `on_packet` needs.
        async fn send(&mut self, body: Value) -> PluginOutput {
            let Self {
                plugin,
                desktop,
                device,
                config,
            } = self;
            let packet = Packet::from_body(types::RUNCOMMAND_REQUEST, &body).unwrap();
            let mut ctx = PluginContext {
                device,
                desktop,
                config,
            };
            plugin.on_packet(&mut ctx, &packet).await.unwrap()
        }

        async fn tick(&mut self) -> PluginOutput {
            let Self {
                plugin,
                desktop,
                device,
                config,
            } = self;
            let mut ctx = PluginContext {
                device,
                desktop,
                config,
            };
            plugin.on_tick(&mut ctx).await.unwrap()
        }

        async fn connect(&mut self) -> PluginOutput {
            let Self {
                plugin,
                desktop,
                device,
                config,
            } = self;
            let mut ctx = PluginContext {
                device,
                desktop,
                config,
            };
            plugin.on_connect(&mut ctx).await.unwrap()
        }
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::RUNCOMMAND.status, PluginStatus::Implemented);
        assert_eq!(roster::RUNCOMMAND.incoming, &[types::RUNCOMMAND_REQUEST]);
        assert_eq!(
            roster::RUNCOMMAND.outgoing,
            &[types::RUNCOMMAND, types::RUNCOMMAND_OUTPUT]
        );
    }

    #[test]
    fn the_config_parser_handles_both_entry_shapes() {
        let commands =
            parse_commands("lock:loginctl lock-session, music:Play music:playerctl play-pause");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].key, "lock");
        assert_eq!(commands[0].name, "lock");
        assert_eq!(commands[0].program, "loginctl");
        assert_eq!(commands[0].args, vec!["lock-session"]);
        assert_eq!(commands[1].key, "music");
        assert_eq!(commands[1].name, "Play music");
        assert_eq!(commands[1].program, "playerctl");
    }

    #[test]
    fn malformed_entries_are_skipped_not_executed() {
        // No command, empty key, and a bare key with nothing after the colon.
        let commands = parse_commands(":oops, , nocommand:, good:true");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].key, "good");
        assert_eq!(commands[0].program, "true");
    }

    #[tokio::test]
    async fn no_config_means_no_executable_commands() {
        let mut harness = Harness::new("");
        let output = harness.connect().await;
        assert!(harness.plugin.allowed_keys().is_empty());
        let list = &output.packets[0];
        assert_eq!(list.ptype, types::RUNCOMMAND);
        let command_list = list
            .body
            .get("commandList")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(command_list, "{}");
        // The desktop cannot accept new commands from the phone.
        assert_eq!(list.body.get("canAddCommand"), Some(&json!(false)));
    }

    #[tokio::test]
    async fn the_allowlist_is_advertised_on_connect() {
        let mut harness = Harness::new("lock:Shut the screen:true");
        let output = harness.connect().await;
        let list = output.packets[0]
            .body
            .get("commandList")
            .and_then(Value::as_str)
            .unwrap();
        let parsed: Value = serde_json::from_str(list).unwrap();
        assert_eq!(parsed["lock"]["name"], json!("Shut the screen"));
        assert_eq!(parsed["lock"]["command"], json!("true"));
    }

    #[tokio::test]
    async fn an_unknown_key_is_refused() {
        let mut harness = Harness::new("lock:true");
        harness.connect().await;
        let output = harness.send(json!({ "key": "rm" })).await;
        assert!(output.packets.is_empty(), "nothing is executed or reported");
        assert_eq!(output.events.len(), 1);
        assert_eq!(harness.plugin.running(), 0);
    }

    #[tokio::test]
    async fn a_missing_key_is_refused() {
        let mut harness = Harness::new("lock:true");
        harness.connect().await;
        let output = harness.send(json!({})).await;
        assert_eq!(output.events.len(), 1);
        assert_eq!(harness.plugin.running(), 0);
    }

    #[tokio::test]
    async fn an_allowed_key_runs_and_reports_started() {
        let mut harness = Harness::new("noop:true");
        harness.connect().await;
        let output = harness.send(json!({ "key": "noop" })).await;
        assert_eq!(output.events.len(), 1);
        assert_eq!(harness.plugin.running(), 1);

        // Let the child exit, then the tick reports the exit code.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let tick = harness.tick().await;
        let finished: Vec<&Packet> = tick
            .packets
            .iter()
            .filter(|packet| packet.body.contains_key("commandFinished"))
            .collect();
        assert_eq!(finished.len(), 1, "one commandFinished packet");
        assert_eq!(finished[0].body.get("success"), Some(&json!(true)));
        assert_eq!(finished[0].body.get("exitCode"), Some(&json!(0)));
        assert_eq!(harness.plugin.running(), 0);
    }

    #[tokio::test]
    async fn the_command_list_can_be_requested_again() {
        let mut harness = Harness::new("noop:true");
        let output = harness.send(json!({ "requestCommandList": true })).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::RUNCOMMAND);
    }

    #[tokio::test]
    async fn stop_with_nothing_running_is_harmless() {
        let mut harness = Harness::new("noop:true");
        harness.connect().await;
        let output = harness.send(json!({ "stop": true })).await;
        assert!(output.packets.is_empty());
        assert_eq!(harness.plugin.running(), 0);
    }
}
