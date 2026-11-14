//! Daemon composition tests.
//!
//! The startup test observes the daemon's own discovery announcement and then
//! connects to the advertised TCP port. The link tests use the independent
//! testkit peer; no production link code runs in the test process.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_net::{
    Discovery, Fingerprint, LinkEvent, LinkLimits, LinkListener, LinkManager, LocalEndpoint,
    PeerCertificate, TlsIdentity, TrustTable,
};
use handfast_proto::{
    types, Capabilities, DeviceId, DeviceName, DeviceType, Identity, MAX_TCP_PORT, MIN_TCP_PORT,
    PAYLOAD_PORT_MAX, PAYLOAD_PORT_MIN,
};
use handfast_store::Store;
use handfast_testkit::peer::PeerConfig;
use handfast_testkit::tls_peer::{dial, TlsPeer, TlsPeerConfig, TlsVersion};
use handfast_testkit::WirePacket;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::{TlsAcceptor, TlsConnector};

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
async fn unpair_clears_pins_and_stops_the_session() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
    let discovery_port = free_udp_port();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon_filtered(
        directory.path(),
        tcp_port,
        discovery_port,
        Some("y\n"),
        &log,
        "debug",
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
        .expect("the pairing response arrives")
        .expect("the daemon answers");
    assert_eq!(response.body.get("pair"), Some(&json!(true)));
    wait_for_log(&log, "pairing accepted", &mut daemon).await;

    peer.send(&config.identity.ping_packet("before unpair"))
        .await
        .expect("send a ping before unpair");
    wait_for_log(&log, "plugin events", &mut daemon).await;

    peer.send(&config.identity.pair_packet(false, None))
        .await
        .expect("send an unpair request");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, peer.recv()).await {
            // Plugin packets from the running session may arrive before the
            // close. Only the close is part of this contract.
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) => break,
            Err(_) => panic!("the manager did not close the link on unpair"),
        }
    }
    wait_for_log(&log, "device session closing", &mut daemon).await;
    assert!(
        read_log(&log)
            .lines()
            .any(|line| line.contains("device session closing") && line.contains("unpaired")),
        "the session stopped for the unpair reason: {}",
        read_log(&log)
    );

    // The manager rate-limits inbound connections per IP for 500 ms.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // The manager pin is gone when a fresh link from the same peer is unpaired.
    let again = dial(
        SocketAddr::from((Ipv4Addr::LOCALHOST, tcp_port)),
        &daemon_id,
        &config,
    )
    .await
    .expect("the testkit peer reconnects");
    wait_for_log_count(&log, "unpaired link authenticated", 2, &mut daemon).await;

    again.close().await.expect("close the testkit link");
    stop(daemon);

    let store = Store::open(directory.path().join("state.db")).unwrap();
    let record = store
        .device(TEST_PEER_ID)
        .unwrap()
        .expect("the authenticated peer is persisted");
    assert!(!record.paired, "unpair clears the legacy paired flag");
    assert_eq!(
        record.certificate_fingerprint, None,
        "unpair clears the durable pin"
    );
    assert!(store.trust_table().unwrap().is_empty());
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
async fn v020_state_migrates_and_restart_reuses_the_certificate() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state.db");
    let device_id = "cccccccccccccccccccccccccccccccc";
    write_v020_state(&state_path, device_id, TEST_PEER_ID);

    let tcp_port = free_tcp_port();
    let discovery_port = free_udp_port();
    let log = directory.path().join("daemon.log");
    let mut first = spawn_daemon(directory.path(), tcp_port, discovery_port, None, &log);
    wait_for_log(&log, "TCP listener bound", &mut first).await;
    stop(first);

    let (first_identity, first_device) = {
        let store = Store::open(&state_path).unwrap();
        let identity = store
            .identity()
            .unwrap()
            .expect("the first start creates identity_v1");
        let device = store
            .device(TEST_PEER_ID)
            .unwrap()
            .expect("the v0.2.0 device record survives");
        (identity, device)
    };
    assert_eq!(first_identity.device_id.as_str(), device_id);
    let certificate =
        PeerCertificate::parse(&first_identity.cert_der).expect("the migrated certificate parses");
    assert_eq!(certificate.device_id().as_str(), device_id);
    assert_eq!(first_device.name, "Old phone");
    assert_eq!(first_device.device_type, "phone");
    assert!(!first_device.paired);
    assert_eq!(first_device.certificate_fingerprint, None);

    let mut second = spawn_daemon(directory.path(), tcp_port, discovery_port, None, &log);
    wait_for_log(&log, "TCP listener bound", &mut second).await;
    stop(second);

    let second_identity = {
        let store = Store::open(&state_path).unwrap();
        store
            .identity()
            .unwrap()
            .expect("the second start loads identity_v1")
    };
    assert_eq!(first_identity.cert_der, second_identity.cert_der);
    assert_eq!(first_identity.key_der, second_identity.key_der);
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

