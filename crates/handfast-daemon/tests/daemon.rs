//! Daemon composition tests.
//!
//! The startup test observes the daemon's own discovery announcement and then
//! connects to the advertised TCP port. The link tests use the independent
//! testkit peer; no production link code runs in the test process.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use handfast_net::{Discovery, Fingerprint};
use handfast_proto::{
    Capabilities, DeviceId, DeviceName, DeviceType, Identity, MAX_TCP_PORT, MIN_TCP_PORT,
};
use handfast_store::Store;
use handfast_testkit::peer::PeerConfig;
use handfast_testkit::tls_peer::{dial, TlsPeerConfig, TlsVersion};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const TEST_PEER_ID: &str = "fedcba9876543210fedcba9876543210";

// The daemon tests share the fixed KDE Connect TCP range. Run them one at a
// time so port scans cannot race each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

#[tokio::test]
async fn startup_binds_the_next_port_and_announces_the_actual_port() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let occupied_port = free_tcp_port_below_max();
    let identity = print_identity(directory.path(), occupied_port);
    let device_id = identity["body"]["deviceId"].as_str().unwrap().to_string();

    let occupied = std::net::TcpListener::bind(("0.0.0.0", occupied_port))
        .expect("occupy the configured start port");
    let discovery_port = free_udp_port();
    let receiver = Discovery::bind(receiver_identity(), discovery_port, occupied_port)
        .await
        .expect("bind the announcement receiver");

    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(directory.path(), occupied_port, discovery_port, None, &log);
    wait_for_state_file(directory.path(), &mut daemon).await;

    let (_, announcement) = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("the daemon announces before the timeout")
        .expect("the announcement is valid");
    assert_eq!(announcement.device_id.as_str(), device_id.as_str());
    assert_ne!(
        announcement.tcp_port, occupied_port,
        "the daemon scanned past the occupied port"
    );
    assert!(
        (MIN_TCP_PORT..=MAX_TCP_PORT).contains(&announcement.tcp_port),
        "the announced port is in the KDE Connect range"
    );
    TcpStream::connect(("127.0.0.1", announcement.tcp_port))
        .await
        .expect("the announced TCP port accepts before discovery announces");

    stop(daemon);
    drop(occupied);
}

#[tokio::test]
async fn discovery_sighting_dials_the_testkit_peer() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
    let discovery_port = free_udp_port();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(directory.path(), tcp_port, discovery_port, None, &log);
    wait_for_state_file(directory.path(), &mut daemon).await;

    let peer_port = free_tcp_port();
    let listener = TcpListener::bind(("127.0.0.1", peer_port))
        .await
        .expect("bind the testkit listener in the KDE Connect range");
    let config = testkit_peer();
    let peer_task =
        tokio::spawn(async move { handfast_testkit::tls_peer::accept(&listener, &config).await });

    let sender = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, discovery_port));
    let datagram = format!(
        r#"{{"id":0,"type":"kdeconnect.identity","body":{{"deviceId":"{TEST_PEER_ID}","deviceName":"testkit","protocolVersion":8,"tcpPort":{peer_port}}}}}"#
    );

    let peer = tokio::time::timeout(Duration::from_secs(8), async move {
        loop {
            sender
                .send_to(datagram.as_bytes(), target)
                .await
                .expect("send the discovery announcement");
            tokio::time::sleep(Duration::from_millis(50)).await;
            if peer_task.is_finished() {
                break;
            }
        }
        peer_task
            .await
            .expect("the testkit task joins")
            .expect("the daemon dials the testkit peer")
    })
    .await
    .expect("the daemon dials before the timeout");

    assert_eq!(
        peer.peer_identity()
            .body
            .get("deviceId")
            .and_then(Value::as_str),
        Some(daemon_id.as_str()),
        "the testkit peer authenticated the daemon"
    );
    wait_for_log(&log, "unpaired link authenticated", &mut daemon).await;
    stop(daemon);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    let record = store
        .device(TEST_PEER_ID)
        .unwrap()
        .expect("the discovered peer is persisted");
    assert_eq!(record.name, "Handfast test peer");
    assert_eq!(record.device_type, "desktop");
    assert!(!record.paired);
    assert_eq!(record.certificate_fingerprint, None);
}

