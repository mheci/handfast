//! Payload service tests against the independent testkit payload peer.
//!
//! Every test that binds a port in 1739-1764 holds [`PORTS`] so parallel tests
//! in this binary cannot collide.

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_net::{
    PayloadError, PayloadLimits, PayloadPeer, PayloadService, PayloadSource, PeerCertificate,
    TlsIdentity,
};
use handfast_proto::{DeviceId, PayloadOffer, PayloadSize};
use handfast_testkit::payload_peer::{
    PayloadReceiver, PayloadReceiverConfig, PayloadSender, PayloadSenderConfig,
};
use handfast_testkit::peer::PeerConfig;
use handfast_testkit::tls_peer::{CertificateKind, TlsPeerConfig, TlsVersion};
use tokio::net::TcpStream;

const HANDFAST_DEVICE_ID: &str = "00112233445566778899aabbccddeeff";
const PEER_DEVICE_ID: &str = "fedcba9876543210fedcba9876543210";

static PORTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn ports() -> tokio::sync::MutexGuard<'static, ()> {
    PORTS.lock().await
}

fn device_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("the test device ID is valid")
}

fn payload_service(limits: PayloadLimits) -> (Arc<PayloadService>, Vec<u8>) {
    let identity = Arc::new(
        TlsIdentity::generate(device_id(HANDFAST_DEVICE_ID)).expect("generate the local identity"),
    );
    let certificate = identity.cert_der().to_vec();
    (Arc::new(PayloadService::new(identity, limits)), certificate)
}

fn limits() -> PayloadLimits {
    PayloadLimits {
        connect_timeout: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(2),
        accept_timeout: Duration::from_secs(2),
        io_timeout: Duration::from_secs(2),
        ..PayloadLimits::default()
    }
}

fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn source(bytes: &[u8]) -> PayloadSource {
    PayloadSource {
        size: Some(bytes.len() as u64),
        reader: Box::new(Cursor::new(bytes.to_vec())),
    }
}

fn peer(device: &str, address: SocketAddr, certificate: &[u8]) -> PayloadPeer {
    PayloadPeer {
        device_id: device_id(device),
        address,
        certificate: PeerCertificate::parse(certificate).expect("the testkit certificate parses"),
    }
}

fn dummy_peer() -> PayloadPeer {
    let trusted = TlsPeerConfig::ecdsa(PeerConfig::server(), TlsVersion::Both)
        .expect("build a test certificate");
    peer(
        PEER_DEVICE_ID,
        SocketAddr::from(([127, 0, 0, 1], 0)),
        trusted.cert_der(),
    )
}

fn free_port() -> u16 {
    for port in 1739..=1764 {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            drop(listener);
            return port;
        }
    }
    panic!("no free payload port in 1739-1764");
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[tokio::test]
async fn sender_streams_one_byte_one_mib_and_eight_mib() {
    let _ports = ports().await;
    for size in [1usize, 1024 * 1024, 8 * 1024 * 1024] {
        let (service, certificate) = payload_service(limits());
        let receiver = PayloadReceiver::new(PayloadReceiverConfig {
            expected_peer_certificate: Some(certificate),
            ..PayloadReceiverConfig::default()
        })
        .expect("build the testkit receiver");
        let receiver_certificate = receiver
            .cert_der()
            .expect("the receiver presents a certificate")
            .to_vec();
        let peer = peer(PEER_DEVICE_ID, loopback(0), &receiver_certificate);
        let bytes = payload(size);
        let prepared = service
            .prepare_send(peer, source(&bytes))
            .await
            .expect("bind the payload listener");
        let address = loopback(prepared.port());
        let mut sink = Vec::new();

        let (send, receive) = tokio::join!(
            prepared.start(None),
            receiver.receive(address, size as u64, &mut sink),
        );
        let send = send.expect("send the payload");
        let receive = receive.expect("receive the payload");

        assert_eq!(send.bytes, size as u64, "{size} bytes sent");
        assert_eq!(send.extra_bytes, 0);
        assert_eq!(receive.bytes, size as u64, "{size} bytes received");
        assert!(receive.eof);
        assert_eq!(sink, bytes);
    }
}