/// Write the v0.2.0 schema: settings, devices and plugin state, with no
/// `identity_v1` table and no `certificate_fingerprint` field.
fn write_v020_state(path: &Path, device_id: &str, peer_id: &str) {
    let database = redb::Database::create(path).expect("create the v0.2.0 state file");
    let write = database.begin_write().expect("begin the v0.2.0 write");
    {
        let devices: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("devices_v1");
        let mut devices = write.open_table(devices).expect("open devices_v1");
        let record = json!({
            "id": peer_id,
            "name": "Old phone",
            "device_type": "phone",
            "paired": false,
            "last_seen": 1_700_000_000_i64,
        });
        let value = serde_json::to_vec(&record).expect("encode the v0.2.0 device");
        devices
            .insert(peer_id, value.as_slice())
            .expect("write the v0.2.0 device");

        let settings: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("settings_v1");
        let mut settings = write.open_table(settings).expect("open settings_v1");
        settings
            .insert("device_id", device_id)
            .expect("write the device ID");

        let plugins: redb::TableDefinition<&str, u8> =
            redb::TableDefinition::new("plugin_state_v1");
        let mut plugins = write.open_table(plugins).expect("open plugin_state_v1");
        plugins
            .insert("peer\u{1f}ping", 1)
            .expect("write the plugin state");
    }
    write.commit().expect("commit the v0.2.0 state");
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
    spawn_daemon_filtered(directory, tcp_port, discovery_port, stdin, log, "info")
}

fn spawn_daemon_filtered(
    directory: &Path,
    tcp_port: u16,
    discovery_port: u16,
    stdin: Option<&str>,
    log: &Path,
    filter: &str,
) -> Child {
    let stderr = std::fs::File::create(log).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_handfastd"));
    command
        .args([
            "--data-dir",
            directory.to_str().unwrap(),
            "--incoming-dir",
            incoming_dir(directory).to_str().unwrap(),
            "--tcp-port",
            &tcp_port.to_string(),
            "--discovery-port",
            &discovery_port.to_string(),
        ])
        // Keep the control socket under the temp data directory instead of the
        // real runtime directory, so parallel tests cannot share it.
        .env_remove("XDG_RUNTIME_DIR")
        .env("RUST_LOG", filter)
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

async fn wait_for_log_count(log: &Path, needle: &str, count: usize, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if read_log(log).matches(needle).count() >= count {
            return;
        }
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited while waiting for {count} x {needle:?}"
        );
        assert!(
            Instant::now() < deadline,
            "the log never contained {count} x {needle:?}: {}",
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

fn incoming_dir(directory: &Path) -> PathBuf {
    directory.join("incoming")
}

fn control_socket(directory: &Path) -> PathBuf {
    directory.join("handfastd.sock")
}

fn temp_dir(incoming: &Path) -> PathBuf {
    incoming.join(".handfast-tmp")
}

fn share_file_wire(filename: &str, size: u64, port: u16) -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::SHARE_REQUEST.to_string(),
        body: json!({ "filename": filename })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: Some(i64::try_from(size).expect("a test size fits i64")),
        payload_transfer_info: Some(json!({ "port": port })),
    }
}

fn empty_file_wire(filename: &str) -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::SHARE_REQUEST.to_string(),
        body: json!({ "filename": filename })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: None,
        payload_transfer_info: None,
    }
}

fn share_text_wire(text: &str) -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::SHARE_REQUEST.to_string(),
        body: json!({ "text": text })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: None,
        payload_transfer_info: None,
    }
}

fn share_url_wire(url: &str) -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::SHARE_REQUEST.to_string(),
        body: json!({ "url": url })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: None,
        payload_transfer_info: None,
    }
}

fn free_tcp_port_below_payload() -> u16 {
    for port in MIN_TCP_PORT..PAYLOAD_PORT_MIN {
        if std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
    panic!(
        "no free TCP port in {MIN_TCP_PORT}-{}",
        PAYLOAD_PORT_MIN - 1
    );
}

fn free_payload_port() -> u16 {
    for port in PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX {
        if std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return port;
        }
    }
    panic!("no free payload port in {PAYLOAD_PORT_MIN}-{PAYLOAD_PORT_MAX}");
}

