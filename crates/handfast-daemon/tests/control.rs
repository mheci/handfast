#![cfg(unix)]

//! `hfctl` round trips over the Unix control socket.
//!
//! Each test runs a `ControlServer` plus a scripted command loop, then spawns
//! the real `hfctl` binary.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{control_path, ControlResponse, ControlServer, DaemonCommand};
use handfast_desktop::{FileLimits, Files, LocalFiles};
use handfast_proto::DeviceId;
use tokio::sync::mpsc;

const DEVICE: &str = "0123456789abcdef0123456789abcdef";

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serializes `XDG_RUNTIME_DIR` changes and gives each test its own runtime
/// directory.
struct TestEnv {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    runtime: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let guard = ENV_LOCK.lock().await;
        let runtime = tempfile::TempDir::new().expect("a temp runtime dir");
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        Self {
            _guard: guard,
            runtime,
        }
    }

    fn runtime(&self) -> &Path {
        self.runtime.path()
    }
}

fn device_id() -> DeviceId {
    DeviceId::new(DEVICE).expect("a valid device id")
}

async fn run_hfctl(env: &TestEnv, data_dir: &Path, path: &Path) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    let runtime = env.runtime().to_path_buf();
    let path = path.to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_hfctl"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(["share", "--device", DEVICE, "--data-dir"])
                .arg(&data_dir)
                .arg(&path)
                .output()
                .expect("run hfctl")
        }),
    )
    .await
    .expect("hfctl finishes")
    .expect("the hfctl task joins")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

async fn join_script(script: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(5), script)
        .await
        .expect("the script finishes")
        .expect("the script joins");
}

#[tokio::test]
async fn hfctl_share_round_trips_over_the_socket() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let source_dir = tempfile::TempDir::new().expect("a temp source dir");
    let source = source_dir.path().join("report.pdf");
    std::fs::write(&source, b"payload").expect("write the source file");

    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::Share {
            device_id,
            path,
            reply,
        }) = command_rx.recv().await
        {
            let _ = observed_tx.send((device_id, path));
            let _ = reply.send(ControlResponse::Ok { bytes: 7 });
        }
    });
    assert!(
        control_path(data_dir.path()).exists(),
        "the control socket is bound"
    );

    let output = run_hfctl(&env, data_dir.path(), &source).await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("sent 7 bytes to {DEVICE}"),
        "hfctl prints the byte count and device"
    );

    let (device, path) = tokio::time::timeout(Duration::from_secs(5), observed_rx)
        .await
        .expect("the daemon sees the request")
        .expect("the script forwards the request");
    assert_eq!(device, device_id());
    assert_eq!(path, source);

    server.shutdown().await;
    join_script(script).await;
}

#[tokio::test]
async fn hfctl_reports_an_unknown_device() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let source_dir = tempfile::TempDir::new().expect("a temp source dir");
    let source = source_dir.path().join("report.pdf");
    std::fs::write(&source, b"payload").expect("write the source file");

    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::Share { reply, .. }) = command_rx.recv().await {
            let _ = reply.send(ControlResponse::Err {
                message: format!("device {DEVICE} is not connected"),
            });
        }
    });

    let output = run_hfctl(&env, data_dir.path(), &source).await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!("device {DEVICE} is not connected")),
        "stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "no success output");

    server.shutdown().await;
    join_script(script).await;
}

#[tokio::test]
async fn hfctl_reports_an_invalid_path() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let incoming = tempfile::TempDir::new().expect("a temp incoming dir");

    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let script = tokio::spawn(async move {
        let files = LocalFiles::new(incoming.path(), FileLimits::default())
            .expect("build the files adapter");
        if let Some(DaemonCommand::Share { path, reply, .. }) = command_rx.recv().await {
            let message = match files.open_outgoing(&path).await {
                Ok(_) => "unexpectedly opened a directory".to_string(),
                Err(error) => error.to_string(),
            };
            let _ = reply.send(ControlResponse::Err { message });
        }
    });

    let output = run_hfctl(&env, data_dir.path(), data_dir.path()).await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("not a regular file"),
        "stderr: {}",
        stderr(&output)
    );

    server.shutdown().await;
    join_script(script).await;
}

#[tokio::test]
async fn hfctl_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let source = data_dir.path().join("report.pdf");

    let output = run_hfctl(&env, data_dir.path(), &source).await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfastd is not running"),
        "stderr: {}",
        stderr(&output)
    );
}

#[tokio::test]
async fn hfctl_rejects_a_malformed_device_id() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let source = data_dir.path().join("report.pdf");
    let runtime = env.runtime().to_path_buf();
    let data_dir = data_dir.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_hfctl"))
            .env("XDG_RUNTIME_DIR", runtime)
            .args(["share", "--device", "not-a-device", "--data-dir"])
            .arg(&data_dir)
            .arg(&source)
            .output()
            .expect("run hfctl")
    })
    .await
    .expect("the hfctl task joins");

    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
}
