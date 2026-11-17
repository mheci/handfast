#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{
    CommandConfigAction, CommandSummary, ControlResponse, ControlServer, DaemonCommand,
};
use tokio::sync::{mpsc, oneshot};

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
    command.push("command".to_string());
    command.extend(args.iter().map(|arg| (*arg).to_string()));
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&command)
                .output()
                .expect("run handfast command")
        }),
    )
    .await
    .expect("handfast command finishes")
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
    action: Option<CommandConfigAction>,
}

async fn command_config(
    env: &TestEnv,
    data_dir: &Path,
    args: &[&str],
    commands: Vec<CommandSummary>,
    removed: u64,
) -> Outcome {
    let (requests, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, requests)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            if let DaemonCommand::CommandConfig { action, reply } = command {
                let response = match &action {
                    CommandConfigAction::List => ControlResponse::Commands {
                        commands: commands.clone(),
                    },
                    CommandConfigAction::Add { .. } => ControlResponse::Ok { bytes: 0 },
                    CommandConfigAction::Remove { .. } => ControlResponse::Ok { bytes: removed },
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
async fn command_list_prints_the_configured_commands() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = command_config(
        &env,
        data_dir.path(),
        &["list"],
        vec![CommandSummary {
            key: "abc".to_string(),
            name: "List files".to_string(),
            command: "ls -l".to_string(),
        }],
        0,
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    let text = stdout(&outcome.output);
    assert!(text.contains("abc"), "{text}");
    assert!(text.contains("List files"), "{text}");
    assert!(text.contains("ls -l"), "{text}");
    assert_eq!(outcome.action, Some(CommandConfigAction::List));
}

#[tokio::test]
async fn command_add_sends_the_argv_array() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = command_config(
        &env,
        data_dir.path(),
        &["add", "--name", "List files", "--", "ls", "-l"],
        Vec::new(),
        0,
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    let Some(CommandConfigAction::Add { key, name, argv }) = outcome.action else {
        panic!("the CLI sends an add: {:?}", outcome.action);
    };
    assert!(!key.is_empty(), "the CLI generates a key");
    assert_eq!(name, "List files");
    assert_eq!(argv, vec!["ls".to_string(), "-l".to_string()], "{argv:?}");
    assert!(stdout(&outcome.output).contains(&key), "the key is printed");
}

#[tokio::test]
async fn command_remove_reports_a_missing_command() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = command_config(&env, data_dir.path(), &["remove", "abc"], Vec::new(), 0).await;

    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("no command abc"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        outcome.action,
        Some(CommandConfigAction::Remove {
            key: "abc".to_string()
        })
    );
}

#[tokio::test]
async fn command_remove_prints_the_removed_key() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = command_config(&env, data_dir.path(), &["remove", "abc"], Vec::new(), 1).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(stdout(&outcome.output).trim(), "removed abc");
}

#[tokio::test]
async fn command_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &["list"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
}

#[tokio::test]
async fn command_add_needs_an_argv_after_the_separator() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &["add", "--name", "Empty"]).await;

    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("ARGV"),
        "stderr: {}",
        stderr(&output)
    );
}
