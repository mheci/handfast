#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{ContactSummary, ControlResponse, ControlServer, DaemonCommand};
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
    command.push("contacts".to_string());
    command.extend(args.iter().map(|arg| (*arg).to_string()));
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&command)
                .output()
                .expect("run handfast contacts")
        }),
    )
    .await
    .expect("handfast contacts finishes")
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
    refresh: Option<bool>,
}

async fn contacts(
    env: &TestEnv,
    data_dir: &Path,
    args: &[&str],
    contacts: Vec<ContactSummary>,
) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            if let DaemonCommand::Contacts {
                device_id: _,
                refresh,
                reply,
            } = command
            {
                let _ = observed.send(refresh);
                let response = if refresh {
                    ControlResponse::Ok { bytes: 0 }
                } else {
                    ControlResponse::Contacts {
                        contacts: contacts.clone(),
                    }
                };
                let _ = reply.send(response);
                break;
            }
        }
    });

    let output = run_cli(env, data_dir, args).await;
    server.shutdown().await;
    let refresh = observed_rx.await.ok();
    script.await.expect("the script joins");
    Outcome { output, refresh }
}

fn jane() -> ContactSummary {
    ContactSummary {
        uid: "uid-1".to_string(),
        name: "Jane Doe".to_string(),
        phones: vec!["+123".to_string()],
        emails: vec!["jane@example.com".to_string()],
    }
}

#[tokio::test]
async fn contacts_lists_the_cached_contacts() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = contacts(&env, data_dir.path(), &[PEER], vec![jane()]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        "Jane Doe\t+123\tjane@example.com"
    );
    assert_eq!(outcome.refresh, Some(false));
}

#[tokio::test]
async fn contacts_refresh_sends_the_request() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = contacts(&env, data_dir.path(), &[PEER, "--refresh"], Vec::new()).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("requested contacts from {PEER}")
    );
    assert_eq!(outcome.refresh, Some(true));
}

#[tokio::test]
async fn contacts_lists_nothing_for_an_empty_cache() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = contacts(&env, data_dir.path(), &[PEER], Vec::new()).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(stdout(&outcome.output).trim().is_empty());
}

#[tokio::test]
async fn contacts_refuses_an_invalid_device_id() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &["not-a-device"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("is not a device ID"),
        "stderr: {}",
        stderr(&output)
    );
}

#[tokio::test]
async fn contacts_reports_a_missing_daemon() {
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
