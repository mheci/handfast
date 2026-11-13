use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use handfast_net::Discovery;
use handfast_proto::{
    types, Capabilities, DeviceId, DeviceName, DeviceType, Identity, MAX_TCP_PORT, MIN_TCP_PORT,
};
use handfast_store::Store;
use serde_json::Value;
use tokio::net::UdpSocket;

const DESKTOP_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

// The daemon tests share the fixed KDE Connect TCP range. Run them one at a
// time so port scans cannot race each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

#[tokio::test]
async fn announced_devices_are_persisted() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let (mut daemon, port) = spawn_daemon(directory.path(), tcp_port);
    wait_for_state_file(directory.path(), &mut daemon).await;

    let peer_port = free_tcp_port();
    let peer = Identity::new(
        DeviceId::generate(),
        DeviceName::new("Discovery test phone"),
        DeviceType::Phone,
        Capabilities::new([types::PING], [types::PING]).unwrap(),
        Some(peer_port),
    );
    let sender = Discovery::bind(peer.clone(), 0, peer_port).await.unwrap();
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

    // The discovery socket binds just after the state file, so announce
    // repeatedly. A lost datagram is harmless.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited before receiving an announcement"
        );
        let _ = sender.announce(target).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    stop(daemon);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    let record = store
        .device(peer.device_id.as_str())
        .unwrap()
        .expect("the daemon persisted the discovered device");
    assert_eq!(record.name, "Discovery test phone");
    assert_eq!(record.device_type, "phone");
    assert!(!record.paired);
    assert!(record.last_seen.unwrap_or_default() > 0);
}

#[tokio::test]
async fn desktop_announcements_are_persisted() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let (mut daemon, port) = spawn_daemon(directory.path(), tcp_port);
    wait_for_state_file(directory.path(), &mut daemon).await;

    // The real desktop UDP packet carries only these four body fields.
    let peer_port = free_tcp_port();
    let datagram = format!(
        r#"{{"id":0,"type":"kdeconnect.identity","body":{{"deviceId":"{DESKTOP_ID}","deviceName":"kde-desktop","protocolVersion":8,"tcpPort":{peer_port}}}}}"#
    );
    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited before receiving an announcement"
        );
        let _ = sender.send_to(datagram.as_bytes(), target).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    stop(daemon);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    let record = store
        .device(DESKTOP_ID)
        .unwrap()
        .expect("the daemon persisted the desktop announcement");
    assert_eq!(record.name, "kde-desktop");
    assert_eq!(record.device_type, "unknown");
    assert!(!record.paired);
    assert!(record.last_seen.unwrap_or_default() > 0);
}

#[tokio::test]
async fn self_announcements_are_ignored() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"]
        .as_str()
        .expect("the printed identity has a device ID")
        .to_string();
    let (mut daemon, port) = spawn_daemon(directory.path(), tcp_port);
    wait_for_state_file(directory.path(), &mut daemon).await;

    let peer_port = free_tcp_port();
    let datagram = format!(
        r#"{{"id":0,"type":"kdeconnect.identity","body":{{"deviceId":"{daemon_id}","deviceName":"self","protocolVersion":8,"tcpPort":{peer_port}}}}}"#
    );
    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    for _ in 0..20 {
        let _ = sender.send_to(datagram.as_bytes(), target).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    stop(daemon);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    assert!(
        store.device(&daemon_id).unwrap().is_none(),
        "the daemon must not persist or dial its own announcement"
    );
}

fn print_identity(directory: &Path, tcp_port: u16) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .args([
            "--print-identity",
            "--data-dir",
            directory.to_str().unwrap(),
            "--tcp-port",
            &tcp_port.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn spawn_daemon(directory: &Path, tcp_port: u16) -> (Child, u16) {
    let port = free_udp_port();
    let daemon = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .arg("--data-dir")
        .arg(directory)
        .arg("--discovery-port")
        .arg(port.to_string())
        .arg("--tcp-port")
        .arg(tcp_port.to_string())
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    (daemon, port)
}

// The daemon opens its state file before binding the discovery socket, so
// wait for the file instead of a fixed startup delay.
async fn wait_for_state_file(directory: &Path, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !directory.join("state.db").exists() {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited before opening its state file"
        );
        assert!(
            Instant::now() < deadline,
            "handfastd did not open its state file"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn stop(mut daemon: Child) {
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

fn free_tcp_port() -> u16 {
    for port in MIN_TCP_PORT..=MAX_TCP_PORT {
        if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
            drop(listener);
            return port;
        }
    }
    panic!("no free TCP port in {MIN_TCP_PORT}-{MAX_TCP_PORT}");
}

fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap().port()
}