#[tokio::test]
async fn receiver_reads_one_byte_one_mib_and_eight_mib() {
    let _ports = ports().await;
    for size in [1usize, 1024 * 1024, 8 * 1024 * 1024] {
        let (service, certificate) = payload_service(limits());
        let port = free_port();
        let sender = PayloadSender::new(PayloadSenderConfig {
            bind_port: Some(port),
            expected_peer_certificate: Some(certificate),
            ..PayloadSenderConfig::default()
        })
        .expect("build the testkit sender");
        let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
        let bytes = payload(size);
        let offer = PayloadOffer {
            size: PayloadSize::Known(size as u64),
            port: sender.port(),
        };
        let mut sink = Vec::new();

        let (send, receive) = tokio::join!(
            sender.accept_and_send(&bytes),
            service.receive(peer, offer, &mut sink, None),
        );
        send.expect("send the payload");
        let outcome = receive.expect("receive the payload");

        assert_eq!(outcome.bytes, size as u64, "{size} bytes received");
        assert_eq!(outcome.extra_bytes, 0);
        assert_eq!(sink, bytes);
    }
}

#[tokio::test]
async fn tls_12_and_13_negotiate_with_ecdsa_and_rsa_peers() {
    let _ports = ports().await;
    for version in [TlsVersion::Tls12, TlsVersion::Tls13] {
        for certificate in [CertificateKind::Ecdsa, CertificateKind::Rsa] {
            let (service, local_certificate) = payload_service(limits());
            let receiver = PayloadReceiver::new(PayloadReceiverConfig {
                version,
                certificate,
                expected_peer_certificate: Some(local_certificate),
                ..PayloadReceiverConfig::default()
            })
            .expect("build the testkit receiver");
            let receiver_certificate = receiver
                .cert_der()
                .expect("the receiver presents a certificate")
                .to_vec();
            let peer = peer(PEER_DEVICE_ID, loopback(0), &receiver_certificate);
            let prepared = service
                .prepare_send(peer, source(b"x"))
                .await
                .expect("bind the payload listener");
            let address = loopback(prepared.port());
            let mut sink = Vec::new();
            let (send, receive) = tokio::join!(
                prepared.start(None),
                receiver.receive(address, 1, &mut sink),
            );
            send.expect("Handfast is the TLS server");
            receive.expect("the testkit receiver is the TLS client");
            assert_eq!(sink, b"x");

            let (service, local_certificate) = payload_service(limits());
            let port = free_port();
            let sender = PayloadSender::new(PayloadSenderConfig {
                version,
                certificate,
                bind_port: Some(port),
                expected_peer_certificate: Some(local_certificate),
                ..PayloadSenderConfig::default()
            })
            .expect("build the testkit sender");
            let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
            let offer = PayloadOffer {
                size: PayloadSize::Known(1),
                port: sender.port(),
            };
            let mut sink = Vec::new();
            let (send, receive) = tokio::join!(
                sender.accept_and_send(b"x"),
                service.receive(peer, offer, &mut sink, None),
            );
            send.expect("the testkit sender is the TLS server");
            receive.expect("Handfast is the TLS client");
            assert_eq!(sink, b"x");
        }
    }
}

#[tokio::test]
async fn truncated_payload_is_a_size_mismatch() {
    let _ports = ports().await;
    let (service, certificate) = payload_service(limits());
    let port = free_port();
    let bytes = payload(1024);
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        truncate_after: Some(512),
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Known(1024),
        port: sender.port(),
    };
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        sender.accept_and_send(&bytes),
        service.receive(peer, offer, &mut sink, None),
    );
    let _ = send;
    match receive {
        Err(PayloadError::SizeMismatch { expected, got }) => {
            assert_eq!(expected, 1024);
            assert_eq!(got, 512);
        }
        other => panic!("expected SizeMismatch, got {other:?}"),
    }
    assert_eq!(sink.len(), 512, "the truncated bytes reach the sink");
}

