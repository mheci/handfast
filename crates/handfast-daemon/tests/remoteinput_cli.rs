#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{ControlResponse, ControlServer, DaemonCommand, RemoteInputAction};
use handfast_plugins::remotecontrol::{RemoteClick, RemoteInputModifiers};
use tokio::sync::{mpsc, oneshot};

const PEER: &str = "fedcba9876543210fedcba9876543210";

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

async fn run_cli(env: &TestEnv, data_dir: &Path, args: &[&str]) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    let runtime = env.runtime.path().to_path_buf();
    let mut command: Vec<String> = vec!["--data-dir".to_string()];
    command.push(data_dir.to_string_lossy().into_owned());
    command.push("remote-input".to_string());
    command.extend(args.iter().map(|arg| (*arg).to_string()));
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&command)
                .output()
                .expect("run handfast remote-input")
        }),
    )
    .await
    .expect("handfast remote-input finishes")
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
    action: Option<RemoteInputAction>,
}

async fn remote_input(env: &TestEnv, data_dir: &Path, args: &[&str]) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            if let DaemonCommand::RemoteInput { action, reply, .. } = command {
                let _ = observed.send(action);
                let _ = reply.send(ControlResponse::Ok { bytes: 0 });
                break;
            }
        }
    });

    let output = run_cli(env, data_dir, args).await;
    server.shutdown().await;
    let action = observed_rx.await.ok();
    script.await.expect("the script joins");
    Outcome { output, action }
}

#[tokio::test]
async fn remote_input_sends_a_move_with_negative_deltas() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_input(&env, data_dir.path(), &[PEER, "--move", "-5", "3"]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("sent input to {PEER}")
    );
    assert_eq!(
        outcome.action,
        Some(RemoteInputAction::Move { dx: -5, dy: 3 })
    );
}

#[tokio::test]
async fn remote_input_sends_a_click() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_input(&env, data_dir.path(), &[PEER, "--click", "double"]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        outcome.action,
        Some(RemoteInputAction::Click {
            button: RemoteClick::Double,
        })
    );
}

#[tokio::test]
async fn remote_input_sends_a_key_with_modifiers() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_input(
        &env,
        data_dir.path(),
        &[PEER, "--key", "hello", "--ctrl", "--shift"],
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        outcome.action,
        Some(RemoteInputAction::Key {
            text: "hello".to_string(),
            modifiers: RemoteInputModifiers {
                alt: false,
                ctrl: true,
                shift: true,
                sup: false,
            },
        })
    );
}

#[tokio::test]
async fn remote_input_needs_one_operation() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_input(
        &env,
        data_dir.path(),
        &[PEER, "--move", "1", "2", "--click", "left"],
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
    assert!(outcome.action.is_none());
}

#[tokio::test]
async fn remote_input_refuses_modifiers_without_a_key() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = remote_input(&env, data_dir.path(), &[PEER, "--click", "left", "--alt"]).await;

    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("modifiers need --key or --special"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(outcome.action.is_none());
}

#[tokio::test]
async fn remote_input_refuses_an_out_of_range_special_key() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[PEER, "--special", "33"]).await;

    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
}

#[tokio::test]
async fn remote_input_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[PEER, "--click", "left"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
}
