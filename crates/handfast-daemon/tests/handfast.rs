//! Every test spawns the real binary, so the subcommand surface, the exit
//! codes and the printed packets are the contract a user sees.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

const LOG_ROTATE_BYTES: usize = 5 * 1024 * 1024;

fn handfast(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_handfast"))
        .args(args)
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .expect("run handfast")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn subcommands(help: &str) -> Vec<String> {
    let (_, commands) = help
        .split_once("Commands:")
        .expect("the help lists commands");
    commands
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

fn long_flags(help: &str) -> Vec<String> {
    let mut flags: Vec<String> = help
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
        .filter(|token| token.starts_with("--") && token.len() > 2)
        .filter(|token| *token != "--help" && *token != "--version")
        .map(str::to_string)
        .collect();
    flags.sort();
    flags.dedup();
    flags
}

#[test]
fn help_lists_the_daemon_share_identity_and_version_subcommands() {
    let output = handfast(&["--help"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let commands = subcommands(&stdout(&output));
    for command in ["daemon", "share", "service", "identity", "version"] {
        assert!(
            commands.iter().any(|listed| listed == command),
            "{command} missing from {commands:?}"
        );
    }
}

#[test]
fn service_help_lists_the_five_commands() {
    let output = handfast(&["service", "--help"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let commands = subcommands(&stdout(&output));
    for command in ["install", "uninstall", "start", "stop", "status"] {
        assert!(
            commands.iter().any(|listed| listed == command),
            "{command} missing from {commands:?}"
        );
    }
}

#[cfg(not(feature = "gui"))]
#[test]
fn no_subcommand_prints_help_and_exits_two() {
    // A GUI build opens the window here instead, so only the headless build
    // can run this rule.
    let output = handfast(&[]);
    assert_eq!(output.status.code(), Some(2));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("Usage: handfast"), "{text}");
}

#[test]
fn daemon_help_lists_the_six_flags() {
    let output = handfast(&["daemon", "--help"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let help = stdout(&output);
    let flags = long_flags(&help);
    for flag in [
        "--incoming-dir",
        "--tcp-port",
        "--discovery-port",
        "--log-file",
        "--print-identity",
    ] {
        assert!(
            flags.iter().any(|listed| listed == flag),
            "{flag} missing from {flags:?}"
        );
    }

    let root = handfast(&["--help"]);
    assert!(root.status.success(), "stderr: {}", stderr(&root));
    let root_flags = long_flags(&stdout(&root));
    assert!(
        root_flags.iter().any(|listed| listed == "--data-dir"),
        "--data-dir is global: {root_flags:?}"
    );
}

#[test]
fn identity_matches_the_daemon_print_path_byte_for_byte() {
    let directory = tempfile::tempdir().unwrap();
    let data_dir = directory.path().to_str().unwrap();

    for tcp_port in [None, Some("1721")] {
        let expected = tcp_port.unwrap_or("1716");
        let mut identity_args = vec!["identity", "--data-dir", data_dir];
        let mut printed_args = vec!["daemon", "--print-identity", "--data-dir", data_dir];
        if let Some(port) = tcp_port {
            identity_args.extend(["--tcp-port", port]);
            printed_args.extend(["--tcp-port", port]);
        }

        let identity = handfast(&identity_args);
        assert!(identity.status.success(), "stderr: {}", stderr(&identity));
        let printed = handfast(&printed_args);
        assert!(printed.status.success(), "stderr: {}", stderr(&printed));

        assert_eq!(
            identity.stdout, printed.stdout,
            "handfast identity and handfast daemon --print-identity print the same packet at {expected}"
        );
        let packet: Value = serde_json::from_slice(&identity.stdout).expect("a JSON packet");
        assert_eq!(
            packet["body"]["tcpPort"],
            serde_json::json!(expected.parse::<u16>().unwrap())
        );
        assert!(packet["body"]["deviceId"].as_str().is_some(), "{packet}");
    }
}

#[test]
fn data_dir_is_global_before_and_after_the_subcommand() {
    let directory = tempfile::tempdir().unwrap();
    let data_dir = directory.path().to_str().unwrap();

    let before = handfast(&["--data-dir", data_dir, "identity"]);
    assert!(before.status.success(), "stderr: {}", stderr(&before));
    let after = handfast(&["identity", "--data-dir", data_dir]);
    assert!(after.status.success(), "stderr: {}", stderr(&after));

    assert_eq!(
        before.stdout, after.stdout,
        "the global --data-dir resolves to the same store in both positions"
    );
}

#[test]
fn version_prints_the_package_version() {
    let expected = format!("handfast {}", env!("CARGO_PKG_VERSION"));
    let subcommand = handfast(&["version"]);
    assert!(
        subcommand.status.success(),
        "stderr: {}",
        stderr(&subcommand)
    );
    assert_eq!(stdout(&subcommand).trim(), expected);

    let flag = handfast(&["--version"]);
    assert!(flag.status.success(), "stderr: {}", stderr(&flag));
    assert_eq!(stdout(&flag).trim(), expected);
}

#[cfg(unix)]
#[test]
fn share_without_a_daemon_exits_one_and_reports_not_running() {
    // The Windows control pipe is per user, so a live daemon would answer this
    // request instead of the missing endpoint.
    let directory = tempfile::tempdir().unwrap();
    let data_dir = directory.path().to_str().unwrap();
    let source = directory.path().join("report.txt");
    std::fs::write(&source, b"report").unwrap();

    let output = handfast(&[
        "share",
        "--device",
        "fedcba9876543210fedcba9876543210",
        "--data-dir",
        data_dir,
        source.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("handfast is not running"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn daemon_subcommand_binds_the_tcp_listener_and_writes_the_log_file() {
    let directory = tempfile::tempdir().unwrap();
    let stderr_log = directory.path().join("daemon.log");
    let log_file = directory.path().join("handfast.log");
    std::fs::write(&log_file, vec![b'x'; LOG_ROTATE_BYTES + 1]).expect("seed an oversized log");
    let stderr_file = std::fs::File::create(&stderr_log).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_handfast"))
        .args([
            "daemon",
            "--data-dir",
            directory.path().to_str().unwrap(),
            "--incoming-dir",
            directory.path().join("incoming").to_str().unwrap(),
            "--tcp-port",
            &free_tcp_port().to_string(),
            "--discovery-port",
            &free_udp_port().to_string(),
            "--log-file",
            log_file.to_str().unwrap(),
        ])
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("spawn handfast daemon");

    wait_for_log(&stderr_log, "TCP listener bound", &mut daemon);
    let written = std::fs::read_to_string(&log_file).expect("read the log file");
    assert!(
        written.contains("TCP listener bound"),
        "the log file carries the daemon lines: {written}"
    );
    let mut rotated = log_file.as_os_str().to_os_string();
    rotated.push(".1");
    assert!(
        Path::new(&rotated).exists(),
        "an oversized log rotates to <name>.1"
    );

    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn daemon_creates_a_missing_log_directory() {
    let directory = tempfile::tempdir().unwrap();
    let stderr_log = directory.path().join("daemon.log");
    let log_file = directory.path().join("logs").join("handfast.log");
    assert!(!log_file.parent().unwrap().exists());
    let stderr_file = std::fs::File::create(&stderr_log).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_handfast"))
        .args([
            "daemon",
            "--data-dir",
            directory.path().join("state").to_str().unwrap(),
            "--incoming-dir",
            directory.path().join("incoming").to_str().unwrap(),
            "--tcp-port",
            &free_tcp_port().to_string(),
            "--discovery-port",
            &free_udp_port().to_string(),
            "--log-file",
            log_file.to_str().unwrap(),
        ])
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("spawn handfast daemon");

    wait_for_log(&stderr_log, "TCP listener bound", &mut daemon);
    let written = std::fs::read_to_string(&log_file).expect("read the log file");
    assert!(
        written.contains("TCP listener bound"),
        "the log file's parent is created: {written}"
    );

    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

fn wait_for_log(log: &Path, needle: &str, daemon: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains(needle) {
            return;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfast daemon exited while waiting for {needle:?}: {text}"
        );
        assert!(
            Instant::now() < deadline,
            "the log never contained {needle:?}: {text}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn free_tcp_port() -> u16 {
    for port in handfast_proto::MIN_TCP_PORT..=handfast_proto::MAX_TCP_PORT {
        if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
            drop(listener);
            return port;
        }
    }
    panic!("no free TCP port in the KDE Connect range");
}

fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    socket.local_addr().unwrap().port()
}