#[tokio::test]
async fn paired_peer_reaches_a_session_and_dispatches_a_ping() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
    let discovery_port = free_udp_port();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(
        directory.path(),
        tcp_port,
        discovery_port,
        Some("y\n"),
        &log,
    );
    wait_for_state_file(directory.path(), &mut daemon).await;
    wait_for_log(&log, "TCP listener bound", &mut daemon).await;

    let config = testkit_peer();
    let pin = Fingerprint::of_certificate(config.cert_der());
    let mut peer = dial(
        SocketAddr::from((Ipv4Addr::LOCALHOST, tcp_port)),
        &daemon_id,
        &config,
    )
    .await
    .expect("the testkit peer completes the link");
    wait_for_log(&log, "unpaired link authenticated", &mut daemon).await;

    peer.send(&config.identity.ping_packet("before pairing"))
        .await
        .expect("send a ping before pairing");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !read_log(&log).contains("plugin events"),
        "an unpaired ping must not reach a session"
    );

    peer.send(&config.identity.pair_packet(true, Some(unix_now())))
        .await
        .expect("send a pairing request");
    let response = peer
        .recv()
        .await
        .expect("the pairing response arrives")
        .expect("the daemon answers");
    assert_eq!(response.ptype, "kdeconnect.pair");
    assert_eq!(response.body.get("pair"), Some(&json!(true)));
    wait_for_log(&log, "pairing accepted", &mut daemon).await;

    peer.send(&config.identity.ping_packet("after pairing"))
        .await
        .expect("send a ping after pairing");
    wait_for_log(&log, "plugin events", &mut daemon).await;
    let log_text = read_log(&log);
    assert!(
        log_text.contains("Ping"),
        "the ping reached the session and the ping plugin: {log_text}"
    );

    peer.close().await.expect("close the testkit link");
    stop(daemon);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    let record = store
        .device(TEST_PEER_ID)
        .unwrap()
        .expect("the authenticated peer is persisted");
    assert!(record.paired, "acceptance writes the legacy paired flag");
    assert_eq!(
        record.certificate_fingerprint,
        Some(*pin.as_bytes()),
        "acceptance persists the certificate pin"
    );
}

#[tokio::test]
async fn rejected_pairing_writes_no_pin_and_starts_no_session() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
    let discovery_port = free_udp_port();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(
        directory.path(),
        tcp_port,
        discovery_port,
        Some("n\n"),
        &log,
    );
    wait_for_state_file(directory.path(), &mut daemon).await;
    wait_for_log(&log, "TCP listener bound", &mut daemon).await;

    let config = testkit_peer();
    let mut peer = dial(
        SocketAddr::from((Ipv4Addr::LOCALHOST, tcp_port)),
        &daemon_id,
        &config,
    )
    .await
    .expect("the testkit peer completes the link");
    wait_for_log(&log, "unpaired link authenticated", &mut daemon).await;

    peer.send(&config.identity.pair_packet(true, Some(unix_now())))
        .await
        .expect("send a pairing request");
    let response = peer
        .recv()
        .await
        .expect("the rejection arrives")
        .expect("the daemon answers");
    assert_eq!(response.ptype, "kdeconnect.pair");
    assert_eq!(response.body.get("pair"), Some(&json!(false)));
    wait_for_log(&log, "pairing rejected", &mut daemon).await;

    peer.send(&config.identity.ping_packet("after rejection"))
        .await
        .expect("send a ping after rejection");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !read_log(&log).contains("plugin events"),
        "a rejected peer must not reach a session"
    );

    peer.close().await.expect("close the testkit link");
    stop(daemon);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    let record = store
        .device(TEST_PEER_ID)
        .unwrap()
        .expect("the authenticated peer is persisted");
    assert!(!record.paired);
    assert_eq!(record.certificate_fingerprint, None);
}

#[tokio::test]
async fn exhausted_range_exits_before_discovery() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let occupied = std::net::TcpListener::bind(("0.0.0.0", MAX_TCP_PORT))
        .expect("occupy the last port in the range");
    let discovery_port = free_udp_port();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(directory.path(), MAX_TCP_PORT, discovery_port, None, &log);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            assert!(!status.success(), "the daemon exits nonzero");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit after the range was exhausted"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !read_log(&log).contains("discovery listening"),
        "the daemon must not announce after a failed bind"
    );
    drop(occupied);
}

