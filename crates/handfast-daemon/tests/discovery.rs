use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Command, Stdio};
use std::time::Duration;

use handfast_net::Discovery;
use handfast_proto::{types, Capabilities, DeviceId, DeviceName, DeviceType, Identity};
use handfast_store::Store;

#[tokio::test]
async fn announced_devices_are_persisted() {
    let directory = tempfile::tempdir().unwrap();
    let port = free_udp_port();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_handfastd"))
        .arg("--data-dir")
        .arg(directory.path())
        .arg("--discovery-port")
        .arg(port.to_string())
        .arg("--tcp-port")
        .arg("1716")
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let peer = Identity::new(
        DeviceId::generate(),
        DeviceName::new("Discovery test phone"),
        DeviceType::Phone,
        Capabilities::new([types::PING], [types::PING]).unwrap(),
        Some(1716),
    );
    let sender = Discovery::bind(peer.clone(), 0, 1716).await.unwrap();
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

    // The daemon opens its state file before binding the discovery socket, so
    // wait for the file instead of a fixed startup delay.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !directory.path().join("state.db").exists() {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited before opening its state file"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "handfastd did not open its state file"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The discovery socket binds just after the state file, so announce
    // repeatedly. A lost datagram is harmless.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited before receiving an announcement"
        );
        let _ = sender.announce(target).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    daemon.kill().unwrap();
    daemon.wait().unwrap();

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

fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap().port()
}
