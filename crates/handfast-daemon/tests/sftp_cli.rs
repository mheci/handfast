#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{ControlResponse, ControlServer, DaemonCommand, SftpAction};
use handfast_runtime::SftpEntry;
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
    command.push("sftp".to_string());
    command.extend(args.iter().map(|arg| (*arg).to_string()));
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&command)
                .output()
                .expect("run handfast sftp")
        }),
    )
    .await
    .expect("handfast sftp finishes")
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
    action: Option<SftpAction>,
}

async fn sftp(env: &TestEnv, data_dir: &Path, args: &[&str]) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            if let DaemonCommand::Sftp { action, reply, .. } = command {
                let response = match &action {
                    SftpAction::List { .. } => ControlResponse::SftpEntries {
                        entries: vec![SftpEntry {
                            name: "file.txt".to_string(),
                            directory: false,
                            size: 12,
                        }],
                    },
                    SftpAction::Download { .. } | SftpAction::Upload { .. } => {
                        ControlResponse::Ok { bytes: 5 }
                    }
                    SftpAction::Start => ControlResponse::Ok { bytes: 0 },
                };
                let _ = observed.send(action);
                let _ = reply.send(response);
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
async fn sftp_starts_the_server() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sftp(&env, data_dir.path(), &[PEER, "--start"]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("requested SFTP from {PEER}; retry --list after the phone responds")
    );
    assert_eq!(outcome.action, Some(SftpAction::Start));
}

#[tokio::test]
async fn sftp_lists_a_directory() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sftp(&env, data_dir.path(), &[PEER, "--list", "/"]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(stdout(&outcome.output).trim(), "file.txt\tfile\t12");
    assert_eq!(
        outcome.action,
        Some(SftpAction::List {
            path: "/".to_string()
        })
    );
}

#[tokio::test]
async fn sftp_downloads_a_file() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sftp(
        &env,
        data_dir.path(),
        &[PEER, "--download", "/file.txt", "file.txt"],
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        "downloaded /file.txt to file.txt (5 bytes)"
    );
    assert_eq!(
        outcome.action,
        Some(SftpAction::Download {
            remote: "/file.txt".to_string(),
            local: PathBuf::from("file.txt"),
        })
    );
}

#[tokio::test]
async fn sftp_uploads_a_file() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sftp(
        &env,
        data_dir.path(),
        &[PEER, "--upload", "file.txt", "/file.txt"],
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        "uploaded file.txt to /file.txt (5 bytes)"
    );
    assert_eq!(
        outcome.action,
        Some(SftpAction::Upload {
            local: PathBuf::from("file.txt"),
            remote: "/file.txt".to_string(),
        })
    );
}

#[tokio::test]
async fn sftp_needs_one_operation() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sftp(&env, data_dir.path(), &[PEER, "--start", "--list", "/"]).await;

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
async fn sftp_refuses_an_invalid_device_id() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &["not-a-device", "--start"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("is not a device ID"),
        "stderr: {}",
        stderr(&output)
    );
}

#[tokio::test]
async fn sftp_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[PEER, "--start"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
}
