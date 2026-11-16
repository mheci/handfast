//! Commands are configured locally, never accepted from a peer. The plugin
//! runs them as argv arrays without a shell and streams the output back.

use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use handfast_desktop::CommandSpec;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::roster;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_LINES: usize = 200;
const MAX_KEY_BYTES: usize = 256;
const MAX_RUNNING: usize = 4;

struct RunningCommand {
    id: String,
    child: Child,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    started: Instant,
}

/// Whether `key` is a command key a peer may use in a request.
#[must_use]
pub fn valid_command_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_BYTES && !key.chars().any(char::is_control)
}

fn log(level: LogLevel, message: impl Into<String>) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level,
        message: message.into(),
    })
}

fn started_packet(command: &str, id: &str) -> Result<Packet, ProtoError> {
    Packet::new(
        types::RUNCOMMAND,
        json!({ "commandStarted": true, "command": command, "id": id }),
    )
}

fn output_packet(id: &str, stdout: Vec<String>, stderr: Vec<String>) -> Result<Packet, ProtoError> {
    Packet::new(
        types::RUNCOMMAND_OUTPUT,
        json!({ "commandOutput": true, "stdout": stdout, "stderr": stderr, "id": id }),
    )
}

fn finished_packet(id: &str, success: bool, code: i32) -> Result<Packet, ProtoError> {
    Packet::new(
        types::RUNCOMMAND_OUTPUT,
        json!({ "commandFinished": true, "success": success, "exitCode": code, "id": id }),
    )
}

fn list_packet(specs: &[CommandSpec]) -> Result<Packet, ProtoError> {
    let mut list = serde_json::Map::new();
    for spec in specs {
        list.insert(
            spec.key.clone(),
            json!({ "name": spec.name, "command": spec.display() }),
        );
    }
    Packet::new(
        types::RUNCOMMAND,
        json!({
            "commandList": Value::Object(list).to_string(),
            "canAddCommand": false,
        }),
    )
}

fn output_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .take(MAX_LINES)
        .map(str::to_string)
        .collect()
}

async fn read_limited<R: AsyncRead + Unpin>(mut pipe: R, limit: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                if data.len() < limit {
                    let take = (limit - data.len()).min(read);
                    data.extend_from_slice(&buffer[..take]);
                }
            }
            Err(_) => break,
        }
    }
    data
}

async fn collect(task: Option<JoinHandle<Vec<u8>>>, deadline: Instant) -> Vec<u8> {
    let Some(task) = task else {
        return Vec::new();
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(remaining, task).await {
        Ok(Ok(data)) => data,
        _ => Vec::new(),
    }
}

/// Runs locally configured commands for a paired peer.
#[derive(Default)]
pub struct RunCommandPlugin {
    running: Vec<RunningCommand>,
    next_id: u64,
}

impl RunCommandPlugin {
    async fn stop_all(&mut self) {
        for mut running in self.running.drain(..) {
            let _ = running.child.kill().await;
            let _ = running.child.wait().await;
        }
    }

    fn start(&mut self, spec: &CommandSpec) -> PluginOutput {
        if self.running.len() >= MAX_RUNNING {
            return log(
                LogLevel::Warn,
                format!("runcommand is limited to {MAX_RUNNING} concurrent commands"),
            );
        }
        let id = format!("{}-{}", spec.key, self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        let Some((program, arguments)) = spec.argv.split_first() else {
            return log(LogLevel::Warn, "a command has no program");
        };
        let mut command = tokio::process::Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let packet = match finished_packet(&id, false, -1) {
                    Ok(packet) => packet,
                    Err(error) => return log(LogLevel::Warn, error.to_string()),
                };
                return PluginOutput::new()
                    .with_packet(packet)
                    .with_event(PluginEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("failed to start the command: {error}"),
                    });
            }
        };
        let stdout = child
            .stdout
            .take()
            .map(|pipe| tokio::spawn(read_limited(pipe, MAX_OUTPUT_BYTES)));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| tokio::spawn(read_limited(pipe, MAX_OUTPUT_BYTES)));
        self.running.push(RunningCommand {
            id: id.clone(),
            child,
            stdout,
            stderr,
            started: Instant::now(),
        });
        match started_packet(&spec.display(), &id) {
            Ok(packet) => PluginOutput::new().with_packet(packet),
            Err(error) => log(LogLevel::Warn, error.to_string()),
        }
    }
}

