#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{
    ControlResponse, ControlServer, DaemonCommand, DaemonStatus, DeviceSummary, IdentitySummary,
    PairedDevice, RemoteCommandRequest, RemoteDeviceCommands,
};
use handfast_proto::DeviceId;
use handfast_runtime::RemoteCommand;
use tokio::sync::{mpsc, oneshot};

const DEVICE: &str = "0123456789abcdef0123456789abcdef";
const PEER: &str = "fedcba9876543210fedcba9876543210";
const KEY: &str = "550e8400-e29b-41d4-a716-446655440000";

// Serializes the process-global `XDG_RUNTIME_DIR` change.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestEnv {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    runtime: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let guard = SERIAL.lock().await;
        let runtime = tempfile::TempDir::new().expect("a temp runtime dir");
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        Self {
            _guard: guard,
            runtime,
        }
    }
}

fn device_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("a valid device id")
}

fn command() -> RemoteCommand {
    RemoteCommand {
        key: KEY.to_string(),
        name: "List files".to_string(),
        command: "ls -l".to_string(),
    }
}

fn status_with(commands: Vec<RemoteCommand>, can_add: bool) -> DaemonStatus {
    DaemonStatus {
        version: "0.3.0".to_string(),
        pid: 4242,
        uptime_secs: 17,
        identity: IdentitySummary {
            device_id: device_id(DEVICE),
            device_name: "Laptop".to_string(),
            device_type: "desktop".to_string(),
            fingerprint: "ab".repeat(32),
            tcp_port: 1716,
        },
        incoming_dir: PathBuf::from("/home/me/Downloads"),
        connected: vec![DeviceSummary {
            device_id: device_id(PEER),
            device_name: "Other desktop".to_string(),
            device_type: "desktop".to_string(),
            connected: true,
        }],
        paired: vec![PairedDevice {
            device_id: device_id(PEER),
            device_name: "Other desktop".to_string(),
            device_type: "desktop".to_string(),
            connected: true,
        }],
        pending_pairings: Vec::new(),
        transfers: Vec::new(),
        remote_sinks: Vec::new(),
        remote_players: Vec::new(),
        remote_commands: vec![RemoteDeviceCommands {
            device_id: device_id(PEER),
            commands,
            can_add,
        }],
        truncated: false,
    }
}

async fn run_cli(env: &TestEnv, data_dir: &Path, args: &[&str]) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    let runtime = env.runtime.path().to_path_buf();
    let mut remote_command: Vec<String> = vec!["remote-command".to_string()];
    remote_command.extend(args.iter().map(|arg| (*arg).to_string()));
    remote_command.push("--data-dir".to_string());
    remote_command.push(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&remote_command)
                .output()
                .expect("run handfast remote-command")
        }),
    )
    .await
    .expect("handfast remote-command finishes")
    .expect("the handfast task joins")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

struct Outcome {
    output: std::process::Output,
    request: Option<RemoteCommandRequest>,
}

async fn remote_command(
    env: &TestEnv,
    data_dir: &Path,
    args: &[&str],
    status: DaemonStatus,
) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                DaemonCommand::Status { reply } => {
                    let _ = reply.send(ControlResponse::Status {
                        status: Box::new(status.clone()),
                    });
                }
                DaemonCommand::RemoteCommand { request, reply, .. } => {
                    let _ = observed.send(request);
                    let _ = reply.send(ControlResponse::Ok { bytes: 0 });
                    break;
                }
                _ => {}
            }
        }
    });

    let output = run_cli(env, data_dir, args).await;
    server.shutdown().await;
    let request = observed_rx.await.ok();
    script.await.expect("the script joins");
    Outcome { output, request }
}

#[tokio::test]
async fn remote_command_lists_the_reported_commands() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_command(
        &env,
        data_dir.path(),
        &[PEER],
        status_with(vec![command()], true),
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    let text = stdout(&outcome.output);
    assert!(text.contains(KEY), "{text}");
    assert!(text.contains("List files"), "{text}");
    assert!(text.contains("ls -l"), "{text}");
    assert!(outcome.request.is_none());
}

#[tokio::test]
async fn remote_command_sends_a_run_request() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_command(
        &env,
        data_dir.path(),
        &[PEER, "--run", KEY],
        status_with(vec![command()], true),
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("sent command {KEY} to {PEER}")
    );
    assert_eq!(
        outcome.request,
        Some(RemoteCommandRequest::Run {
            key: KEY.to_string(),
        })
    );
}

#[tokio::test]
async fn remote_command_requests_the_list() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_command(
        &env,
        data_dir.path(),
        &[PEER, "--list"],
        status_with(vec![command()], true),
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("requested the command list from {PEER}")
    );
    assert_eq!(outcome.request, Some(RemoteCommandRequest::List));
}

#[tokio::test]
async fn remote_command_needs_one_operation() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_command(
        &env,
        data_dir.path(),
        &[PEER, "--list", "--setup"],
        status_with(vec![command()], true),
    )
    .await;

    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("needs one operation"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(outcome.request.is_none());
}

#[tokio::test]
async fn remote_command_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[PEER]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
}
