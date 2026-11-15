#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{ControlResponse, ControlServer, DaemonCommand};
use handfast_proto::DeviceId;
use tokio::sync::{mpsc, oneshot};

const DEVICE: &str = "0123456789abcdef0123456789abcdef";

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

fn device_id() -> DeviceId {
    DeviceId::new(DEVICE).expect("a valid device id")
}

/// Run `handfast ring` with the arguments after the subcommand name.
async fn run_cli(env: &TestEnv, data_dir: &Path, args: &[&str]) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    let runtime = env.runtime.path().to_path_buf();
    let mut ring: Vec<String> = vec!["ring".to_string()];
    ring.extend(args.iter().map(|arg| (*arg).to_string()));
    ring.push("--data-dir".to_string());
    ring.push(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&ring)
                .output()
                .expect("run handfast ring")
        }),
    )
    .await
    .expect("handfast ring finishes")
    .expect("the handfast task joins")
}

struct Outcome {
    output: std::process::Output,
    observed: Option<DeviceId>,
}

async fn ring(
    env: &TestEnv,
    data_dir: &Path,
    args: &[&str],
    reply_with: ControlResponse,
) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::Ring { device_id, reply }) = command_rx.recv().await {
            let _ = observed.send(device_id);
            let _ = reply.send(reply_with);
        }
    });

    let output = run_cli(env, data_dir, args).await;
    server.shutdown().await;
    let observed = observed_rx.await.ok();
    script.await.expect("the script joins");
    Outcome { output, observed }
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[tokio::test]
async fn ring_reaches_the_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = ring(
        &env,
        data_dir.path(),
        &[DEVICE],
        ControlResponse::Ok { bytes: 0 },
    )
    .await;
    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(stdout(&outcome.output).trim(), format!("ringing {DEVICE}"));
    assert_eq!(outcome.observed, Some(device_id()));
}

#[tokio::test]
async fn ring_reports_an_offline_device() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = ring(
        &env,
        data_dir.path(),
        &[DEVICE],
        ControlResponse::Err {
            message: format!("device {DEVICE} is not connected"),
        },
    )
    .await;
    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("is not connected"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(stdout(&outcome.output).is_empty());
    assert_eq!(outcome.observed, Some(device_id()));
}

#[tokio::test]
async fn ring_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[DEVICE]).await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[tokio::test]
async fn ring_needs_a_device_id() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[]).await;
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
}

#[tokio::test]
async fn ring_rejects_a_malformed_device_id() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = ring(
        &env,
        data_dir.path(),
        &["not-a-device"],
        ControlResponse::Ok { bytes: 0 },
    )
    .await;
    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("is not a device ID"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(outcome.observed.is_none(), "no command reaches the daemon");
}
