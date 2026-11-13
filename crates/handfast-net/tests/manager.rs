//! Link manager and admission policy tests.
//!
//! These drive the public manager API with the independent testkit peer. No
//! test reaches into the manager's state; every assertion is an event, a
//! closed socket or a policy rejection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use handfast_net::{
    Fingerprint, LinkEvent, LinkLimits, LinkListener, LinkManager, LinkManagerHandle,
    LocalEndpoint, NetError, TlsError, TlsIdentity, TrustTable,
};
use handfast_proto::{types, Capabilities, DeviceId, DeviceName, DeviceType, Identity};
use handfast_testkit::peer::PeerConfig;
use handfast_testkit::tls_peer::{accept, dial, TlsPeerConfig, TlsVersion};
use handfast_testkit::WirePacket;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const LOCAL_ID: &str = "00112233445566778899aabbccddeeff";
const PEER_ID: &str = "fedcba9876543210fedcba9876543210";

fn device_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("the test device ID is valid")
}

fn local_endpoint(id: &str) -> Arc<LocalEndpoint> {
    let tls = Arc::new(TlsIdentity::generate(device_id(id)).expect("generate the local identity"));
    let identity = Identity::new(
        tls.device_id().clone(),
        DeviceName::new("Handfast manager test"),
        DeviceType::Desktop,
        Capabilities::new([types::PING, types::PAIR], [types::PING])
            .expect("the test capabilities are valid"),
        Some(1716),
    );
    Arc::new(LocalEndpoint::new(tls, identity).expect("the local IDs match"))
}

fn peer(id: &str) -> TlsPeerConfig {
    let mut config = PeerConfig::client();
    config.device_id = id.to_string();
    TlsPeerConfig::ecdsa(config, TlsVersion::Tls13).expect("build the testkit peer")
}

fn ping() -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::PING.to_string(),
        body: json!({ "message": "manager test" })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: None,
        payload_transfer_info: None,
    }
}

fn pair_request() -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::PAIR.to_string(),
        body: json!({ "pair": true, "timestamp": 0 })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: None,
        payload_transfer_info: None,
    }
}

fn no_inbound_rate_limit() -> LinkLimits {
    LinkLimits {
        per_ip_interval: Duration::ZERO,
        ..LinkLimits::default()
    }
}

struct Harness {
    handle: LinkManagerHandle,
    events: mpsc::Receiver<LinkEvent>,
    task: JoinHandle<handfast_net::Result<()>>,
    local: Arc<LocalEndpoint>,
    address: SocketAddr,
}

async fn start(id: &str, trust: TrustTable, limits: LinkLimits, tick: Duration) -> Harness {
    let local = local_endpoint(id);
    let listener = LinkListener::bind_in_range(1716, Arc::clone(local.tls()))
        .await
        .expect("bind a listener in the KDE Connect range");
    let address = SocketAddr::from((
        [127, 0, 0, 1],
        listener
            .local_addr()
            .expect("read the bound address")
            .port(),
    ));
    let (events, events_rx) = mpsc::channel(256);
    let (handle, task) =
        LinkManager::start_with_tick(listener, Arc::clone(&local), trust, limits, events, tick);
    Harness {
        handle,
        events: events_rx,
        task,
        local,
        address,
    }
}

async fn next_event(events: &mut mpsc::Receiver<LinkEvent>) -> LinkEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event arrives before the timeout")
        .expect("the manager is still running")
}

async fn expect_connected(events: &mut mpsc::Receiver<LinkEvent>) -> bool {
    match next_event(events).await {
        LinkEvent::Connected { paired, .. } => paired,
        other => panic!("expected Connected, got {other:?}"),
    }
}

