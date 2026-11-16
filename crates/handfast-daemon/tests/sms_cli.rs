#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{
    ControlResponse, ControlServer, DaemonCommand, SmsAction, SmsConversationSummary,
    SmsMessageSummary,
};
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
    command.push("sms".to_string());
    command.extend(args.iter().map(|arg| (*arg).to_string()));
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&command)
                .output()
                .expect("run handfast sms")
        }),
    )
    .await
    .expect("handfast sms finishes")
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
    action: Option<SmsAction>,
}

async fn sms(env: &TestEnv, data_dir: &Path, args: &[&str]) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            if let DaemonCommand::Sms { action, reply, .. } = command {
                let response = match &action {
                    SmsAction::Conversations => ControlResponse::SmsConversations {
                        conversations: vec![SmsConversationSummary {
                            thread_id: 7,
                            address: "+123".to_string(),
                            body: "hello".to_string(),
                            date: 10,
                            unread: true,
                        }],
                    },
                    SmsAction::Thread { .. } => ControlResponse::SmsThread {
                        messages: vec![SmsMessageSummary {
                            address: "+123".to_string(),
                            body: "hello".to_string(),
                            date: 10,
                            outgoing: false,
                            read: true,
                        }],
                    },
                    SmsAction::Refresh
                    | SmsAction::RefreshThread { .. }
                    | SmsAction::Send { .. } => ControlResponse::Ok { bytes: 0 },
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
async fn sms_lists_the_cached_conversations() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sms(&env, data_dir.path(), &[PEER]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(stdout(&outcome.output).trim(), "7\t+123\thello");
    assert_eq!(outcome.action, Some(SmsAction::Conversations));
}

#[tokio::test]
async fn sms_lists_one_thread() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sms(&env, data_dir.path(), &[PEER, "--thread", "7"]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(stdout(&outcome.output).trim(), "10\tin\t+123\thello");
    assert_eq!(outcome.action, Some(SmsAction::Thread { thread_id: 7 }));
}

#[tokio::test]
async fn sms_refreshes_the_conversations() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sms(&env, data_dir.path(), &[PEER, "--refresh"]).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("requested conversations from {PEER}")
    );
    assert_eq!(outcome.action, Some(SmsAction::Refresh));
}

#[tokio::test]
async fn sms_refreshes_one_thread() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sms(
        &env,
        data_dir.path(),
        &[PEER, "--thread", "7", "--refresh-thread"],
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("requested thread 7 from {PEER}")
    );
    assert_eq!(
        outcome.action,
        Some(SmsAction::RefreshThread { thread_id: 7 })
    );
}

#[tokio::test]
async fn sms_sends_a_message() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sms(
        &env,
        data_dir.path(),
        &[PEER, "--send", "+123", "--body", "hello"],
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(stdout(&outcome.output).trim(), "sent SMS to +123");
    assert_eq!(
        outcome.action,
        Some(SmsAction::Send {
            address: "+123".to_string(),
            body: "hello".to_string(),
        })
    );
}

#[tokio::test]
async fn sms_send_needs_a_body() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sms(&env, data_dir.path(), &[PEER, "--send", "+123"]).await;

    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("--send needs --body"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(outcome.action.is_none());
}

#[tokio::test]
async fn sms_send_cannot_be_combined_with_a_thread() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = sms(
        &env,
        data_dir.path(),
        &[PEER, "--send", "+123", "--body", "hello", "--thread", "7"],
    )
    .await;

    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("cannot be combined"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(outcome.action.is_none());
}

#[tokio::test]
async fn sms_refuses_an_invalid_device_id() {
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
async fn sms_reports_a_missing_daemon() {
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
