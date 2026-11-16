#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use handfast_daemon::control::{
    ControlResponse, ControlServer, DaemonCommand, DaemonStatus, DeviceSummary, IdentitySummary,
    PairedDevice, RemoteDevicePlayers, RemoteMediaCommand,
};
use handfast_proto::DeviceId;
use handfast_runtime::{RemoteMediaAction, RemotePlayer};
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

fn player() -> RemotePlayer {
    RemotePlayer {
        player: "mpv".to_string(),
        title: "Song".to_string(),
        artist: "Band".to_string(),
        album: "Record".to_string(),
        length_ms: 300_000,
        position_ms: 1_200,
        volume: 40,
        playing: true,
        can_seek: true,
        can_play: true,
        can_pause: true,
        can_go_previous: true,
        can_go_next: true,
        ..RemotePlayer::default()
    }
}

fn status_with(players: Vec<RemotePlayer>) -> DaemonStatus {
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
        remote_sinks: Vec::new(),
        remote_players: vec![RemoteDevicePlayers {
            device_id: device_id(PEER),
            players,
        }],
        truncated: false,
    }
}

async fn run_cli(env: &TestEnv, data_dir: &Path, args: &[&str]) -> std::process::Output {
    let data_dir = data_dir.to_path_buf();
    let runtime = env.runtime.path().to_path_buf();
    let mut media: Vec<String> = vec!["media".to_string()];
    media.extend(args.iter().map(|arg| (*arg).to_string()));
    media.push("--data-dir".to_string());
    media.push(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_handfast"))
                .env("XDG_RUNTIME_DIR", runtime)
                .args(&media)
                .output()
                .expect("run handfast media")
        }),
    )
    .await
    .expect("handfast media finishes")
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
    command: Option<RemoteMediaCommand>,
}

async fn media(env: &TestEnv, data_dir: &Path, args: &[&str], status: DaemonStatus) -> Outcome {
    let (commands, mut command_rx) = mpsc::channel(4);
    let server = ControlServer::start(data_dir, commands)
        .await
        .expect("start the control server");
    let (observed, observed_rx) = oneshot::channel();
    let script = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                DaemonCommand::Status { reply } => {
                    let _ = reply.send(ControlResponse::Status {
                        status: Box::new(status.clone()),
                    });
                }
                DaemonCommand::Media { command, reply, .. } => {
                    let _ = observed.send(command);
                    let _ = reply.send(ControlResponse::Ok { bytes: 0 });
                    break;
                }
                _ => {}
            }
        }
    });

    let output = run_cli(env, data_dir, args).await;
    server.shutdown().await;
    let command = observed_rx.await.ok();
    script.await.expect("the script joins");
    Outcome { output, command }
}

#[tokio::test]
async fn media_lists_the_reported_players() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = media(&env, data_dir.path(), &[PEER], status_with(vec![player()])).await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    let text = stdout(&outcome.output);
    assert!(text.contains("mpv"), "{text}");
    assert!(text.contains("playing"), "{text}");
    assert!(text.contains("Song - Band"), "{text}");
    assert!(text.contains("1200/300000 ms"), "{text}");
    assert!(text.contains("40%"), "{text}");
    assert!(outcome.command.is_none());
}

#[tokio::test]
async fn media_sends_an_action() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = media(
        &env,
        data_dir.path(),
        &[PEER, "--player", "mpv", "--action", "next"],
        status_with(vec![player()]),
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(stdout(&outcome.output).trim(), "sent media command to mpv");
    assert_eq!(
        outcome.command,
        Some(RemoteMediaCommand::Action {
            player: "mpv".to_string(),
            action: RemoteMediaAction::Next,
        })
    );
}

#[tokio::test]
async fn media_sets_the_volume() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = media(
        &env,
        data_dir.path(),
        &[PEER, "--player", "mpv", "--volume", "30"],
        status_with(vec![player()]),
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        outcome.command,
        Some(RemoteMediaCommand::SetVolume {
            player: "mpv".to_string(),
            volume: 30,
        })
    );
}

#[tokio::test]
async fn media_seeks_by_an_offset() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = media(
        &env,
        data_dir.path(),
        &[PEER, "--player", "mpv", "--seek", "-5000"],
        status_with(vec![player()]),
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        outcome.command,
        Some(RemoteMediaCommand::Seek {
            player: "mpv".to_string(),
            offset_us: -5000,
        })
    );
}

#[tokio::test]
async fn media_requests_the_player_list() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");

    let outcome = media(
        &env,
        data_dir.path(),
        &[PEER, "--list"],
        status_with(vec![player()]),
    )
    .await;

    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert_eq!(
        stdout(&outcome.output).trim(),
        format!("requested the player list from {PEER}")
    );
    assert_eq!(outcome.command, Some(RemoteMediaCommand::RequestList));
}

#[tokio::test]
async fn media_needs_a_player_when_there_are_two() {
    let env = TestEnv::new().await;
    let data_dir = tempfile::TempDir::new().expect("a temp data dir");
    let mut second = player();
    second.player = "spotify".to_string();

    let outcome = media(
        &env,
        data_dir.path(),
        &[PEER, "--action", "play"],
        status_with(vec![player(), second]),
    )
    .await;

    assert_eq!(
        outcome.output.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(
        stderr(&outcome.output).contains("needs --player"),
        "stderr: {}",
        stderr(&outcome.output)
    );
    assert!(outcome.command.is_none());
}

#[tokio::test]
async fn media_reports_a_missing_daemon() {
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
