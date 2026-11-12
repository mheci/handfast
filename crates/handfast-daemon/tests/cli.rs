use std::process::Command;

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
fn version_flag_works() {
    let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("handfastd"));
}