fn occupy_payload_ports() -> Vec<std::net::TcpListener> {
    (PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX)
        .map(|port| std::net::TcpListener::bind(("0.0.0.0", port)).expect("occupy a payload port"))
        .collect()
}

/// Bind the test's payload listener. It presents the control peer's certificate,
/// so the daemon's pin accepts the TLS client handshake.
fn payload_server(config: &TlsPeerConfig, port: u16) -> (TcpListener, TlsAcceptor) {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .expect("bind the test payload listener");
    listener
        .set_nonblocking(true)
        .expect("the test payload listener is nonblocking");
    let listener = TcpListener::from_std(listener).expect("adopt the test payload listener");
    let acceptor = TlsAcceptor::from(Arc::new(
        config
            .server_config()
            .expect("build the payload server config"),
    ));
    (listener, acceptor)
}

async fn serve_payload(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    bytes: Vec<u8>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
) -> std::io::Result<u64> {
    let (tcp, _) = listener.accept().await?;
    let mut stream = acceptor.accept(tcp).await.map_err(std::io::Error::other)?;
    if let Some(release) = release {
        let _ = release.await;
    }
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;
    Ok(bytes.len() as u64)
}

async fn receive_payload(config: &TlsPeerConfig, port: u16, max: u64) -> std::io::Result<Vec<u8>> {
    let connector = TlsConnector::from(Arc::new(
        config
            .client_config()
            .expect("build the payload client config"),
    ));
    let tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
    let name = ServerName::try_from("localhost").expect("a server name");
    let mut stream = connector
        .connect(name, tcp)
        .await
        .map_err(std::io::Error::other)?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() as u64 > max {
            break;
        }
    }
    Ok(buffer)
}

async fn pair(peer: &mut TlsPeer, config: &TlsPeerConfig) {
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
}

async fn spawn_and_pair(
    directory: &Path,
    log: &Path,
    discovery_port: u16,
) -> (Child, TlsPeer, TlsPeerConfig, String) {
    let tcp_port = free_tcp_port_below_payload();
    let identity = print_identity(directory, tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
    let mut daemon = spawn_daemon(directory, tcp_port, discovery_port, Some("y\n"), log);
    wait_for_state_file(directory, &mut daemon).await;
    wait_for_log(log, "TCP listener bound", &mut daemon).await;
    let config = testkit_peer();
    let mut peer = dial(
        SocketAddr::from((Ipv4Addr::LOCALHOST, tcp_port)),
        &daemon_id,
        &config,
    )
    .await
    .expect("the testkit peer completes the link");
    wait_for_log(log, "unpaired link authenticated", &mut daemon).await;
    pair(&mut peer, &config).await;
    wait_for_log(log, "pairing accepted", &mut daemon).await;
    (daemon, peer, config, daemon_id)
}

async fn wait_for_path(path: &Path, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "handfastd exited before {} appeared",
            path.display()
        );
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_absence(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} was never removed",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait for the first `.part` file and return its path.
async fn wait_for_temp(incoming: &Path) -> PathBuf {
    let directory = temp_dir(incoming);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                return entry.path();
            }
        }
        assert!(
            Instant::now() < deadline,
            "no temp file appeared in {}",
            directory.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn temp_entries(incoming: &Path) -> usize {
    std::fs::read_dir(temp_dir(incoming))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

async fn run_hfctl(data_dir: PathBuf, device: String, path: PathBuf) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_hfctl"))
                .env_remove("XDG_RUNTIME_DIR")
                .args(["share", "--device", &device, "--data-dir"])
                .arg(&data_dir)
                .arg(&path)
                .output()
                .expect("run hfctl")
        }),
    )
    .await
    .expect("hfctl finishes")
    .expect("the hfctl task joins")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[cfg(unix)]
