#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{
    ControlResponse, ControlServer, DaemonCommand, DaemonStatus, IdentitySummary, PairingSummary,
};
use handfast_proto::DeviceId;
use tokio::sync::{mpsc, oneshot};

const DEVICE: &str = "0123456789abcdef0123456789abcdef";

// Serializes the process-global `XDG_RUNTIME_DIR` change.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serializes the tests and gives each one its own runtime directory.
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

fn row_fingerprint() -> String {
    "cd".repeat(32)
}

fn pending() -> PairingSummary {
    PairingSummary {
        device_id: device_id(),
        device_name: "Pixel".to_string(),
        fingerprint: row_fingerprint(),
    }
}

fn status_with_pending(pending: Vec<PairingSummary>) -> DaemonStatus {
    DaemonStatus {
        version: "0.3.0".to_string(),
        pid: 4242,
        uptime_secs: 17,
        identity: IdentitySummary {
            device_id: device_id(),
            device_name: "Laptop".to_string(),
            device_type: "desktop".to_string(),
            fingerprint: "ab".repeat(32),
            tcp_port: 1716,
        },
        incoming_dir: PathBuf::from("/home/me/Downloads"),
        connected: Vec::new(),
        paired: Vec::new(),
        pending_pairings: pending,
        transfers: Vec::new(),
        remote_sinks: Vec::new(),
        remote_players: Vec::new(),
        truncated: false,
    }
}

/// Run `handfast pair` with the arguments after the subcommand name.
async fn run_cli(env: &TestEnv, data_dir: &Path, args: &[&str]) -> std::process::Output {
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

struct Outcome {
    output: std::process::Output,
    decision: Option<(DeviceId, bool, String)>,
}

/// Serve one status snapshot and one decision reply, then run the CLI.
async fn pair(
    env: &TestEnv,
    data_dir: &Path,
    args: &[&str],
    status: DaemonStatus,
    reply_with: ControlResponse,
) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::Status { reply }) = command_rx.recv().await {
            let _ = reply.send(ControlResponse::Status {
                status: Box::new(status),
            });
        }
        if let Some(DaemonCommand::PairingDecision {
            device_id,
            accept,
            fingerprint,
            reply,
        }) = command_rx.recv().await
        {
            let _ = observed.send((device_id, accept, fingerprint));
            let _ = reply.send(reply_with);
        }
    });

    let output = run_cli(env, data_dir, args).await;
    server.shutdown().await;
    let decision = observed_rx.await.ok();
    script.await.expect("the script joins");
    Outcome { output, decision }
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

    let outcome = pair(
        &env,
        data_dir.path(),
        &["--accept", DEVICE],
        status_with_pending(vec![pending()]),
        ControlResponse::Decided,
    )
    .await;
    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("pairing accepted for {DEVICE}")
    );

    let (device, accept, fingerprint) = outcome.decision.expect("the CLI sends a decision");
    assert_eq!(device, device_id());
    assert!(accept);
    assert_eq!(
        fingerprint,
        row_fingerprint(),
        "the row fingerprint travels"
    );
}

#[tokio::test]
async fn pair_rejects_over_the_socket() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = pair(
        &env,
        data_dir.path(),
        &["--reject", DEVICE],
        status_with_pending(vec![pending()]),
        ControlResponse::Decided,
    )
    .await;
    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("pairing rejected for {DEVICE}")
    );

    let (device, accept, fingerprint) = outcome.decision.expect("the CLI sends a decision");
    assert_eq!(device, device_id());
    assert!(!accept);
    assert_eq!(fingerprint, row_fingerprint());
}

#[tokio::test]
async fn pair_reports_a_changed_request() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = pair(
        &env,
        data_dir.path(),
        &["--accept", DEVICE],
        status_with_pending(vec![pending()]),
        ControlResponse::Err {
            message: "the pairing request changed; refresh and decide again".to_string(),
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
        stderr(&outcome.output).contains("pairing request changed"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(stdout(&outcome.output).is_empty(), "no success output");
    assert!(
        outcome.decision.is_some(),
        "the CLI sends the fingerprint it saw"
    );
}

#[tokio::test]
async fn pair_reports_no_pending_request() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = pair(
        &env,
        data_dir.path(),
        &["--accept", DEVICE],
        status_with_pending(Vec::new()),
        ControlResponse::Decided,
    )
    .await;
    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("no pairing request"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(stdout(&outcome.output).is_empty(), "no success output");
    assert!(
        outcome.decision.is_none(),
        "the CLI must not send a decision without a pending row"
    );
}

#[tokio::test]
async fn pair_reports_a_missing_daemon() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &["--reject", DEVICE]).await;
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

    let output = run_cli(&env, data_dir.path(), &[]).await;
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));

    let output = run_cli(
        &env,
        data_dir.path(),
        &["--accept", DEVICE, "--reject", DEVICE],
    )
    .await;
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
}
