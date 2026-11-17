#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{
    ControlResponse, ControlServer, DaemonCommand, DaemonStatus, DeviceSummary, IdentitySummary,
    PairedDevice, RemoteDeviceSinks,
};
use handfast_proto::DeviceId;
use handfast_runtime::RemoteSink;
use tokio::sync::{mpsc, oneshot};

const DEVICE: &str = "0123456789abcdef0123456789abcdef";
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

fn device_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("a valid device id")
}

fn sink() -> RemoteSink {
    RemoteSink {
        name: "speakers".to_string(),
        description: "Built-in speakers".to_string(),
        volume: 26214,
        muted: false,
        max_volume: 65536,
        enabled: true,
    }
}

fn status_with_sink() -> DaemonStatus {
    DaemonStatus {
        version: "0.3.0".to_string(),
        pid: 4242,
        uptime_secs: 17,
        identity: IdentitySummary {
            device_id: device_id(DEVICE),
            device_name: "Laptop".to_string(),
            device_type: "desktop".to_string(),
            fingerprint: "ab".repeat(32),
            tcp_port: 1716,
        },
        incoming_dir: PathBuf::from("/home/me/Downloads"),
        connected: vec![DeviceSummary {
            device_id: device_id(PEER),
            device_name: "Other desktop".to_string(),
            device_type: "desktop".to_string(),
            connected: true,
        }],
        paired: vec![PairedDevice {
            device_id: device_id(PEER),
            device_name: "Other desktop".to_string(),
            device_type: "desktop".to_string(),
            connected: true,
        }],
        pending_pairings: Vec::new(),
        transfers: Vec::new(),
        remote_sinks: vec![RemoteDeviceSinks {
            device_id: device_id(PEER),
            sinks: vec![sink()],
        }],
        remote_players: Vec::new(),
        remote_commands: Vec::new(),
        truncated: false,
    }
}

async fn run_cli(env: &TestEnv, data_dir: &Path, args: &[&str]) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    let runtime = env.runtime.path().to_path_buf();
    let mut volume: Vec<String> = vec!["volume".to_string()];
    volume.extend(args.iter().map(|arg| (*arg).to_string()));
    volume.push("--data-dir".to_string());
    volume.push(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&volume)
                .output()
                .expect("run handfast volume")
        }),
    )
    .await
    .expect("handfast volume finishes")
    .expect("the handfast task joins")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[tokio::test]
async fn volume_lists_the_reported_sinks() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::Status { reply }) = command_rx.recv().await {
            let _ = reply.send(ControlResponse::Status {
                status: Box::new(status_with_sink()),
            });
        }
    });

    let output = run_cli(&env, data_dir.path(), &[PEER]).await;
    server.shutdown().await;
    script.await.expect("the script joins");

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("speakers"), "{text}");
    assert!(text.contains("40%"), "{text}");
    assert!(text.contains("unmuted"), "{text}");
    assert!(text.contains("Built-in speakers"), "{text}");
    assert!(text.contains("(default)"), "{text}");
}

#[tokio::test]
async fn volume_sets_a_percent() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::RemoteVolume {
            device_id,
            sink,
            percent,
            muted,
            reply,
        }) = command_rx.recv().await
        {
            let _ = observed.send((device_id, sink, percent, muted));
            let _ = reply.send(ControlResponse::Ok { bytes: 0 });
        }
    });

    let output = run_cli(
        &env,
        data_dir.path(),
        &[PEER, "--sink", "speakers", "--percent", "40"],
    )
    .await;
    server.shutdown().await;
    let observed = observed_rx.await.expect("the CLI sends a request");
    script.await.expect("the script joins");

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output).trim(), format!("set speakers on {PEER}"));
    assert_eq!(observed.0, device_id(PEER));
    assert_eq!(observed.1, "speakers");
    assert_eq!(observed.2, Some(40));
    assert_eq!(observed.3, None);
}

#[tokio::test]
async fn volume_mutes() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir.path(), commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        if let Some(DaemonCommand::RemoteVolume {
            device_id,
            sink,
            percent,
            muted,
            reply,
        }) = command_rx.recv().await
        {
            let _ = observed.send((device_id, sink, percent, muted));
            let _ = reply.send(ControlResponse::Ok { bytes: 0 });
        }
    });

    let output = run_cli(
        &env,
        data_dir.path(),
        &[PEER, "--sink", "speakers", "--mute"],
    )
    .await;
    server.shutdown().await;
    let observed = observed_rx.await.expect("the CLI sends a request");
    script.await.expect("the script joins");

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(observed.2, None);
    assert_eq!(observed.3, Some(true));
}

#[tokio::test]
async fn volume_needs_a_sink_for_a_change() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let output = run_cli(&env, data_dir.path(), &[PEER, "--percent", "40"]).await;

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("needs --sink"),
        "stderr: {}",
        stderr(&output)
    );
}

#[tokio::test]
async fn volume_reports_a_missing_daemon() {
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