fn interrupt(daemon: &Child) {
    let status = Command::new("kill")
        .args(["-INT", &daemon.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(status.success(), "kill -INT failed");
}

#[cfg(unix)]
fn wait_for_exit(mut daemon: Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            assert!(status.success(), "handfastd exits cleanly on SIGINT");
            return;
        }
        assert!(Instant::now() < deadline, "handfastd did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn peer_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("a valid test device ID")
}

fn local_endpoint() -> (Arc<LocalEndpoint>, Arc<TlsIdentity>) {
    let device_id = peer_id("00112233445566778899aabbccddeeff");
    let tls =
        Arc::new(TlsIdentity::generate(device_id.clone()).expect("generate a local identity"));
    let identity = Identity::new(
        device_id,
        DeviceName::new("Handfast share daemon"),
        DeviceType::Desktop,
        Capabilities::default(),
        Some(MIN_TCP_PORT),
    );
    let local = Arc::new(LocalEndpoint::new(Arc::clone(&tls), identity).expect("the IDs match"));
    (local, tls)
}

#[tokio::test]
async fn paired_peer_sends_a_file_and_it_is_committed_atomically() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;

    let port = free_payload_port();
    let (listener, acceptor) = payload_server(&config, port);
    let bytes: Vec<u8> = (0..256 * 1024).map(|index| (index % 251) as u8).collect();
    let expected = bytes.clone();
    let (release, release_rx) = oneshot::channel();
    let send = tokio::spawn(serve_payload(listener, acceptor, bytes, Some(release_rx)));

    peer.send(&share_file_wire("photo.jpg", expected.len() as u64, port))
        .await
        .expect("send the share request");

    let temp = wait_for_temp(&incoming_dir(directory.path())).await;
    let saved = incoming_dir(directory.path()).join("photo.jpg");
    assert!(
        !saved.exists(),
        "the final name is not visible before the commit"
    );
    release.send(()).expect("release the payload");
    wait_for_file(&saved).await;
    assert_eq!(
        std::fs::read(&saved).expect("read the saved file"),
        expected,
        "the committed bytes match the payload"
    );
    assert!(!temp.exists(), "the temp file is removed by the commit");
    assert_eq!(temp_entries(&incoming_dir(directory.path())), 0);
    assert_eq!(
        send.await
            .expect("the sender joins")
            .expect("the payload is sent"),
        expected.len() as u64
    );
    wait_for_log(&log, "ShareCompleted", &mut daemon).await;

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn paired_peer_sends_text_and_url_and_logs_redacted_events() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, _config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;
    let body = "SECRET-SHARE-TEXT-BODY";
    let url = "https://example.com/SECRET-SHARE-URL-BODY";

    peer.send(&share_text_wire(body))
        .await
        .expect("send a text share");
    peer.send(&share_url_wire(url))
        .await
        .expect("send a url share");
    wait_for_log(&log, "event=\"ShareText\"", &mut daemon).await;
    wait_for_log(&log, "event=\"ShareUrl\"", &mut daemon).await;

    let log_text = read_log(&log);
    assert!(
        log_text.contains(&format!("bytes={}", body.len())),
        "{log_text}"
    );
    assert!(
        !log_text.contains("SECRET-SHARE-TEXT-BODY"),
        "the log must not contain the text body: {log_text}"
    );
    assert!(
        !log_text.contains("SECRET-SHARE-URL-BODY"),
        "the log must not contain the url body: {log_text}"
    );

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn daemon_log_redacts_text_and_url_bodies() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, _config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;

    peer.send(&share_text_wire("hunter2-correct-horse"))
        .await
        .expect("send a text share");
    peer.send(&share_url_wire("https://example.com/private?token=hunter3"))
        .await
        .expect("send a url share");
    wait_for_log(&log, "event=\"ShareText\"", &mut daemon).await;
    wait_for_log(&log, "event=\"ShareUrl\"", &mut daemon).await;

    let text = read_log(&log);
    assert!(!text.contains("hunter2-correct-horse"), "{text}");
    assert!(!text.contains("hunter3"), "{text}");
    assert!(text.contains("event=\"ShareText\""), "{text}");
    assert!(text.contains("event=\"ShareUrl\""), "{text}");

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn unpaired_peer_share_packet_never_reaches_the_plugin() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(directory.path(), tcp_port, free_udp_port(), None, &log);
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

    let port = free_payload_port();
    peer.send(&share_text_wire("unpaired secret"))
        .await
        .expect("send an unpaired text share");
    peer.send(&share_file_wire("unpaired.bin", 1, port))
        .await
        .expect("send an unpaired file share");
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        !read_log(&log).contains("plugin events"),
        "an unpaired share packet must not reach a session: {}",
        read_log(&log)
    );
    assert!(
        !incoming_dir(directory.path()).join("unpaired.bin").exists(),
        "an unpaired peer cannot write a file"
    );

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn truncated_payload_leaves_no_file_and_emits_share_failed() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;

    let port = free_payload_port();
    let (listener, acceptor) = payload_server(&config, port);
    let send = tokio::spawn(serve_payload(listener, acceptor, b"1234".to_vec(), None));
    peer.send(&share_file_wire("short.bin", 8, port))
        .await
        .expect("send a share request larger than the payload");

    wait_for_log(&log, "ShareFailed", &mut daemon).await;
    let text = read_log(&log);
    assert!(text.contains("size mismatch"), "{text}");
    assert!(!incoming_dir(directory.path()).join("short.bin").exists());
    assert_eq!(temp_entries(&incoming_dir(directory.path())), 0);
    let _ = send.await;

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn disconnect_mid_transfer_removes_the_temp() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;

    let port = free_payload_port();
    let (listener, acceptor) = payload_server(&config, port);
    let (release, release_rx) = oneshot::channel();
    let send = tokio::spawn(serve_payload(
        listener,
        acceptor,
        b"payload".to_vec(),
        Some(release_rx),
    ));
    peer.send(&share_file_wire("partial.bin", 7, port))
        .await
        .expect("send a share request");

    let temp = wait_for_temp(&incoming_dir(directory.path())).await;
    peer.close()
        .await
        .expect("close the testkit link mid transfer");
    wait_for_log(&log, "device link closed", &mut daemon).await;
    wait_for_absence(&temp).await;

    assert!(!incoming_dir(directory.path()).join("partial.bin").exists());
    assert_eq!(temp_entries(&incoming_dir(directory.path())), 0);
    drop(release);
    send.abort();
    stop(daemon);
}