async fn expect_disconnected(events: &mut mpsc::Receiver<LinkEvent>) -> (DeviceId, String) {
    match next_event(events).await {
        LinkEvent::Disconnected { device_id, reason } => (device_id, reason),
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn simultaneous_discovery_completes_both_directions() {
    let mut a = start(
        "00112233445566778899aabbccddeeff",
        TrustTable::default(),
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;
    let mut b = start(
        "fedcba9876543210fedcba9876543210",
        TrustTable::default(),
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;

    a.handle
        .consider_peer(b.local.device_id().clone(), b.address);
    b.handle
        .consider_peer(a.local.device_id().clone(), a.address);

    for harness in [&mut a, &mut b] {
        assert!(!expect_connected(&mut harness.events).await);
        expect_disconnected(&mut harness.events).await;
        assert!(!expect_connected(&mut harness.events).await);
    }

    a.handle.shutdown();
    b.handle.shutdown();
    let _ = a.task.await;
    let _ = b.task.await;
}

#[tokio::test]
async fn replacing_peer_with_the_same_certificate_emits_disconnect_before_connect() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        no_inbound_rate_limit(),
        Duration::from_millis(100),
    )
    .await;
    let config = peer(PEER_ID);
    let mut first = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the first testkit dial succeeds");
    assert!(!expect_connected(&mut harness.events).await);
    let second = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the second testkit dial succeeds");

    let (disconnected, reason) = expect_disconnected(&mut harness.events).await;
    assert_eq!(disconnected, device_id(PEER_ID));
    assert_eq!(reason, "replaced");
    assert!(!expect_connected(&mut harness.events).await);

    let closed = tokio::time::timeout(Duration::from_secs(2), first.recv())
        .await
        .expect("the old peer sees the close");
    assert!(
        matches!(closed, Ok(None) | Err(_)),
        "the replaced link is closed"
    );
    drop(second);
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn different_certificate_for_the_same_device_is_rejected() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        no_inbound_rate_limit(),
        Duration::from_millis(100),
    )
    .await;
    let config = peer(PEER_ID);
    let mut first = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the first testkit dial succeeds");
    assert!(!expect_connected(&mut harness.events).await);

    let changed = peer(PEER_ID);
    let mut second = dial(harness.address, LOCAL_ID, &changed)
        .await
        .expect("the second testkit side completes its handshake");
    let closed = tokio::time::timeout(Duration::from_secs(2), second.recv())
        .await
        .expect("the new link is closed");
    assert!(
        matches!(closed, Ok(None) | Err(_)),
        "the different certificate is rejected"
    );

    let extra = tokio::time::timeout(Duration::from_millis(300), harness.events.recv()).await;
    assert!(
        extra.is_err(),
        "the manager keeps the old link and emits no replacement events"
    );

    first
        .send(&pair_request())
        .await
        .expect("the old link still writes");
    match next_event(&mut harness.events).await {
        LinkEvent::PairingRequest { peer, paired, .. } => {
            assert_eq!(peer.device_id, device_id(PEER_ID));
            assert!(!paired);
        }
        other => panic!("expected PairingRequest, got {other:?}"),
    }
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn pending_cap_closes_extra_connections() {
    let limits = LinkLimits {
        max_pending: 1,
        handshake_timeout: Duration::from_secs(5),
        ..LinkLimits::default()
    };
    let harness = start(
        LOCAL_ID,
        TrustTable::default(),
        limits,
        Duration::from_millis(100),
    )
    .await;
    let first = TcpStream::connect(harness.address)
        .await
        .expect("open the silent connection");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut second = TcpStream::connect(harness.address)
        .await
        .expect("open the extra connection");
    let mut buffer = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), second.read(&mut buffer))
        .await
        .expect("the pending cap closes the extra connection");
    assert_eq!(read.expect("read the close"), 0);
    drop(first);
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn unpaired_cap_rejects_the_43rd_link() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        no_inbound_rate_limit(),
        Duration::from_millis(100),
    )
    .await;
    let mut peers = Vec::new();
    for index in 0..42u128 {
        let id = format!("{index:032x}");
        let config = peer(&id);
        let testkit = dial(harness.address, LOCAL_ID, &config)
            .await
            .unwrap_or_else(|error| panic!("unpaired link {index}: {error}"));
        peers.push(testkit);
    }
    assert_eq!(peers.len(), 42, "all unpaired slots are held open");
    for index in 0..42 {
        match next_event(&mut harness.events).await {
            LinkEvent::Connected { .. } => {}
            other => panic!("expected Connected {index}, got {other:?}"),
        }
    }

    let overflow = peer("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(
        dial(harness.address, LOCAL_ID, &overflow).await.is_err(),
        "the 43rd unpaired link is rejected before TLS"
    );
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn inbound_rate_limit_rejects_a_second_connection_inside_the_window() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;
    let first = peer("00000000000000000000000000000001");
    let _first = dial(harness.address, LOCAL_ID, &first)
        .await
        .expect("the first inbound link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    let second = peer("00000000000000000000000000000002");
    assert!(
        dial(harness.address, LOCAL_ID, &second).await.is_err(),
        "the second connection inside 500 ms is rejected"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    let third = peer("00000000000000000000000000000003");
    let _third = dial(harness.address, LOCAL_ID, &third)
        .await
        .expect("the connection after the window is admitted");
    assert!(!expect_connected(&mut harness.events).await);
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn peer_table_recovers_after_a_discovery_flood() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;
    for index in 0..256u128 {
        let id = DeviceId::new(format!("{index:032x}")).expect("the flood ID is valid");
        harness
            .handle
            .consider_peer(id, SocketAddr::from(([127, 0, 0, 1], 1)));
        if index % 16 == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let real = peer(PEER_ID);
    let real_id = device_id(PEER_ID);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the real peer listener");
    let address = listener.local_addr().expect("read the real peer address");
    let server = tokio::spawn(async move { accept(&listener, &real).await });
    harness.handle.consider_peer(real_id.clone(), address);

    let _real = server
        .await
        .expect("the real peer task completes")
        .expect("the real peer is dialed after the flood");
    match next_event(&mut harness.events).await {
        LinkEvent::Connected { peer, .. } => assert_eq!(peer.device_id, real_id),
        other => panic!("expected Connected for the real peer, got {other:?}"),
    }
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn unpaired_idle_link_is_closed() {
    let limits = LinkLimits {
        unpaired_idle_timeout: Duration::from_millis(200),
        ..LinkLimits::default()
    };
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        limits,
        Duration::from_millis(50),
    )
    .await;
    let config = peer(PEER_ID);
    let _testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the unpaired link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    let (disconnected, _reason) = expect_disconnected(&mut harness.events).await;
    assert_eq!(disconnected, device_id(PEER_ID));
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn a_late_pairing_request_resets_the_unpaired_idle_timer() {
    let limits = LinkLimits {
        unpaired_idle_timeout: Duration::from_millis(400),
        ..LinkLimits::default()
    };
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        limits,
        Duration::from_millis(50),
    )
    .await;
    let config = peer(PEER_ID);
    let mut testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the unpaired link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    tokio::time::sleep(Duration::from_millis(300)).await;
    testkit
        .send(&pair_request())
        .await
        .expect("send the late pairing request");
    match next_event(&mut harness.events).await {
        LinkEvent::PairingRequest { .. } => {}
        other => panic!("expected PairingRequest, got {other:?}"),
    }

    let extra = tokio::time::timeout(Duration::from_millis(250), harness.events.recv()).await;
    assert!(extra.is_err(), "the pairing request reset the idle timer");
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn paired_idle_link_stays_open() {
    let config = peer(PEER_ID);
    let pin = Fingerprint::of_certificate(config.cert_der());
    let mut trust = TrustTable::default();
    trust.set(device_id(PEER_ID), pin);
    let limits = LinkLimits {
        unpaired_idle_timeout: Duration::from_millis(200),
        ..LinkLimits::default()
    };
    let mut harness = start(LOCAL_ID, trust, limits, Duration::from_millis(50)).await;
    let _testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the paired link is admitted");
    assert!(expect_connected(&mut harness.events).await);

    let extra = tokio::time::timeout(Duration::from_millis(400), harness.events.recv()).await;
    assert!(
        extra.is_err(),
        "a paired idle link is not closed by the unpaired timeout"
    );
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn shutdown_closes_every_link_and_stops_the_manager() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        no_inbound_rate_limit(),
        Duration::from_millis(100),
    )
    .await;
    let first = peer("00000000000000000000000000000001");
    let second = peer("00000000000000000000000000000002");
    let mut first_peer = dial(harness.address, LOCAL_ID, &first)
        .await
        .expect("the first link is admitted");
    let mut second_peer = dial(harness.address, LOCAL_ID, &second)
        .await
        .expect("the second link is admitted");
    assert!(!expect_connected(&mut harness.events).await);
    assert!(!expect_connected(&mut harness.events).await);

    harness.handle.shutdown();
    let result = tokio::time::timeout(Duration::from_secs(6), harness.task)
        .await
        .expect("the manager stops");
    result
        .expect("the manager task joins")
        .expect("the manager returns Ok");

    for testkit in [&mut first_peer, &mut second_peer] {
        let closed = tokio::time::timeout(Duration::from_secs(2), testkit.recv())
            .await
            .expect("the peer sees the close");
        assert!(
            matches!(closed, Ok(None) | Err(_)),
            "shutdown closes every link"
        );
    }
}

#[tokio::test]
async fn events_are_ordered_connected_packet_disconnected() {
    let config = peer(PEER_ID);
    let pin = Fingerprint::of_certificate(config.cert_der());
    let mut trust = TrustTable::default();
    trust.set(device_id(PEER_ID), pin);
    let mut harness = start(
        LOCAL_ID,
        trust,
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;
    let mut testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the paired link is admitted");
    assert!(expect_connected(&mut harness.events).await);

    testkit
        .send(&ping())
        .await
        .expect("send a ping through the paired link");
    match next_event(&mut harness.events).await {
        LinkEvent::Packet {
            device_id: packet_device,
            packet,
        } => {
            assert_eq!(packet_device, device_id(PEER_ID));
            assert_eq!(packet.ptype, types::PING);
        }
        other => panic!("expected Packet, got {other:?}"),
    }

    testkit.close().await.expect("close the testkit link");
    let (disconnected, _reason) = expect_disconnected(&mut harness.events).await;
    assert_eq!(disconnected, device_id(PEER_ID));
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn unpaired_gate_drops_packets_and_emits_pairing_requests() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;
    let config = peer(PEER_ID);
    let mut testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the unpaired link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    testkit.send(&ping()).await.expect("send a ping");
    testkit
        .send(&pair_request())
        .await
        .expect("send a pair request");
    match next_event(&mut harness.events).await {
        LinkEvent::PairingRequest {
            peer,
            request,
            paired,
            ..
        } => {
            assert_eq!(peer.device_id, device_id(PEER_ID));
            assert!(request.is_request());
            assert!(!paired);
        }
        other => panic!("expected PairingRequest, got {other:?}"),
    }
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn malformed_pairing_packet_closes_the_link() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;
    let config = peer(PEER_ID);
    let mut testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the unpaired link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    let mut malformed = pair_request();
    malformed.body.remove("pair");
    testkit
        .send(&malformed)
        .await
        .expect("send the malformed pair packet");
    let (disconnected, _reason) = expect_disconnected(&mut harness.events).await;
    assert_eq!(disconnected, device_id(PEER_ID));
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn pinning_an_unpaired_link_frees_its_unpaired_slot() {
    let limits = LinkLimits {
        max_unpaired_links: 1,
        per_ip_interval: Duration::ZERO,
        ..LinkLimits::default()
    };
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        limits,
        Duration::from_millis(100),
    )
    .await;
    let first_id = device_id("00000000000000000000000000000001");
    let first_config = peer(first_id.as_str());
    let first = dial(harness.address, LOCAL_ID, &first_config)
        .await
        .expect("the first unpaired link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    let second_config = peer("00000000000000000000000000000002");
    assert!(
        dial(harness.address, LOCAL_ID, &second_config)
            .await
            .is_err(),
        "the unpaired cap rejects the second link"
    );

    let pin = Fingerprint::of_certificate(first_config.cert_der());
    harness
        .handle
        .set_trust(first_id, Some(pin))
        .await
        .expect("the active certificate is pinned");

    let second = dial(harness.address, LOCAL_ID, &second_config)
        .await
        .expect("pinning the first link frees its unpaired slot");
    assert!(!expect_connected(&mut harness.events).await);

    drop(second);
    first.close().await.expect("close the first link");
    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn replacement_link_delivers_packets_on_the_new_generation() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        no_inbound_rate_limit(),
        Duration::from_millis(100),
    )
    .await;
    let config = peer(PEER_ID);
    let _first = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the first link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    let mut second = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the replacement link is admitted");
    let (disconnected, reason) = expect_disconnected(&mut harness.events).await;
    assert_eq!(disconnected, device_id(PEER_ID));
    assert_eq!(reason, "replaced");
    assert!(!expect_connected(&mut harness.events).await);

    second
        .send(&pair_request())
        .await
        .expect("the replacement link writes");
    match next_event(&mut harness.events).await {
        LinkEvent::PairingRequest { peer, .. } => {
            assert_eq!(peer.device_id, device_id(PEER_ID));
        }
        other => panic!("expected a packet from the replacement link, got {other:?}"),
    }

    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn outbound_backoff_resets_after_a_successful_handshake() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        no_inbound_rate_limit(),
        Duration::from_millis(100),
    )
    .await;
    let peer_id = device_id(PEER_ID);

    harness
        .handle
        .consider_peer(peer_id.clone(), SocketAddr::from(([127, 0, 0, 1], 1)));
    tokio::time::sleep(Duration::from_secs(1)).await;

    tokio::time::sleep(Duration::from_secs(5)).await;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the retry listener");
    let address = listener.local_addr().expect("read the retry address");
    let config = peer(PEER_ID);
    let server = tokio::spawn(async move { accept(&listener, &config).await });
    harness.handle.consider_peer(peer_id.clone(), address);
    let first = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("the sighting after the backoff dials")
        .expect("the retry task joins")
        .expect("the sighting after the backoff connects");
    assert!(!expect_connected(&mut harness.events).await);

    drop(first);
    let (disconnected, _) = expect_disconnected(&mut harness.events).await;
    assert_eq!(disconnected, peer_id);

    // A second failure must start a 5 s backoff, not the 10 s a preserved
    // failure count would produce.
    harness
        .handle
        .consider_peer(peer_id.clone(), SocketAddr::from(([127, 0, 0, 1], 1)));
    tokio::time::sleep(Duration::from_secs(1)).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the second retry listener");
    let address = listener
        .local_addr()
        .expect("read the second retry address");
    let config = peer(PEER_ID);
    let server = tokio::spawn(async move { accept(&listener, &config).await });
    harness.handle.consider_peer(peer_id.clone(), address);
    let _second = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the reset backoff dials at 5 s, not 10 s")
        .unwrap()
        .expect("the second retry connects");
    assert!(!expect_connected(&mut harness.events).await);

    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn set_trust_opens_the_gate_and_none_closes_the_link() {
    let mut harness = start(
        LOCAL_ID,
        TrustTable::default(),
        LinkLimits::default(),
        Duration::from_millis(100),
    )
    .await;
    let config = peer(PEER_ID);
    let peer_id = device_id(PEER_ID);
    let mut testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the unpaired link is admitted");
    assert!(!expect_connected(&mut harness.events).await);

    let other = peer("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let wrong_pin = Fingerprint::of_certificate(other.cert_der());
    let error = harness
        .handle
        .set_trust(peer_id.clone(), Some(wrong_pin))
        .await;
    assert!(
        matches!(error, Err(NetError::Tls(TlsError::PinMismatch { .. }))),
        "{error:?}"
    );

    let pin = Fingerprint::of_certificate(config.cert_der());
    harness
        .handle
        .set_trust(peer_id.clone(), Some(pin))
        .await
        .expect("the active certificate is pinned");
    testkit
        .send(&ping())
        .await
        .expect("send a ping through the now-paired link");
    match next_event(&mut harness.events).await {
        LinkEvent::Packet { packet, .. } => assert_eq!(packet.ptype, types::PING),
        other => panic!("expected Packet after the pin, got {other:?}"),
    }

    harness
        .handle
        .set_trust(peer_id, None)
        .await
        .expect("clearing trust succeeds");
    let (_, _reason) = expect_disconnected(&mut harness.events).await;
    harness.handle.shutdown();
    let _ = harness.task.await;
}
