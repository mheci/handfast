//! Reliable share channel tests at the daemon boundary.
//!
//! These drive the public manager API with the independent testkit peer. The
//! session runner tests live in `src/session.rs`; these two prove the manager
//! gate and the full share channel behavior the daemon depends on.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use handfast_net::{
    Fingerprint, LinkEvent, LinkLimits, LinkListener, LinkManager, LinkManagerHandle,
    LocalEndpoint, ShareEvent, TlsIdentity, TrustTable,
};
use handfast_proto::{types, Capabilities, DeviceId, DeviceName, DeviceType, Identity};
use handfast_testkit::payload_peer::{PayloadSender, PayloadSenderConfig};
use handfast_testkit::peer::PeerConfig;
use handfast_testkit::tls_peer::{dial, TlsPeerConfig, TlsVersion};
use handfast_testkit::WirePacket;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const LOCAL_ID: &str = "00112233445566778899aabbccddeeff";
const PEER_ID: &str = "fedcba9876543210fedcba9876543210";

// The manager listener and the testkit payload sender both bind inside
// 1716-1764. Serialize the tests so their port scans cannot race.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn device_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("the test device ID is valid")
}

fn local_endpoint(id: &str) -> (Arc<LocalEndpoint>, Arc<TlsIdentity>) {
    let tls = Arc::new(TlsIdentity::generate(device_id(id)).expect("generate the local identity"));
    let identity = Identity::new(
        tls.device_id().clone(),
        DeviceName::new("Handfast share test"),
        DeviceType::Desktop,
        Capabilities::default(),
        Some(1716),
    );
    let local = Arc::new(LocalEndpoint::new(Arc::clone(&tls), identity).expect("the IDs match"));
    (local, tls)
}

fn peer(id: &str) -> TlsPeerConfig {
    let mut config = PeerConfig::client();
    config.device_id = id.to_string();
    TlsPeerConfig::ecdsa(config, TlsVersion::Tls13).expect("build the testkit peer")
}

fn share_text() -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::SHARE_REQUEST.to_string(),
        body: json!({ "text": "share channel test" })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: None,
        payload_transfer_info: None,
    }
}

fn share_file(port: u16) -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::SHARE_REQUEST.to_string(),
        body: json!({ "filename": "secret.txt" })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: Some(1),
        payload_transfer_info: Some(json!({ "port": port })),
    }
}

fn payload_sender() -> PayloadSender {
    let mut config = PayloadSenderConfig::new(PeerConfig::client(), TlsVersion::Both);
    for port in handfast_proto::PAYLOAD_PORT_MIN..=handfast_proto::PAYLOAD_PORT_MAX {
        config.bind_port = Some(port);
        if let Ok(sender) = PayloadSender::new(config.clone()) {
            return sender;
        }
    }
    panic!("no free payload port in 1739-1764");
}

struct Harness {
    handle: LinkManagerHandle,
    events: mpsc::Receiver<LinkEvent>,
    share_events: mpsc::Receiver<ShareEvent>,
    task: JoinHandle<handfast_net::Result<()>>,
    address: SocketAddr,
}

async fn start(trust: TrustTable, share_queue: usize) -> Harness {
    let (local, tls) = local_endpoint(LOCAL_ID);
    let listener = LinkListener::bind_in_range(1716, tls)
        .await
        .expect("bind a listener in the KDE Connect range");
    let address = SocketAddr::from((
        [127, 0, 0, 1],
        listener
            .local_addr()
            .expect("read the bound address")
            .port(),
    ));
    let (events, events_rx) = mpsc::channel(64);
    let (share_events, share_events_rx) = mpsc::channel(share_queue);
    let (handle, task) = LinkManager::start_with_tick(
        listener,
        local,
        trust,
        LinkLimits::default(),
        events,
        share_events,
        Duration::from_millis(100),
    );
    Harness {
        handle,
        events: events_rx,
        share_events: share_events_rx,
        task,
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
        _ => panic!("expected Connected"),
    }
}

async fn expect_disconnected(events: &mut mpsc::Receiver<LinkEvent>) -> String {
    match next_event(events).await {
        LinkEvent::Disconnected { reason, .. } => reason,
        _ => panic!("expected Disconnected"),
    }
}

#[tokio::test]
async fn full_share_channel_closes_the_link_with_the_reason() {
    let _serial = SERIAL.lock().await;
    let config = peer(PEER_ID);
    let pin = Fingerprint::of_certificate(config.cert_der());
    let mut trust = TrustTable::default();
    trust.set(device_id(PEER_ID), pin);
    let mut harness = start(trust, 1).await;
    let mut testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the paired link is admitted");
    assert!(expect_connected(&mut harness.events).await);

    testkit
        .send(&share_text())
        .await
        .expect("fill the share queue");
    testkit
        .send(&share_text())
        .await
        .expect("overflow the share queue");

    let reason = expect_disconnected(&mut harness.events).await;
    assert_eq!(reason, "share event queue full");
    let closed = tokio::time::timeout(Duration::from_secs(2), testkit.recv())
        .await
        .expect("the peer sees the close");
    assert!(matches!(closed, Ok(None) | Err(_)), "the link closes");

    harness.handle.shutdown();
    let _ = harness.task.await;
}

#[tokio::test]
async fn unpaired_share_packet_never_reaches_the_plugin() {
    let _serial = SERIAL.lock().await;
    let config = peer(PEER_ID);
    let mut harness = start(TrustTable::default(), 32).await;
    let sender = payload_sender();
    let mut testkit = dial(harness.address, LOCAL_ID, &config)
        .await
        .expect("the unpaired link is admitted");
    assert!(
        !expect_connected(&mut harness.events).await,
        "the link stays unpaired"
    );

    testkit
        .send(&share_file(sender.port()))
        .await
        .expect("send the unpaired share request");

    let leaked =
        tokio::time::timeout(Duration::from_millis(500), harness.share_events.recv()).await;
    assert!(
        leaked.is_err(),
        "an unpaired share packet never reaches the share channel"
    );
    let leaked = tokio::time::timeout(Duration::from_millis(100), harness.events.recv()).await;
    assert!(
        leaked.is_err(),
        "an unpaired share packet never reaches the lossy packet channel"
    );

    harness.handle.shutdown();
    let _ = harness.task.await;
}