#[tokio::test]
async fn two_queued_files_arrive_in_order() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;

    let first_port = free_payload_port();
    let second_port = free_payload_port();
    assert_ne!(first_port, second_port);
    let (first_listener, first_acceptor) = payload_server(&config, first_port);
    let (second_listener, second_acceptor) = payload_server(&config, second_port);
    let first = tokio::spawn(serve_payload(
        first_listener,
        first_acceptor,
        b"first".to_vec(),
        None,
    ));
    let second = tokio::spawn(serve_payload(
        second_listener,
        second_acceptor,
        b"second".to_vec(),
        None,
    ));

    peer.send(&share_file_wire("first.bin", 5, first_port))
        .await
        .expect("send the first share request");
    peer.send(&share_file_wire("second.bin", 6, second_port))
        .await
        .expect("send the second share request");

    let root = incoming_dir(directory.path());
    wait_for_file(&root.join("first.bin")).await;
    wait_for_file(&root.join("second.bin")).await;
    assert_eq!(std::fs::read(root.join("first.bin")).unwrap(), b"first");
    assert_eq!(std::fs::read(root.join("second.bin")).unwrap(), b"second");
    first.await.expect("the first sender joins").expect("sent");
    second
        .await
        .expect("the second sender joins")
        .expect("sent");

    let text = read_log(&log);
    let first_done = text
        .find("first.bin")
        .expect("the first completion is logged");
    let second_done = text
        .find("second.bin")
        .expect("the second completion is logged");
    assert!(
        first_done < second_done,
        "the files complete in queue order: {text}"
    );
    assert_eq!(temp_entries(&root), 0);

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn large_first_file_with_a_queued_second_completes_both() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;

    let first_port = free_payload_port();
    let second_port = free_payload_port();
    assert_ne!(first_port, second_port);
    let (first_listener, first_acceptor) = payload_server(&config, first_port);
    let (second_listener, second_acceptor) = payload_server(&config, second_port);
    let first_bytes: Vec<u8> = (0..4 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();
    let first_expected = first_bytes.clone();
    let first = tokio::spawn(serve_payload(
        first_listener,
        first_acceptor,
        first_bytes,
        None,
    ));
    let second = tokio::spawn(serve_payload(
        second_listener,
        second_acceptor,
        b"z".to_vec(),
        None,
    ));

    peer.send(&share_file_wire(
        "large.bin",
        first_expected.len() as u64,
        first_port,
    ))
    .await
    .expect("send the large share request");
    peer.send(&share_file_wire("queued.bin", 1, second_port))
        .await
        .expect("send the queued share request");

    let root = incoming_dir(directory.path());
    wait_for_file(&root.join("large.bin")).await;
    wait_for_file(&root.join("queued.bin")).await;
    assert_eq!(
        std::fs::read(root.join("large.bin")).unwrap(),
        first_expected
    );
    assert_eq!(std::fs::read(root.join("queued.bin")).unwrap(), b"z");
    first.await.expect("the first sender joins").expect("sent");
    second
        .await
        .expect("the second sender joins")
        .expect("sent");
    assert_eq!(temp_entries(&root), 0);

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn duplicate_filename_gets_a_suffix() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;
    let root = incoming_dir(directory.path());

    for (index, bytes) in [b"one".as_slice(), b"two".as_slice()].iter().enumerate() {
        let port = free_payload_port();
        let (listener, acceptor) = payload_server(&config, port);
        let send = tokio::spawn(serve_payload(listener, acceptor, bytes.to_vec(), None));
        peer.send(&share_file_wire("report.txt", bytes.len() as u64, port))
            .await
            .expect("send a duplicate name");
        if index == 0 {
            wait_for_file(&root.join("report.txt")).await;
        } else {
            wait_for_file(&root.join("report (1).txt")).await;
        }
        send.await.expect("the sender joins").expect("sent");
    }

    assert_eq!(std::fs::read(root.join("report.txt")).unwrap(), b"one");
    assert_eq!(std::fs::read(root.join("report (1).txt")).unwrap(), b"two");
    assert_eq!(temp_entries(&root), 0);

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn full_share_channel_closes_the_link() {
    let _serial = serial().await;
    let (local, tls) = local_endpoint();
    let listener = LinkListener::bind_in_range(free_tcp_port(), tls)
        .await
        .expect("bind a listener in the KDE Connect range");
    let address = SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        listener.local_addr().expect("the bound address").port(),
    ));
    let config = testkit_peer();
    let mut trust = TrustTable::default();
    trust.set(
        peer_id(TEST_PEER_ID),
        Fingerprint::of_certificate(config.cert_der()),
    );
    let (events, mut events_rx) = mpsc::channel(8);
    // The daemon owns the only consumer. A stalled daemon loop is exactly this:
    // a full 32-slot channel with nobody draining it.
    let (share_events, _share_rx) = mpsc::channel(handfast_net::SHARE_EVENT_QUEUE);
    let (links, task) = LinkManager::start(
        listener,
        local,
        trust,
        LinkLimits::default(),
        events,
        share_events,
    );
    let mut peer = dial(address, "00112233445566778899aabbccddeeff", &config)
        .await
        .expect("the paired link is admitted");
    let connected = tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
        .await
        .expect("the manager emits Connected")
        .expect("the manager is running");
    assert!(
        matches!(connected, LinkEvent::Connected { paired: true, .. }),
        "the test peer is paired"
    );

    for _ in 0..=handfast_net::SHARE_EVENT_QUEUE {
        peer.send(&share_text_wire("fill the share channel"))
            .await
            .expect("send a share packet");
    }

    let reason = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events_rx.recv().await.expect("the manager is running") {
                LinkEvent::Disconnected { reason, .. } => break reason,
                _ => {}
            }
        }
    })
    .await
    .expect("the manager closes the link");
    assert_eq!(reason, "share event queue full");
    let closed = tokio::time::timeout(Duration::from_secs(2), peer.recv())
        .await
        .expect("the peer sees the close");
    assert!(matches!(closed, Ok(None) | Err(_)), "the link closes");

    links.shutdown();
    let _ = task.await;
}