#[tokio::test]
async fn corrupt_identity_is_a_fatal_startup_error() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let other_id = DeviceId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    {
        let store = Store::open(directory.path().join("state.db")).unwrap();
        store
            .set_setting("device_id", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .unwrap();
        let tls = handfast_net::TlsIdentity::generate(other_id).unwrap();
        store
            .set_identity(&handfast_store::IdentityRecord {
                device_id: tls.device_id().as_str().to_string(),
                cert_der: tls.cert_der().to_vec(),
                key_der: tls.key_der().to_vec(),
            })
            .unwrap();
    }

    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(
        directory.path(),
        free_tcp_port(),
        free_udp_port(),
        None,
        &log,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            assert!(!status.success(), "a corrupt identity is fatal");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not reject the corrupt identity"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        read_log(&log).contains("does not match"),
        "the error names the device ID mismatch: {}",
        read_log(&log)
    );
}

#[tokio::test]
async fn restart_reuses_the_persisted_certificate() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let discovery_port = free_udp_port();
    let log = directory.path().join("daemon.log");

    let mut first = spawn_daemon(directory.path(), tcp_port, discovery_port, None, &log);
    wait_for_state_file(directory.path(), &mut first).await;
    wait_for_log(&log, "TCP listener bound", &mut first).await;
    stop(first);
    let first_record = {
        let store = Store::open(directory.path().join("state.db")).unwrap();
        store
            .identity()
            .unwrap()
            .expect("the first start persists identity")
    };

    let mut second = spawn_daemon(directory.path(), tcp_port, discovery_port, None, &log);
    wait_for_state_file(directory.path(), &mut second).await;
    wait_for_log(&log, "TCP listener bound", &mut second).await;
    stop(second);
    let second_record = {
        let store = Store::open(directory.path().join("state.db")).unwrap();
        store
            .identity()
            .unwrap()
            .expect("the second start loads identity")
    };

    assert_eq!(first_record.device_id, second_record.device_id);
    assert_eq!(first_record.cert_der, second_record.cert_der);
    assert_eq!(first_record.key_der, second_record.key_der);
}

fn testkit_peer() -> TlsPeerConfig {
    let mut config = PeerConfig::client();
    config.device_id = TEST_PEER_ID.to_string();
    TlsPeerConfig::ecdsa(config, TlsVersion::Both).expect("build the testkit peer")
}

fn receiver_identity() -> Identity {
    Identity::new(
        DeviceId::generate(),
        DeviceName::new("announcement receiver"),
        DeviceType::Desktop,
        Capabilities::default(),
        Some(MIN_TCP_PORT),
    )
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

fn spawn_daemon(
    directory: &Path,
    tcp_port: u16,
    discovery_port: u16,
    stdin: Option<&str>,
    log: &Path,
) -> Child {
    let stderr = std::fs::File::create(log).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_handfastd"));
    command
        .args([
            "--data-dir",
            directory.to_str().unwrap(),
            "--tcp-port",
            &tcp_port.to_string(),
            "--discovery-port",
            &discovery_port.to_string(),
        ])
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    match stdin {
        Some(_) => {
            command.stdin(Stdio::piped());
        }
        None => {
            command.stdin(Stdio::null());
        }
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("the child stdin is piped")
            .write_all(input.as_bytes())
            .unwrap();
    }
    child
}

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

async fn wait_for_log(log: &Path, needle: &str, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if read_log(log).contains(needle) {
            return;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited while waiting for {needle:?}"
        );
        assert!(
            Instant::now() < deadline,
            "the log never contained {needle:?}: {}",
            read_log(log)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn read_log(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
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

fn free_tcp_port_below_max() -> u16 {
    for port in MIN_TCP_PORT..MAX_TCP_PORT {
        if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
            drop(listener);
            return port;
        }
    }
    panic!("no free TCP port below {MAX_TCP_PORT}");
}

fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap().port()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
