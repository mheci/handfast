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

async fn run_pair(env: &TestEnv, data_dir: &Path, args: &[&str]) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    let runtime = env.runtime.path().to_path_buf();
    let mut pair: Vec<String> = vec!["pair".to_string()];
    pair.extend(args.iter().map(|arg| (*arg).to_string()));
    pair.push("--data-dir".to_string());
    pair.push(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&pair)
                .output()
                .expect("run handfast pair")
        }),
    )
    .await
    .expect("handfast pair finishes")
    .expect("the handfast task joins")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[tokio::test]
async fn pair_accepts_over_the_socket() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::PairingDecision {
            device_id,
            accept,
            reply,
        }) = command_rx.recv().await
        {
            let _ = observed.send((device_id, accept));
            let _ = reply.send(ControlResponse::Decided);
        }
    });

    let output = run_pair(&env, data_dir.path(), &["--accept", DEVICE]).await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("pairing accepted for {DEVICE}")
    );

    let (device, accept) = tokio::time::timeout(Duration::from_secs(5), observed_rx)
        .await
        .expect("the daemon sees the request")
        .expect("the script forwards the request");
    assert_eq!(device, device_id());
    assert!(accept);

    server.shutdown().await;
    script.await.expect("the script joins");
}

#[tokio::test]
async fn pair_rejects_over_the_socket() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::PairingDecision {
            device_id,
            accept,
            reply,
        }) = command_rx.recv().await
        {
            let _ = observed.send((device_id, accept));
            let _ = reply.send(ControlResponse::Decided);
        }
    });

    let output = run_pair(&env, data_dir.path(), &["--reject", DEVICE]).await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("pairing rejected for {DEVICE}")
    );

    let (device, accept) = tokio::time::timeout(Duration::from_secs(5), observed_rx)
        .await
        .expect("the daemon sees the request")
        .expect("the script forwards the request");
    assert_eq!(device, device_id());
    assert!(!accept);

    server.shutdown().await;
    script.await.expect("the script joins");
}

#[tokio::test]
async fn pair_reports_an_unknown_request() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::PairingDecision { reply, .. }) = command_rx.recv().await {
            let _ = reply.send(ControlResponse::Err {
                message: format!("no pairing request from {DEVICE}"),
            });
        }
    });

    let output = run_pair(&env, data_dir.path(), &["--accept", DEVICE]).await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("no pairing request"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "no success output");

    server.shutdown().await;
    script.await.expect("the script joins");
}

#[tokio::test]
async fn pair_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_pair(&env, data_dir.path(), &["--reject", DEVICE]).await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "no success output");
}

#[tokio::test]
async fn pair_needs_exactly_one_answer() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_pair(&env, data_dir.path(), &[]).await;
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));

    let output = run_pair(
        &env,
        data_dir.path(),
        &["--accept", DEVICE, "--reject", DEVICE],
    )
    .await;
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
}