#[cfg(unix)]
#[tokio::test]
async fn outbound_hfctl_share_reaches_a_testkit_receiver() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;
    wait_for_path(&control_socket(directory.path()), &mut daemon).await;

    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("report.pdf");
    let bytes: Vec<u8> = (0..256 * 1024).map(|index| (index % 251) as u8).collect();
    std::fs::write(&source, &bytes).expect("write the source file");
    let expected = bytes.clone();
    let len = expected.len() as u64;

    let share = tokio::spawn(run_hfctl(
        directory.path().to_path_buf(),
        TEST_PEER_ID.to_string(),
        source,
    ));
    let packet = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let packet = peer
                .recv()
                .await
                .expect("the link stays open")
                .expect("a packet arrives");
            if packet.ptype == types::SHARE_REQUEST {
                break packet;
            }
        }
    })
    .await
    .expect("the daemon sends the share packet");

    let port = packet
        .payload_transfer_info
        .as_ref()
        .and_then(|value| value.get("port"))
        .and_then(Value::as_u64)
        .expect("a payload port") as u16;
    assert!(
        (PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX).contains(&port),
        "the payload port is in range: {port}"
    );
    let received = receive_payload(&config, port, len)
        .await
        .expect("the receiver reads the payload");
    assert_eq!(received, expected, "the receiver gets the source bytes");

    let output = share.await.expect("hfctl joins");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("sent {len} bytes to {TEST_PEER_ID}")
    );

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[cfg(unix)]
#[tokio::test]
async fn outbound_empty_file_sends_no_payload_listener() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, _config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;
    wait_for_path(&control_socket(directory.path()), &mut daemon).await;

    // Every payload port is taken. An empty share must still succeed because it
    // binds no listener.
    let occupied = occupy_payload_ports();
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("empty.txt");
    std::fs::write(&source, b"").expect("write the empty source file");

    let share = tokio::spawn(run_hfctl(
        directory.path().to_path_buf(),
        TEST_PEER_ID.to_string(),
        source,
    ));
    let packet = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let packet = peer
                .recv()
                .await
                .expect("the link stays open")
                .expect("a packet arrives");
            if packet.ptype == types::SHARE_REQUEST {
                break packet;
            }
        }
    })
    .await
    .expect("the daemon sends the share packet");

    assert_eq!(packet.payload_size, None, "an empty file has no payload");
    assert_eq!(
        packet.payload_transfer_info, None,
        "an empty file advertises no payload port"
    );
    let output = share.await.expect("hfctl joins");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("sent 0 bytes to {TEST_PEER_ID}")
    );
    drop(occupied);

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[cfg(unix)]
#[tokio::test]
async fn outbound_with_no_free_payload_port_sends_no_packet() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (mut daemon, mut peer, _config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;
    wait_for_path(&control_socket(directory.path()), &mut daemon).await;

    let occupied = occupy_payload_ports();
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("report.pdf");
    std::fs::write(&source, b"payload").expect("write the source file");

    let output = run_hfctl(
        directory.path().to_path_buf(),
        TEST_PEER_ID.to_string(),
        source,
    )
    .await;
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("no payload port"),
        "stderr: {}",
        stderr(&output)
    );
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, peer.recv()).await {
            Err(_) => break,
            Ok(Ok(Some(packet))) => assert_ne!(
                packet.ptype,
                types::SHARE_REQUEST,
                "no share packet is sent when no payload port is free"
            ),
            Ok(_) => break,
        }
    }
    drop(occupied);

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_aborts_transfers_and_leaves_no_part_file() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let (daemon, mut peer, config, _) =
        spawn_and_pair(directory.path(), &log, free_udp_port()).await;

    let port = free_payload_port();
    let (listener, acceptor) = payload_server(&config, port);
    let (release, release_rx) = oneshot::channel();
    let send = tokio::spawn(serve_payload(
        listener,
        acceptor,
        b"payload".to_vec(),
        Some(release_rx),
    ));
    peer.send(&share_file_wire("partial.bin", 7, port))
        .await
        .expect("send a share request");
    let temp = wait_for_temp(&incoming_dir(directory.path())).await;

    interrupt(&daemon);
    wait_for_exit(daemon);
    wait_for_absence(&temp).await;

    assert!(!incoming_dir(directory.path()).join("partial.bin").exists());
    assert_eq!(temp_entries(&incoming_dir(directory.path())), 0);
    drop(release);
    send.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_removes_the_control_socket() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(
        directory.path(),
        free_tcp_port(),
        free_udp_port(),
        None,
        &log,
    );
    wait_for_state_file(directory.path(), &mut daemon).await;
    let socket = control_socket(directory.path());
    wait_for_path(&socket, &mut daemon).await;

    interrupt(&daemon);
    wait_for_exit(daemon);
    assert!(
        !socket.exists(),
        "graceful shutdown removes the control socket"
    );
}