#[tokio::test]
async fn extra_bytes_within_the_cap_are_accepted_and_reported() {
    let _ports = ports().await;
    let (service, certificate) = payload_service(limits());
    let port = free_port();
    let extra = 1024 * 1024;
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        extra_bytes: extra,
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Known(1),
        port: sender.port(),
    };
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        sender.accept_and_send(b"x"),
        service.receive(peer, offer, &mut sink, None),
    );
    send.expect("send the payload");
    let outcome = receive.expect("extra bytes inside the cap are accepted");

    assert_eq!(outcome.bytes, 1 + extra);
    assert_eq!(outcome.extra_bytes, extra);
    assert_eq!(sink.len() as u64, 1 + extra);
}

#[tokio::test]
async fn extra_bytes_beyond_the_cap_are_too_large() {
    let _ports = ports().await;
    let (service, certificate) = payload_service(limits());
    let port = free_port();
    let extra = 64 * 1024 * 1024 + 1;
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        extra_bytes: extra,
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Known(1),
        port: sender.port(),
    };
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        sender.accept_and_send(b"x"),
        service.receive(peer, offer, &mut sink, None),
    );
    let _ = send;
    match receive {
        Err(PayloadError::TooLarge(cap)) => assert_eq!(cap, 1 + 64 * 1024 * 1024),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_size_reads_to_eof_and_caps_at_max_size() {
    let _ports = ports().await;

    let (service, certificate) = payload_service(limits());
    let port = free_port();
    let bytes = payload(4096);
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Unknown,
        port: sender.port(),
    };
    let mut sink = Vec::new();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(&bytes),
        service.receive(peer, offer, &mut sink, None),
    );
    send.expect("send the payload");
    let outcome = receive.expect("an unknown size reads to EOF");
    assert_eq!(outcome.bytes, bytes.len() as u64);
    assert_eq!(outcome.extra_bytes, 0);
    assert_eq!(sink, bytes);

    let mut limits = limits();
    limits.max_size = 1024;
    let (service, certificate) = payload_service(limits);
    let port = free_port();
    let bytes = payload(1025);
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Unknown,
        port: sender.port(),
    };
    let mut sink = Vec::new();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(&bytes),
        service.receive(peer, offer, &mut sink, None),
    );
    let _ = send;
    match receive {
        Err(PayloadError::TooLarge(cap)) => assert_eq!(cap, 1024),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_sender_certificate_is_rejected() {
    let _ports = ports().await;
    let (service, certificate) = payload_service(limits());
    let port = free_port();
    let trusted = TlsPeerConfig::ecdsa(PeerConfig::client(), TlsVersion::Both)
        .expect("build the trusted certificate");
    let trusted_certificate = trusted.cert_der().to_vec();
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        wrong_certificate: true,
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(port), &trusted_certificate);
    let offer = PayloadOffer {
        size: PayloadSize::Known(4),
        port,
    };
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        sender.accept_and_send(b"ping"),
        service.receive(peer, offer, &mut sink, None),
    );
    let _ = send;
    assert!(
        matches!(receive, Err(PayloadError::PeerCertificate)),
        "a different server certificate fails the exact DER check: {receive:?}"
    );
}

