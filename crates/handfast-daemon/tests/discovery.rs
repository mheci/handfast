use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use handfast_net::Discovery;
use handfast_proto::{types, Capabilities, DeviceId, DeviceName, DeviceType, Identity};
use handfast_store::Store;
use tokio::net::UdpSocket;

const DESKTOP_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

#[tokio::test]
async fn announced_devices_are_persisted() {
    let directory = tempfile::tempdir().unwrap();
    let (mut daemon, port) = spawn_daemon(directory.path());
    wait_for_state_file(directory.path(), &mut daemon).await;

    let peer = Identity::new(
        DeviceId::generate(),
        DeviceName::new("Discovery test phone"),
        DeviceType::Phone,
        Capabilities::new([types::PING], [types::PING]).unwrap(),
        Some(1716),
    );
    let sender = Discovery::bind(peer.clone(), 0, 1716).await.unwrap();
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
    let directory = tempfile::tempdir().unwrap();
    let (mut daemon, port) = spawn_daemon(directory.path());
    wait_for_state_file(directory.path(), &mut daemon).await;

    // The real desktop UDP packet carries only these four body fields.
    let datagram = format!(
        r#"{{"id":0,"type":"kdeconnect.identity","body":{{"deviceId":"{DESKTOP_ID}","deviceName":"kde-desktop","protocolVersion":8,"tcpPort":1716}}}}"#
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

fn spawn_daemon(directory: &Path) -> (Child, u16) {
    let port = free_udp_port();
    let daemon = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .arg("--data-dir")
        .arg(directory)
        .arg("--discovery-port")
        .arg(port.to_string())
        .arg("--tcp-port")
        .arg("1716")
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

fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap().port()
}