#[tokio::test]
async fn startup_sweeps_stale_part_files() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let incoming = incoming_dir(directory.path());
    let temp = temp_dir(&incoming);
    std::fs::create_dir_all(&temp).unwrap();

    let stale = temp.join("stale.part");
    std::fs::write(&stale, b"stale").unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(48 * 3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&stale)
        .expect("open the stale temp file")
        .set_modified(old)
        .expect("age the stale temp file");
    let young = temp.join("young.part");
    std::fs::write(&young, b"young").unwrap();
    let user_file = incoming.join("keep.txt");
    std::fs::write(&user_file, b"user data").unwrap();

    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(
        directory.path(),
        free_tcp_port(),
        free_udp_port(),
        None,
        &log,
    );
    wait_for_log(&log, "incoming directory ready", &mut daemon).await;

    assert!(
        !stale.exists(),
        "the startup sweep removes a stale part file"
    );
    assert!(young.exists(), "a young part file is kept");
    assert!(user_file.exists(), "a user file in the root is never swept");
    assert_eq!(temp_entries(&incoming), 1, "only the young temp remains");
    stop(daemon);
}

#[tokio::test]
async fn invalid_temp_path_is_a_fatal_startup_error() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let incoming = incoming_dir(directory.path());
    std::fs::create_dir_all(&incoming).unwrap();
    std::fs::write(temp_dir(&incoming), b"not a directory").expect("occupy the temp path");

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
            assert!(!status.success(), "an invalid temp path is fatal");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "handfastd did not reject the invalid temp path"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        read_log(&log).contains("not a directory"),
        "the error names the invalid temp path: {}",
        read_log(&log)
    );
    assert!(
        !control_socket(directory.path()).exists(),
        "a failed startup leaves no control socket"
    );
}