#[tokio::test]
async fn wrong_receiver_certificate_is_rejected() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.max_attempts = 1;
    let (service, certificate) = payload_service(limits);
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        wrong_certificate: true,
        expected_peer_certificate: Some(certificate),
        ..PayloadReceiverConfig::default()
    })
    .expect("build the testkit receiver");
    let trusted = TlsPeerConfig::ecdsa(PeerConfig::server(), TlsVersion::Both)
        .expect("build the trusted certificate");
    let trusted_certificate = trusted.cert_der().to_vec();
    let peer = peer(PEER_DEVICE_ID, loopback(0), &trusted_certificate);
    let prepared = service
        .prepare_send(peer, source(b"x"))
        .await
        .expect("bind the payload listener");
    let address = loopback(prepared.port());
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        prepared.start(None),
        receiver.receive(address, 1, &mut sink),
    );
    let _ = receive;
    assert!(
        matches!(send, Err(PayloadError::PeerCertificate)),
        "a different client certificate fails the exact DER check: {send:?}"
    );
}

#[tokio::test]
async fn missing_client_certificate_fails_the_handshake() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.max_attempts = 1;
    let (service, _certificate) = payload_service(limits);
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        present_client_certificate: false,
        ..PayloadReceiverConfig::default()
    })
    .expect("build the testkit receiver");
    let peer = dummy_peer();
    let prepared = service
        .prepare_send(peer, source(b"x"))
        .await
        .expect("bind the payload listener");
    let address = loopback(prepared.port());
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        prepared.start(None),
        receiver.receive(address, 1, &mut sink),
    );
    let _ = receive;
    assert!(
        matches!(
            send,
            Err(PayloadError::Io(_)) | Err(PayloadError::Timeout(_))
        ),
        "the server requires a client certificate: {send:?}"
    );
}

#[tokio::test]
async fn raw_tcp_peer_fails_the_handshake() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.max_attempts = 1;
    limits.handshake_timeout = Duration::from_millis(300);
    let (service, _certificate) = payload_service(limits);
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        raw_tcp: true,
        read_timeout: Duration::from_secs(2),
        ..PayloadReceiverConfig::default()
    })
    .expect("build the raw TCP receiver");
    let peer = dummy_peer();
    let prepared = service
        .prepare_send(peer, source(b"x"))
        .await
        .expect("bind the payload listener");
    let address = loopback(prepared.port());
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        prepared.start(None),
        receiver.receive(address, 1, &mut sink),
    );
    let _ = receive;
    assert!(
        matches!(send, Err(PayloadError::Timeout("payload handshake"))),
        "a raw TCP peer never sends a ClientHello: {send:?}"
    );
}

#[tokio::test]
async fn accept_times_out_with_no_receiver() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.accept_timeout = Duration::from_millis(200);
    let (service, _certificate) = payload_service(limits);
    let prepared = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("bind the payload listener");

    let started = Instant::now();
    let error = prepared
        .start(None)
        .await
        .expect_err("no receiver connects");
    assert!(
        matches!(error, PayloadError::Timeout("payload accept")),
        "{error:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(190));
}

#[tokio::test]
async fn start_after_the_bind_deadline_times_out() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.accept_timeout = Duration::from_millis(200);
    let (service, _certificate) = payload_service(limits);
    let prepared = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("bind the payload listener");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    let error = prepared
        .start(None)
        .await
        .expect_err("the bind-time deadline expired");
    assert!(
        matches!(error, PayloadError::Timeout("payload accept")),
        "{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "start fails without waiting for another accept"
    );
}

#[tokio::test]
async fn dropping_prepared_send_closes_the_port_and_releases_the_permit() {
    let _ports = ports().await;
    let port = free_port();
    let mut limits = limits();
    limits.port_min = port;
    limits.port_max = port;
    limits.max_active = 1;
    let (service, _certificate) = payload_service(limits);

    let prepared = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("bind the only port");
    assert_eq!(prepared.port(), port);
    drop(prepared);
    assert!(
        TcpStream::connect(loopback(port)).await.is_err(),
        "the listener is closed on drop"
    );
    let again = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("the permit is free and the port rebinds");
    assert_eq!(again.port(), port);
}

