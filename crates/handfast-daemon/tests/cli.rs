use std::net::TcpListener;
use std::process::Command;

use handfast_proto::{MAX_TCP_PORT, MIN_TCP_PORT};
use handfast_store::Store;
use serde_json::Value;

#[test]
fn print_identity_is_stable_and_advertises_implemented_plugins() {
    let directory = tempfile::tempdir().unwrap();
    let run = || {
        let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
            .args([
                "--print-identity",
                "--data-dir",
                directory.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let packet: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            packet["type"],
            Value::String("kdeconnect.identity".to_string())
        );
        assert_eq!(packet["body"]["protocolVersion"], serde_json::json!(8));
        assert_eq!(
            packet["body"]["deviceType"],
            Value::String("desktop".to_string())
        );
        let incoming = packet["body"]["incomingCapabilities"].as_array().unwrap();
        assert!(incoming.contains(&Value::String("kdeconnect.ping".to_string())));
        assert!(!incoming.contains(&Value::String("kdeconnect.clipboard".to_string())));
        assert_eq!(packet["body"]["tcpPort"], serde_json::json!(1716));
        packet["body"]["deviceId"].as_str().unwrap().to_string()
    };

    let first = run();
    let second = run();
    assert_eq!(first, second, "identity must persist across starts");
}

#[test]
fn print_identity_does_not_generate_a_certificate_or_bind_the_port() {
    let directory = tempfile::tempdir().unwrap();
    let (listener, port) = occupy_tcp_port();
    let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .args([
            "--print-identity",
            "--data-dir",
            directory.path().to_str().unwrap(),
            "--tcp-port",
            &port.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the occupied port proves the command did not bind: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let packet: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(packet["body"]["tcpPort"], serde_json::json!(port));
    drop(listener);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    assert!(
        store.identity().unwrap().is_none(),
        "print-identity must not write identity_v1"
    );
}

#[test]
fn tcp_port_outside_the_range_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    for port in ["0", "1700", "1765"] {
        let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
            .args([
                "--print-identity",
                "--data-dir",
                directory.path().to_str().unwrap(),
                "--tcp-port",
                port,
            ])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{port} must be rejected");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("outside 1716-1764"),
            "the error names the range: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn print_identity_creates_no_incoming_directory() {
    let directory = tempfile::tempdir().unwrap();
    let incoming = directory.path().join("incoming");
    let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .args([
            "--print-identity",
            "--data-dir",
            directory.path().to_str().unwrap(),
            "--incoming-dir",
            incoming.to_str().unwrap(),
        ])
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !incoming.exists(),
        "print-identity returns before LocalFiles and creates no incoming directory"
    );
    assert!(
        !incoming.join(".handfast-tmp").exists(),
        "print-identity creates no private temp directory"
    );
    assert!(
        !directory.path().join("handfastd.sock").exists(),
        "print-identity opens no control socket"
    );
}

#[test]
fn print_identity_keeps_share_out_of_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .args([
            "--print-identity",
            "--data-dir",
            directory.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let packet: Value = serde_json::from_slice(&output.stdout).unwrap();
    for key in ["incomingCapabilities", "outgoingCapabilities"] {
        let capabilities = packet["body"][key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} is an array"));
        assert!(
            capabilities.contains(&Value::String("kdeconnect.ping".to_string())),
            "the print branch still advertises the default desktop: {capabilities:?}"
        );
        assert!(
            !capabilities.contains(&Value::String("kdeconnect.share.request".to_string())),
            "print-identity keeps share out of {key}: {capabilities:?}"
        );
        assert!(
            !capabilities.contains(&Value::String(
                "kdeconnect.share.request.update".to_string()
            )),
            "print-identity keeps share updates out of {key}: {capabilities:?}"
        );
    }
}

fn occupy_tcp_port() -> (TcpListener, u16) {
    for port in MIN_TCP_PORT..=MAX_TCP_PORT {
        if let Ok(listener) = TcpListener::bind(("0.0.0.0", port)) {
            return (listener, port);
        }
    }
    panic!("no free TCP port in {MIN_TCP_PORT}-{MAX_TCP_PORT}");
}

#[test]
fn version_flag_works() {
    let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("handfastd"));
}