#[tokio::test]
async fn discovery_pairing_and_file_share_end_to_end() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let discovery_port = free_udp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
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

    let peer_port = free_tcp_port();
    let listener = TcpListener::bind(("127.0.0.1", peer_port))
        .await
        .expect("bind the testkit listener in the KDE Connect range");
    let config = testkit_peer();
    let sender = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, discovery_port));
    let datagram = format!(
        r#"{{"id":0,"type":"kdeconnect.identity","body":{{"deviceId":"{TEST_PEER_ID}","deviceName":"testkit","protocolVersion":8,"tcpPort":{peer_port}}}}}"#
    );
    // The announcement may arrive before the accept future is polled; the TCP
    // listener backlog holds the daemon's dial.
    for _ in 0..20 {
        sender
            .send_to(datagram.as_bytes(), target)
            .await
            .expect("send the discovery announcement");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut peer = tokio::time::timeout(
        Duration::from_secs(10),
        handfast_testkit::tls_peer::accept(&listener, &config),
    )
    .await
    .expect("the daemon dials before the timeout")
    .expect("the testkit peer accepts the daemon");
    assert_eq!(
        peer.peer_identity()
            .body
            .get("deviceId")
            .and_then(Value::as_str),
        Some(daemon_id.as_str()),
        "the testkit peer authenticated the daemon"
    );
    wait_for_log(&log, "unpaired link authenticated", &mut daemon).await;

    pair(&mut peer, &config).await;
    wait_for_log(&log, "pairing accepted", &mut daemon).await;

    let port = free_payload_port();
    let (payload_listener, acceptor) = payload_server(&config, port);
    let bytes = b"discovered".to_vec();
    let expected = bytes.clone();
    let send = tokio::spawn(serve_payload(payload_listener, acceptor, bytes, None));
    peer.send(&share_file_wire(
        "discovered.bin",
        expected.len() as u64,
        port,
    ))
    .await
    .expect("send the share request");
    let saved = incoming_dir(directory.path()).join("discovered.bin");
    wait_for_file(&saved).await;
    assert_eq!(std::fs::read(&saved).expect("read the file"), expected);
    send.await.expect("the sender joins").expect("sent");
    assert_eq!(temp_entries(&incoming_dir(directory.path())), 0);

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}

#[tokio::test]
async fn running_daemon_advertises_share() {
    let _serial = serial().await;
    let directory = tempfile::tempdir().unwrap();
    let tcp_port = free_tcp_port();
    let identity = print_identity(directory.path(), tcp_port);
    let daemon_id = identity["body"]["deviceId"].as_str().unwrap().to_string();
    let log = directory.path().join("daemon.log");
    let mut daemon = spawn_daemon(directory.path(), tcp_port, free_udp_port(), None, &log);
    wait_for_state_file(directory.path(), &mut daemon).await;
    wait_for_log(&log, "TCP listener bound", &mut daemon).await;

    let config = testkit_peer();
    let peer = dial(
        SocketAddr::from((Ipv4Addr::LOCALHOST, tcp_port)),
        &daemon_id,
        &config,
    )
    .await
    .expect("the testkit peer completes the link");
    let body = &peer.peer_identity().body;
    let incoming = body
        .get("incomingCapabilities")
        .and_then(Value::as_array)
        .expect("the daemon advertises incoming capabilities");
    let outgoing = body
        .get("outgoingCapabilities")
        .and_then(Value::as_array)
        .expect("the daemon advertises outgoing capabilities");
    for capabilities in [incoming, outgoing] {
        assert!(
            capabilities.contains(&json!(types::SHARE_REQUEST)),
            "the running daemon advertises share: {capabilities:?}"
        );
        assert!(
            capabilities.contains(&json!(types::SHARE_REQUEST_UPDATE)),
            "the running daemon advertises share updates: {capabilities:?}"
        );
    }

    peer.close().await.expect("close the testkit link");
    stop(daemon);
}