#[tokio::test]
async fn dropping_the_send_future_closes_the_port_and_releases_the_permit() {
    let _ports = ports().await;
    let port = free_port();
    let mut limits = limits();
    limits.port_min = port;
    limits.port_max = port;
    limits.max_active = 1;
    let (service, _certificate) = payload_service(limits);

    let prepared = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("bind the only port");
    let started = tokio::spawn(prepared.start(None));
    tokio::time::sleep(Duration::from_millis(50)).await;
    started.abort();
    let _ = started.await;
    assert!(
        TcpStream::connect(loopback(port)).await.is_err(),
        "the listener is closed when the send future is dropped"
    );
    let again = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("the permit is free and the port rebinds");
    assert_eq!(again.port(), port);
}

#[tokio::test]
async fn dropping_the_receive_future_closes_the_socket_and_releases_the_permit() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.max_active = 1;
    limits.max_pending_connects = 1;
    let (service, certificate) = payload_service(limits);

    let port = free_port();
    let stalled_sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        stall_before_write: Some(Duration::from_secs(2)),
        expected_peer_certificate: Some(certificate.clone()),
        ..PayloadSenderConfig::default()
    })
    .expect("build the stalled sender");
    let peer = peer(
        PEER_DEVICE_ID,
        loopback(stalled_sender.port()),
        stalled_sender.cert_der(),
    );
    let offer = PayloadOffer {
        size: PayloadSize::Known(1),
        port: stalled_sender.port(),
    };
    let pending = service
        .connect(peer, offer)
        .await
        .expect("hold the raw connection");
    let stalled = tokio::spawn(async move {
        let bytes = [0x42u8];
        stalled_sender.accept_and_send(&bytes).await
    });
    let receiving = tokio::spawn({
        let service = Arc::clone(&service);
        async move {
            let mut sink = Vec::new();
            service.receive_pending(pending, &mut sink, None).await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    receiving.abort();
    let _ = receiving.await;
    stalled.abort();
    let _ = stalled.await;

    let port = free_port();
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Known(1),
        port: sender.port(),
    };
    let mut sink = Vec::new();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(b"x"),
        service.receive(peer, offer, &mut sink, None),
    );
    send.expect("send after the cancellation");
    receive.expect("the cancelled transfer released both permits");
    assert_eq!(sink, b"x");
}

#[tokio::test]
async fn concurrent_transfers_complete() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.max_active = 2;
    limits.max_pending_connects = 2;
    let (service, certificate) = payload_service(limits);

    let mut transfers = Vec::new();
    for _ in 0..2 {
        let receiver = PayloadReceiver::new(PayloadReceiverConfig {
            expected_peer_certificate: Some(certificate.clone()),
            ..PayloadReceiverConfig::default()
        })
        .expect("build the testkit receiver");
        let receiver_certificate = receiver
            .cert_der()
            .expect("the receiver presents a certificate")
            .to_vec();
        let peer = peer(PEER_DEVICE_ID, loopback(0), &receiver_certificate);
        let bytes = payload(64 * 1024);
        let prepared = service
            .prepare_send(peer, source(&bytes))
            .await
            .expect("bind a payload listener");
        let address = loopback(prepared.port());
        transfers.push((prepared, receiver, address, bytes));
    }
    let (first, second) = (transfers.remove(0), transfers.remove(0));
    let mut first_sink = Vec::new();
    let mut second_sink = Vec::new();

    let (first_send, first_receive, second_send, second_receive) = tokio::join!(
        first.0.start(None),
        first
            .1
            .receive(first.2, first.3.len() as u64, &mut first_sink),
        second.0.start(None),
        second
            .1
            .receive(second.2, second.3.len() as u64, &mut second_sink),
    );
    first_send.expect("the first transfer completes");
    first_receive.expect("the first receiver completes");
    second_send.expect("the second transfer completes");
    second_receive.expect("the second receiver completes");
    assert_eq!(first_sink, first.3);
    assert_eq!(second_sink, second.3);
}

