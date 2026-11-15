use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{
    ControlResponse, ControlServer, DaemonCommand, DaemonStatus, DeviceSummary, IdentitySummary,
    PairedDevice, TransferSummary,
};
use handfast_proto::DeviceId;
use serde_json::{json, Value};
use tokio::sync::mpsc;

const DEVICE: &str = "0123456789abcdef0123456789abcdef";
const PEER: &str = "fedcba9876543210fedcba9876543210";

// Serializes the process-global `XDG_RUNTIME_DIR` change. The Windows pipe is
// per user, so the two tests must not use the endpoint at the same time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serializes the tests and gives each one its own runtime directory on Unix.
struct TestEnv {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    _runtime: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let guard = SERIAL.lock().await;
        let runtime = tempfile::TempDir::new().expect("a temp runtime dir");
        std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
        Self {
            _guard: guard,
            _runtime: runtime,
        }
    }
}

fn device_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("a valid device id")
}

fn sample_status() -> DaemonStatus {
    DaemonStatus {
        version: "0.2.0".to_string(),
        pid: 4242,
        uptime_secs: 17,
        identity: IdentitySummary {
            device_id: device_id(DEVICE),
            device_name: "Laptop".to_string(),
            device_type: "desktop".to_string(),
            fingerprint: "ab".repeat(32),
        },
        incoming_dir: PathBuf::from("/home/me/Downloads"),
        connected: vec![DeviceSummary {
            device_id: device_id(PEER),
            device_name: "Pixel".to_string(),
            device_type: "phone".to_string(),
        }],
        paired: vec![PairedDevice {
            device_id: device_id(PEER),
            device_name: "Pixel".to_string(),
            device_type: "phone".to_string(),
            connected: true,
        }],
        transfers: vec![TransferSummary {
            device_id: device_id(PEER),
            transfer_id: 7,
            bytes: 1024,
            total: Some(4096),
        }],
        truncated: false,
    }
}

async fn run_status(data_dir: &Path, json: bool) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_handfast"));
            command.args(["status", "--data-dir"]).arg(&data_dir);
            if json {
                command.arg("--json");
            }
            command.output().expect("run handfast status")
        }),
    )
    .await
    .expect("handfast status finishes")
    .expect("the handfast task joins")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[tokio::test]
async fn status_prints_human_fields_rows_and_json() {
    let _env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let script = tokio::spawn(async move {
        for _ in 0..2 {
            match command_rx.recv().await {
                Some(DaemonCommand::Status { reply }) => {
                    let _ = reply.send(ControlResponse::Status {
                        status: Box::new(sample_status()),
                    });
                }
                other => panic!("expected a status command, got {other:?}"),
            }
        }
    });

    let output = run_status(data_dir.path(), false).await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let human = stdout(&output);
    let expected = [
        "version: 0.2.0".to_string(),
        "pid: 4242".to_string(),
        "uptime: 17s".to_string(),
        format!("device id: {DEVICE}"),
        "device name: Laptop".to_string(),
        "device type: desktop".to_string(),
        format!("fingerprint: {}", "ab".repeat(32)),
        "incoming dir: /home/me/Downloads".to_string(),
        "connected: 1".to_string(),
        "paired: 1".to_string(),
        "transfers: 1".to_string(),
    ];
    for line in expected {
        assert!(human.contains(&line), "missing {line:?} in:\n{human}");
    }
    assert!(
        human.contains(&format!("  {PEER}  Pixel  phone  connected")),
        "the paired row marks the open link:\n{human}"
    );
    assert!(
        human.contains(&format!("  {PEER}  7  1024/4096")),
        "the transfer row shows progress:\n{human}"
    );

    let output = run_status(data_dir.path(), true).await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let status: Value = serde_json::from_slice(&output.stdout).expect("status json");
    assert_eq!(status["pid"], json!(4242));
    assert_eq!(status["identity"]["device_id"], json!(DEVICE));
    assert_eq!(status["identity"]["device_name"], json!("Laptop"));
    assert_eq!(status["identity"]["device_type"], json!("desktop"));
    assert_eq!(status["identity"]["fingerprint"], json!("ab".repeat(32)));
    assert_eq!(status["incoming_dir"], json!("/home/me/Downloads"));
    assert_eq!(status["connected"][0]["device_id"], json!(PEER));
    assert_eq!(status["paired"][0]["connected"], json!(true));
    assert_eq!(status["transfers"][0]["transfer_id"], json!(7));
    assert_eq!(status["transfers"][0]["total"], json!(4096));
    assert_eq!(status["truncated"], json!(false));

    server.shutdown().await;
    tokio::time::timeout(Duration::from_secs(5), script)
        .await
        .expect("the script finishes")
        .expect("the script joins");
}

#[tokio::test]
async fn status_reports_a_missing_daemon() {
    let _env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_status(data_dir.path(), false).await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "no status output");
}