#[async_trait]
impl Plugin for RunCommandPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::RUNCOMMAND
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::RUNCOMMAND_REQUEST {
            return Ok(PluginOutput::new());
        }
        let Some(commands) = ctx.desktop.commands.as_ref() else {
            return Ok(log(
                LogLevel::Warn,
                "runcommand packet without a command adapter",
            ));
        };
        if packet
            .body
            .get("requestCommandList")
            .and_then(Value::as_bool)
            == Some(true)
        {
            let specs = commands
                .list()
                .await
                .map_err(|error| PluginError::Backend(error.to_string()))?;
            let packet = list_packet(&specs)
                .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
            return Ok(PluginOutput::new().with_packet(packet));
        }
        if packet.body.get("setup").and_then(Value::as_bool) == Some(true) {
            return Ok(log(
                LogLevel::Info,
                "runcommand setup requested; add commands with `handfast command add`",
            ));
        }
        if packet.body.get("stop").and_then(Value::as_bool) == Some(true) {
            self.stop_all().await;
            return Ok(log(LogLevel::Info, "runcommand stopped"));
        }
        let Some(key) = packet.body.get("key").and_then(Value::as_str) else {
            return Ok(log(LogLevel::Warn, "runcommand packet without a key"));
        };
        if !valid_command_key(key) {
            return Ok(log(LogLevel::Warn, "runcommand packet with an invalid key"));
        }
        let Some(spec) = commands
            .get(key)
            .await
            .map_err(|error| PluginError::Backend(error.to_string()))?
        else {
            return Ok(log(
                LogLevel::Warn,
                format!("runcommand key {key} is not configured"),
            ));
        };
        Ok(self.start(&spec))
    }

    async fn on_tick(&mut self, _ctx: &mut PluginContext<'_>) -> Result<PluginOutput, PluginError> {
        if self.running.is_empty() {
            return Ok(PluginOutput::new());
        }
        let mut finished: Vec<(usize, bool, i32)> = Vec::new();
        for (index, running) in self.running.iter_mut().enumerate() {
            match running.child.try_wait() {
                Ok(Some(status)) => {
                    finished.push((index, status.success(), status.code().unwrap_or(-1)));
                }
                Ok(None) if running.started.elapsed() >= COMMAND_TIMEOUT => {
                    let _ = running.child.kill().await;
                    match running.child.wait().await {
                        Ok(status) => {
                            finished.push((index, status.success(), status.code().unwrap_or(-1)));
                        }
                        Err(_) => finished.push((index, false, -1)),
                    }
                }
                Ok(None) => {}
                Err(_) => finished.push((index, false, -1)),
            }
        }
        let mut output = PluginOutput::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        for (index, success, code) in finished.into_iter().rev() {
            let running = self.running.swap_remove(index);
            let stdout = collect(running.stdout, deadline).await;
            let stderr = collect(running.stderr, deadline).await;
            let stdout = output_lines(&stdout);
            let stderr = output_lines(&stderr);
            if !stdout.is_empty() || !stderr.is_empty() {
                let packet = output_packet(&running.id, stdout, stderr)
                    .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
                output = output.with_packet(packet);
            }
            let packet = finished_packet(&running.id, success, code)
                .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
            output = output.with_packet(packet);
        }
        Ok(output)
    }

    async fn on_disconnect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        self.stop_all().await;
        Ok(PluginOutput::new())
    }
}

/// Creates one command runner per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunCommandFactory;