#[tokio::test]
async fn active_cap_waits_then_times_out() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.max_active = 1;
    limits.accept_timeout = Duration::from_millis(200);
    let (service, _certificate) = payload_service(limits);

    let first = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("the first active permit");
    let started = Instant::now();
    let error = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect_err("the active pool is full");
    assert!(
        matches!(error, PayloadError::Timeout("payload active pool")),
        "{error:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(190));
    drop(first);
    service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect("the permit is released");
}

#[tokio::test]
async fn pending_cap_waits_then_times_out() {
    let _ports = ports().await;
    let port = free_port();
    let mut limits = limits();
    limits.max_pending_connects = 1;
    limits.connect_timeout = Duration::from_millis(200);
    let (service, _certificate) = payload_service(limits);
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Known(1),
        port: sender.port(),
    };

    let first = service
        .connect(peer.clone(), offer)
        .await
        .expect("the first pending permit");
    let started = Instant::now();
    let error = service
        .connect(peer.clone(), offer)
        .await
        .expect_err("the pending pool is full");
    assert!(
        matches!(error, PayloadError::Timeout("payload pending pool")),
        "{error:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(190));
    drop(first);
    let second = service
        .connect(peer, offer)
        .await
        .expect("the pending permit is released");
    drop(second);
}

#[tokio::test]
async fn ports_outside_the_range_are_rejected_before_dialing() {
    let (service, _certificate) = payload_service(limits());
    let peer = dummy_peer();
    for port in [1716, 1765] {
        let error = service
            .connect(
                peer.clone(),
                PayloadOffer {
                    size: PayloadSize::Known(1),
                    port,
                },
            )
            .await
            .expect_err("the port is outside the payload range");
        assert!(
            matches!(error, PayloadError::PortOutOfRange(rejected) if rejected == port),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn no_port_when_the_only_port_is_taken() {
    let _ports = ports().await;
    let port = free_port();
    let occupied =
        std::net::TcpListener::bind(("0.0.0.0", port)).expect("occupy the only payload port");
    let mut limits = limits();
    limits.port_min = port;
    limits.port_max = port;
    let (service, _certificate) = payload_service(limits);

    let error = service
        .prepare_send(dummy_peer(), source(b"x"))
        .await
        .expect_err("the only port is taken");
    assert!(matches!(error, PayloadError::NoPort), "{error:?}");
    drop(occupied);
}

#[tokio::test]
async fn queued_connection_defers_the_tls_handshake() {
    let _ports = ports().await;
    let port = free_port();
    let mut limits = limits();
    limits.connect_timeout = Duration::from_secs(1);
    let (service, certificate) = payload_service(limits);
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Known(4),
        port: sender.port(),
    };

    let started = Instant::now();
    let pending = service
        .connect(peer, offer)
        .await
        .expect("hold the raw connection before the sender accepts");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "connect returns before the TLS handshake"
    );

    let mut sink = Vec::new();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(b"ping"),
        service.receive_pending(pending, &mut sink, None),
    );
    send.expect("send the payload after the deferred handshake");
    let outcome = receive.expect("receive the payload after the deferred handshake");
    assert_eq!(outcome.bytes, 4);
    assert_eq!(sink, b"ping");
}

#[tokio::test]
async fn failed_attempt_then_valid_client_succeeds() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.accept_timeout = Duration::from_secs(5);
    limits.handshake_timeout = Duration::from_millis(500);
    let (service, certificate) = payload_service(limits);
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        expected_peer_certificate: Some(certificate),
        ..PayloadReceiverConfig::default()
    })
    .expect("build the testkit receiver");
    let receiver_certificate = receiver
        .cert_der()
        .expect("the receiver presents a certificate")
        .to_vec();
    let peer = peer(PEER_DEVICE_ID, loopback(0), &receiver_certificate);
    let bytes = payload(1024);
    let prepared = service
        .prepare_send(peer, source(&bytes))
        .await
        .expect("bind the payload listener");
    let address = loopback(prepared.port());
    let started = tokio::spawn(prepared.start(None));

    let raw = TcpStream::connect(address)
        .await
        .expect("connect the failed attempt");
    drop(raw);
    let mut sink = Vec::new();
    let receive = receiver
        .receive(address, bytes.len() as u64, &mut sink)
        .await;
    let send = started.await.expect("the sender task completes");

    receive.expect("the valid client receives the payload");
    let outcome = send.expect("the retry succeeds");
    assert_eq!(outcome.bytes, bytes.len() as u64);
    assert_eq!(sink, bytes);
}

