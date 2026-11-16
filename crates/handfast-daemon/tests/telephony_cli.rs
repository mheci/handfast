#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{ControlResponse, ControlServer, DaemonCommand};
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
    command.push("telephony".to_string());
    command.extend(args.iter().map(|arg| (*arg).to_string()));
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&command)
                .output()
                .expect("run handfast telephony")
        }),
    )
    .await
    .expect("handfast telephony finishes")
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
    requested: bool,
}

async fn telephony(env: &TestEnv, data_dir: &Path, args: &[&str]) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            if let DaemonCommand::TelephonyMute { reply, .. } = command {
                let _ = observed.send(());
                let _ = reply.send(ControlResponse::Ok { bytes: 0 });
                break;
            }
        }
    });

    let output = run_cli(env, data_dir, args).await;
    server.shutdown().await;
    let requested = observed_rx.await.is_ok();
    script.await.expect("the script joins");
    Outcome { output, requested }
}

#[tokio::test]
async fn telephony_mutes_the_ringer() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = telephony(&env, data_dir.path(), &[PEER, "--mute"]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("muted the ringer on {PEER}")
    );
    assert!(outcome.requested);
}

#[tokio::test]
async fn telephony_needs_mute() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = telephony(&env, data_dir.path(), &[PEER]).await;

    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("needs --mute"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(!outcome.requested);
}

#[tokio::test]
async fn telephony_refuses_an_invalid_device_id() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &["not-a-device", "--mute"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("is not a device ID"),
        "stderr: {}",
        stderr(&output)
    );
}

#[tokio::test]
async fn telephony_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[PEER, "--mute"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
}