impl PluginFactory for RunCommandFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::RUNCOMMAND
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(RunCommandPlugin::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use handfast_desktop::{Commands, Desktop, DesktopError, Result as DesktopResult};
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;

    struct FakeCommands {
        specs: Vec<CommandSpec>,
    }

    #[async_trait]
    impl Commands for FakeCommands {
        async fn list(&self) -> DesktopResult<Vec<CommandSpec>> {
            Ok(self.specs.clone())
        }

        async fn get(&self, key: &str) -> DesktopResult<Option<CommandSpec>> {
            Ok(self.specs.iter().find(|spec| spec.key == key).cloned())
        }

        async fn add(&self, _key: &str, _name: &str, _argv: &[String]) -> DesktopResult<()> {
            Err(DesktopError::Backend("not used".to_string()))
        }

        async fn remove(&self, _key: &str) -> DesktopResult<bool> {
            Ok(false)
        }
    }

    fn desktop(specs: Vec<CommandSpec>) -> Desktop {
        Desktop {
            commands: Some(Arc::new(FakeCommands { specs })),
            ..Desktop::default()
        }
    }

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn run(plugin: &mut RunCommandPlugin, desktop: &Desktop, packet: Packet) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    async fn tick(plugin: &mut RunCommandPlugin, desktop: &Desktop) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_tick(&mut ctx).await.unwrap()
    }

    fn packet(body: Value) -> Packet {
        Packet::from_body(types::RUNCOMMAND_REQUEST, &body).unwrap()
    }

    #[cfg(unix)]
    fn echo_spec() -> CommandSpec {
        CommandSpec {
            key: "echo".to_string(),
            name: "Echo".to_string(),
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'hello\\n'".to_string(),
            ],
        }
    }

    #[cfg(windows)]
    fn echo_spec() -> CommandSpec {
        CommandSpec {
            key: "echo".to_string(),
            name: "Echo".to_string(),
            argv: vec![
                "cmd.exe".to_string(),
                "/C".to_string(),
                "echo hello".to_string(),
            ],
        }
    }

    #[tokio::test]
    async fn a_list_request_sends_the_configured_commands() {
        let desktop = desktop(vec![echo_spec()]);
        let mut plugin = RunCommandPlugin::default();

        let output = run(
            &mut plugin,
            &desktop,
            packet(json!({ "requestCommandList": true })),
        )
        .await;

        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].ptype, types::RUNCOMMAND);
        assert_eq!(output.packets[0].body["canAddCommand"], false);
        let list: Value =
            serde_json::from_str(output.packets[0].body["commandList"].as_str().unwrap()).unwrap();
        assert_eq!(list["echo"]["name"], "Echo");
        assert!(list["echo"]["command"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn an_unknown_key_is_a_warning() {
        let desktop = desktop(vec![echo_spec()]);
        let mut plugin = RunCommandPlugin::default();

        let output = run(&mut plugin, &desktop, packet(json!({ "key": "missing" }))).await;

        assert!(output.packets.is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_command_runs_and_reports_output() {
        let desktop = desktop(vec![echo_spec()]);
        let mut plugin = RunCommandPlugin::default();

        let output = run(&mut plugin, &desktop, packet(json!({ "key": "echo" }))).await;
        assert_eq!(output.packets.len(), 1);
        assert_eq!(output.packets[0].body["commandStarted"], true);
        assert!(output.packets[0].body["command"]
            .as_str()
            .unwrap()
            .contains("hello"));

        let mut packets = Vec::new();
        for _ in 0..50 {
            let output = tick(&mut plugin, &desktop).await;
            packets.extend(output.packets);
            if packets.iter().any(|packet| {
                packet.body.get("commandFinished").and_then(Value::as_bool) == Some(true)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            packets.iter().any(|packet| packet
                .body
                .get("commandFinished")
                .and_then(Value::as_bool)
                == Some(true)),
            "the command finishes"
        );
        assert!(
            packets.iter().any(|packet| {
                packet.body.get("commandOutput").and_then(Value::as_bool) == Some(true)
                    && packet
                        .body
                        .get("stdout")
                        .and_then(Value::as_array)
                        .is_some_and(|lines| lines.iter().any(|line| line == "hello"))
            }),
            "the output is reported"
        );
    }

    #[tokio::test]
    async fn concurrent_commands_are_capped() {
        let desktop = desktop(vec![echo_spec()]);
        let mut plugin = RunCommandPlugin::default();

        for _ in 0..MAX_RUNNING {
            run(&mut plugin, &desktop, packet(json!({ "key": "echo" }))).await;
        }
        let output = run(&mut plugin, &desktop, packet(json!({ "key": "echo" }))).await;

        assert!(output.packets.is_empty());
        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
        plugin.stop_all().await;
    }

    #[tokio::test]
    async fn a_stop_request_clears_running_commands() {
        let desktop = desktop(vec![echo_spec()]);
        let mut plugin = RunCommandPlugin::default();
        run(&mut plugin, &desktop, packet(json!({ "key": "echo" }))).await;

        let output = run(&mut plugin, &desktop, packet(json!({ "stop": true }))).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Info,
                ..
            }]
        ));
        assert!(plugin.running.is_empty(), "the command was stopped");
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(&roster::RUNCOMMAND, RunCommandPlugin::default().meta());
        assert_eq!(&roster::RUNCOMMAND, RunCommandFactory.meta());
        assert_eq!(
            roster::RUNCOMMAND.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