#[tokio::test]
async fn sender_write_deadline_fires_when_the_receiver_stalls() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.io_timeout = Duration::from_millis(300);
    let (service, certificate) = payload_service(limits);
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        stall_before_read: Some(Duration::from_secs(2)),
        expected_peer_certificate: Some(certificate),
        ..PayloadReceiverConfig::default()
    })
    .expect("build the testkit receiver");
    let receiver_certificate = receiver
        .cert_der()
        .expect("the receiver presents a certificate")
        .to_vec();
    let peer = peer(PEER_DEVICE_ID, loopback(0), &receiver_certificate);
    let bytes = payload(64 * 1024 * 1024);
    let prepared = service
        .prepare_send(peer, source(&bytes))
        .await
        .expect("bind the payload listener");
    let address = loopback(prepared.port());
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        prepared.start(None),
        receiver.receive(address, bytes.len() as u64, &mut sink),
    );
    let _ = receive;
    assert!(
        matches!(send, Err(PayloadError::Timeout("payload write"))),
        "the sender write deadline fires: {send:?}"
    );
}

#[tokio::test]
async fn sender_stall_triggers_the_receiver_idle_timeout() {
    let _ports = ports().await;
    let mut limits = limits();
    limits.io_timeout = Duration::from_millis(300);
    let (service, certificate) = payload_service(limits);
    let port = free_port();
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        stall_before_write: Some(Duration::from_secs(2)),
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let offer = PayloadOffer {
        size: PayloadSize::Known(4),
        port: sender.port(),
    };
    let mut sink = Vec::new();

    let (send, receive) = tokio::join!(
        sender.accept_and_send(b"ping"),
        service.receive(peer, offer, &mut sink, None),
    );
    let _ = send;
    assert!(
        matches!(receive, Err(PayloadError::Timeout("payload read"))),
        "the receiver idle read deadline fires: {receive:?}"
    );
}

#[tokio::test]
async fn progress_reports_the_bytes_received() {
    let _ports = ports().await;
    let (service, certificate) = payload_service(limits());
    let port = free_port();
    let sender = PayloadSender::new(PayloadSenderConfig {
        bind_port: Some(port),
        expected_peer_certificate: Some(certificate),
        ..PayloadSenderConfig::default()
    })
    .expect("build the testkit sender");
    let peer = peer(PEER_DEVICE_ID, loopback(sender.port()), sender.cert_der());
    let bytes = payload(1024 * 1024);
    let offer = PayloadOffer {
        size: PayloadSize::Known(bytes.len() as u64),
        port: sender.port(),
    };
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);
    let mut sink = Vec::new();

    let receiving = async {
        let (send, receive) = tokio::join!(
            sender.accept_and_send(&bytes),
            service.receive(peer, offer, &mut sink, Some(progress_tx)),
        );
        send.expect("send the payload");
        receive.expect("receive the payload")
    };
    let collecting = async {
        let mut updates = Vec::new();
        while let Some(bytes) = progress_rx.recv().await {
            updates.push(bytes);
        }
        updates
    };
    let (outcome, updates) = tokio::join!(receiving, collecting);

    assert_eq!(outcome.bytes, bytes.len() as u64);
    assert_eq!(updates.last(), Some(&(bytes.len() as u64)));
}
