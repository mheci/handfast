//! Full-link tests against the independent testkit TLS peer.
//!
//! The testkit dialer sends the plaintext hello and acts as the TLS server; the
//! testkit listener reads the hello and acts as the TLS client. No test shares
//! framing, TLS or identity code with `handfast-net`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use handfast_net::{
    connect, Fingerprint, LinkLimits, LinkListener, LinkPolicy, LocalEndpoint, NetError, PeerTrust,
    TlsError, TlsIdentity, TrustTable,
};
use handfast_proto::{
    types, Capabilities, DeviceId, DeviceName, DeviceType, Identity, Packet, ProtoError,
};
use handfast_testkit::frame::MAX_FRAME_SIZE;
use handfast_testkit::tls_peer::{
    accept, connect_silent, dial, dial_with_hello, CertificateKind, TlsPeerConfig, TlsVersion,
};
use handfast_testkit::WirePacket;
use rustls::ProtocolVersion;
use serde_json::{json, Value};
use tokio::net::TcpListener;

const HANDFAST_ID: &str = "00112233445566778899aabbccddeeff";
const OTHER_ID: &str = "fedcba9876543210fedcba9876543210";

fn device_id(value: &str) -> DeviceId {
    DeviceId::new(value).expect("the test device ID is valid")
}

fn local_endpoint(device_id_value: &str) -> Arc<LocalEndpoint> {
    let tls = Arc::new(
        TlsIdentity::generate(device_id(device_id_value)).expect("generate the local identity"),
    );
    let identity = Identity::new(
        tls.device_id().clone(),
        DeviceName::new("Handfast link test"),
        DeviceType::Desktop,
        Capabilities::new([types::PING], [types::PING]).expect("the test capabilities are valid"),
        Some(1716),
    );
    Arc::new(LocalEndpoint::new(tls, identity).expect("the local IDs match"))
}

async fn bind(local: &Arc<LocalEndpoint>) -> (LinkListener, SocketAddr) {
    let listener = LinkListener::bind_in_range(1716, Arc::clone(local.tls()))
        .await
        .expect("bind a port in the KDE Connect range");
    let address = loopback(listener.local_addr().expect("read the bound address"));
    (listener, address)
}

fn loopback(address: SocketAddr) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], address.port()))
}

fn limits() -> LinkLimits {
    LinkLimits::default()
}

fn policy() -> LinkPolicy {
    LinkPolicy::new(LinkLimits::default())
}

fn trust_for(peer_id: &DeviceId, pin: Fingerprint) -> TrustTable {
    let mut trust = TrustTable::default();
    trust.set(peer_id.clone(), pin);
    trust
}

fn peer_config(version: TlsVersion, certificate: CertificateKind) -> TlsPeerConfig {
    match certificate {
        CertificateKind::Ecdsa => {
            TlsPeerConfig::ecdsa(handfast_testkit::PeerConfig::client(), version)
                .expect("build the ECDSA testkit peer")
        }
        CertificateKind::Rsa => TlsPeerConfig::rsa(version),
    }
}

fn echo() -> WirePacket {
    WirePacket {
        id: 0,
        ptype: types::PING.to_string(),
        body: json!({ "message": "echo" })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        payload_size: None,
        payload_transfer_info: None,
    }
}

#[tokio::test]
async fn listener_completes_a_full_link_with_a_pinned_testkit_dialer() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let peer_id = device_id(&peer.identity.device_id);
    let pin = Fingerprint::of_certificate(peer.cert_der());
    let trust = trust_for(&peer_id, pin);
    let dialer = tokio::spawn(async move { dial(address, HANDFAST_ID, &peer).await });

    let link = listener
        .accept(local.as_ref(), &trust, &limits(), &policy())
        .await
        .expect("the listener accepts the testkit dialer");
    let mut peer = dialer
        .await
        .expect("the dialer task completes")
        .expect("the testkit dial succeeds");

    assert_eq!(link.peer().device_id(), &peer_id, "encrypted identity");
    assert_eq!(
        link.peer().certificate.device_id(),
        &peer_id,
        "certificate CN"
    );
    assert_eq!(
        link.peer().certificate.fingerprint(),
        pin,
        "pinned certificate"
    );
    assert!(link.peer().remote_addr.ip().is_loopback());
    assert_eq!(peer.protocol_version(), ProtocolVersion::TLSv1_3);
    assert_eq!(
        peer.peer_identity()
            .body
            .get("deviceId")
            .and_then(Value::as_str),
        Some(HANDFAST_ID),
        "the listener sent its encrypted identity"
    );

    let (_, mut reader, mut writer, _permit) = link.into_parts();
    writer
        .send(&Packet::empty(types::PING).expect("build a ping"))
        .await
        .expect("send a ping through the link");
    let ping = peer
        .recv()
        .await
        .expect("the testkit peer reads the ping")
        .expect("the testkit peer did not see EOF");
    assert_eq!(ping.ptype, types::PING);

    peer.send(&echo()).await.expect("send the echo");
    let reply = reader
        .recv()
        .await
        .expect("the link reads the echo")
        .expect("the link did not see EOF");
    assert_eq!(reply.ptype, types::PING);
    assert_eq!(
        reply.body.get("message").and_then(Value::as_str),
        Some("echo")
    );
}

#[tokio::test]
async fn dialer_completes_a_full_link_with_a_pinned_testkit_listener() {
    let local = local_endpoint(HANDFAST_ID);
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let peer_id = device_id(&peer.identity.device_id);
    let pin = Fingerprint::of_certificate(peer.cert_der());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the testkit listener");
    let address = listener.local_addr().expect("read the testkit address");
    let server = tokio::spawn(async move { accept(&listener, &peer).await });

    let policy = policy();
    let permit = policy
        .reserve_outgoing(&peer_id, PeerTrust::Pinned(pin))
        .expect("reserve the outbound slot");
    let link = connect(
        address,
        local.as_ref(),
        &peer_id,
        PeerTrust::Pinned(pin),
        &limits(),
        permit,
    )
    .await
    .expect("the dialer completes the link");
    let peer = server
        .await
        .expect("the listener task completes")
        .expect("the testkit accept succeeds");

    let hello = peer
        .peer_hello()
        .expect("the testkit listener received the hello");
    assert_eq!(hello.ptype, types::IDENTITY);
    assert_eq!(
        hello.body.get("deviceId").and_then(Value::as_str),
        Some(HANDFAST_ID)
    );
    assert_eq!(
        hello.body.get("deviceName").and_then(Value::as_str),
        Some("Handfast link test")
    );
    assert_eq!(
        hello.body.get("deviceType").and_then(Value::as_str),
        Some("desktop")
    );
    assert!(
        hello
            .body
            .get("incomingCapabilities")
            .and_then(Value::as_array)
            .is_some(),
        "the hello carries the full identity"
    );
    assert_eq!(
        hello.body.get("targetDeviceId").and_then(Value::as_str),
        Some(peer_id.as_str())
    );
    assert_eq!(
        hello
            .body
            .get("targetProtocolVersion")
            .and_then(Value::as_u64),
        Some(8)
    );

    assert_eq!(peer.protocol_version(), ProtocolVersion::TLSv1_3);
    assert_eq!(link.peer().device_id(), &peer_id);
    assert_eq!(link.peer().certificate.device_id(), &peer_id);
    assert_eq!(link.peer().certificate.fingerprint(), pin);
    assert_eq!(
        link.peer().identity.device_name.as_str(),
        "Handfast test peer"
    );
}

#[tokio::test]
async fn listener_accepts_every_tls_version_and_certificate() {
    for (version, expected) in [
        (TlsVersion::Tls12, ProtocolVersion::TLSv1_2),
        (TlsVersion::Tls13, ProtocolVersion::TLSv1_3),
    ] {
        for certificate in [CertificateKind::Ecdsa, CertificateKind::Rsa] {
            let local = local_endpoint(HANDFAST_ID);
            let (listener, address) = bind(&local).await;
            let peer = peer_config(version, certificate);
            let peer_id = device_id(&peer.identity.device_id);
            let dialer = tokio::spawn(async move { dial(address, HANDFAST_ID, &peer).await });

            let link = listener
                .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
                .await
                .unwrap_or_else(|error| panic!("accept {version:?} {certificate:?}: {error}"));
            let peer = dialer
                .await
                .expect("the dialer task completes")
                .unwrap_or_else(|error| panic!("dial {version:?} {certificate:?}: {error}"));

            assert_eq!(
                peer.protocol_version(),
                expected,
                "negotiated {version:?} with {certificate:?}"
            );
            assert_eq!(
                link.peer().device_id(),
                &peer_id,
                "{version:?} {certificate:?} encrypted identity"
            );
            assert_eq!(
                link.peer().certificate.device_id(),
                &peer_id,
                "{version:?} {certificate:?} certificate CN"
            );
        }
    }
}

#[tokio::test]
async fn dialer_connects_with_every_tls_version_and_certificate() {
    for (version, expected) in [
        (TlsVersion::Tls12, ProtocolVersion::TLSv1_2),
        (TlsVersion::Tls13, ProtocolVersion::TLSv1_3),
    ] {
        for certificate in [CertificateKind::Ecdsa, CertificateKind::Rsa] {
            let local = local_endpoint(HANDFAST_ID);
            let peer = peer_config(version, certificate);
            let peer_id = device_id(&peer.identity.device_id);
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind the testkit listener");
            let address = listener.local_addr().expect("read the testkit address");
            let server = tokio::spawn(async move { accept(&listener, &peer).await });

            let policy = policy();
            let permit = policy
                .reserve_outgoing(&peer_id, PeerTrust::Unpaired)
                .expect("reserve the outbound slot");
            let link = connect(
                address,
                local.as_ref(),
                &peer_id,
                PeerTrust::Unpaired,
                &limits(),
                permit,
            )
            .await
            .unwrap_or_else(|error| panic!("connect {version:?} {certificate:?}: {error}"));
            let peer = server
                .await
                .expect("the listener task completes")
                .unwrap_or_else(|error| panic!("accept {version:?} {certificate:?}: {error}"));

            assert_eq!(
                peer.protocol_version(),
                expected,
                "negotiated {version:?} with {certificate:?}"
            );
            assert_eq!(
                link.peer().device_id(),
                &peer_id,
                "{version:?} {certificate:?} encrypted identity"
            );
            assert_eq!(
                link.peer().certificate.device_id(),
                &peer_id,
                "{version:?} {certificate:?} certificate CN"
            );
        }
    }
}

#[tokio::test]
async fn listener_accepts_a_desktop_shaped_hello() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let peer_id = device_id(&peer.identity.device_id);
    let hello = peer.desktop_hello_packet(HANDFAST_ID);
    assert!(
        hello.body.get("deviceType").is_none(),
        "the desktop hello omits the type"
    );
    assert!(
        hello.body.get("incomingCapabilities").is_none(),
        "the desktop hello omits the capabilities"
    );
    let dialer =
        tokio::spawn(async move { dial_with_hello(address, HANDFAST_ID, &peer, &hello).await });

    let link = listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
        .expect("the listener accepts the desktop hello");
    let peer = dialer
        .await
        .expect("the dialer task completes")
        .expect("the dial succeeds");

    assert_eq!(link.peer().device_id(), &peer_id);
    assert_eq!(peer.protocol_version(), ProtocolVersion::TLSv1_3);
}

#[tokio::test]
async fn listener_accepts_a_string_target_version() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let peer_id = device_id(&peer.identity.device_id);
    let hello = peer.android_hello_packet(HANDFAST_ID);
    assert_eq!(
        hello
            .body
            .get("targetProtocolVersion")
            .and_then(Value::as_str),
        Some("8"),
        "the Android hello sends the target version as a string"
    );
    let dialer =
        tokio::spawn(async move { dial_with_hello(address, HANDFAST_ID, &peer, &hello).await });

    let link = listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
        .expect("the listener accepts the string target version");
    let peer = dialer
        .await
        .expect("the dialer task completes")
        .expect("the dial succeeds");

    assert_eq!(link.peer().device_id(), &peer_id);
    assert_eq!(peer.protocol_version(), ProtocolVersion::TLSv1_3);
}

#[tokio::test]
async fn correlates_the_hello_certificate_and_encrypted_identity() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let peer_id = device_id(&peer.identity.device_id);
    let hello = peer.hello_packet(HANDFAST_ID);
    let hello_id = hello
        .body
        .get("deviceId")
        .and_then(Value::as_str)
        .expect("the hello names its device")
        .to_string();
    let dialer =
        tokio::spawn(async move { dial_with_hello(address, HANDFAST_ID, &peer, &hello).await });

    let link = listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
        .expect("the link correlates all three device IDs");
    dialer
        .await
        .expect("the dialer task completes")
        .expect("the dial succeeds");

    assert_eq!(link.peer().device_id().as_str(), hello_id.as_str());
    assert_eq!(link.peer().certificate.device_id(), link.peer().device_id());
    assert_eq!(link.peer().device_id(), &peer_id);
}

#[tokio::test]
async fn framed_ping_round_trips_after_the_handshake() {
    let local = local_endpoint(HANDFAST_ID);
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let peer_id = device_id(&peer.identity.device_id);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the testkit listener");
    let address = listener.local_addr().expect("read the testkit address");
    let server = tokio::spawn(async move { accept(&listener, &peer).await });

    let policy = policy();
    let permit = policy
        .reserve_outgoing(&peer_id, PeerTrust::Unpaired)
        .expect("reserve the outbound slot");
    let link = connect(
        address,
        local.as_ref(),
        &peer_id,
        PeerTrust::Unpaired,
        &limits(),
        permit,
    )
    .await
    .expect("the dialer completes the link");
    let mut peer = server
        .await
        .expect("the listener task completes")
        .expect("the testkit accept succeeds");

    let (_, mut reader, mut writer, _permit) = link.into_parts();
    writer
        .send(&Packet::empty(types::PING).expect("build a ping"))
        .await
        .expect("send the ping");
    let ping = peer
        .recv()
        .await
        .expect("the testkit peer reads the ping")
        .expect("the testkit peer did not see EOF");
    assert_eq!(ping.ptype, types::PING);

    peer.send(&echo()).await.expect("send the echo");
    let reply = reader
        .recv()
        .await
        .expect("the link reads the echo")
        .expect("the link did not see EOF");
    assert_eq!(reply.ptype, types::PING);
    assert_eq!(
        reply.body.get("message").and_then(Value::as_str),
        Some("echo")
    );
}

#[tokio::test]
async fn listener_rejects_a_dialer_certificate_with_a_wrong_common_name() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = TlsPeerConfig::ecdsa_with_common_name(
        handfast_testkit::PeerConfig::client(),
        TlsVersion::Tls13,
        OTHER_ID,
    )
    .expect("build the wrong-CN testkit peer");
    let dialer = tokio::spawn(async move { dial(address, HANDFAST_ID, &peer).await });

    let error = match listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
    {
        Ok(_) => panic!("a wrong common name must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(error, NetError::Tls(TlsError::CertificateDeviceId { .. })),
        "{error}"
    );
    drop(dialer);
}

#[tokio::test]
async fn listener_rejects_a_pinned_peer_whose_certificate_changed() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let peer_id = device_id(&peer.identity.device_id);
    let other = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let pin = Fingerprint::of_certificate(other.cert_der());
    let trust = trust_for(&peer_id, pin);
    let dialer = tokio::spawn(async move { dial(address, HANDFAST_ID, &peer).await });

    let error = match listener
        .accept(local.as_ref(), &trust, &limits(), &policy())
        .await
    {
        Ok(_) => panic!("a changed certificate must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(error, NetError::Tls(TlsError::PinMismatch { .. })),
        "{error}"
    );
    assert_eq!(
        trust.trust(&peer_id),
        PeerTrust::Pinned(pin),
        "a mismatch does not clear the pin"
    );
    drop(dialer);
}

#[tokio::test]
async fn listener_rejects_a_plaintext_identity_and_an_encrypted_identity_that_differ() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer =
        peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa).with_encrypted_device_id(OTHER_ID);
    let dialer = tokio::spawn(async move { dial(address, HANDFAST_ID, &peer).await });

    let error = match listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
    {
        Ok(_) => panic!("a plaintext and encrypted identity mismatch must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(error, NetError::Tls(TlsError::IdentityDeviceId { .. })),
        "{error}"
    );
    drop(dialer);
}

#[tokio::test]
async fn listener_rejects_a_target_protocol_version_other_than_eight() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let mut hello = peer.hello_packet(HANDFAST_ID);
    hello
        .body
        .insert("targetProtocolVersion".to_string(), json!(7));
    let dialer =
        tokio::spawn(async move { dial_with_hello(address, HANDFAST_ID, &peer, &hello).await });

    let error = match listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
    {
        Ok(_) => panic!("a non-8 target version must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(error, NetError::Proto(ProtoError::Identity(_))),
        "{error}"
    );
    drop(dialer);
}

#[tokio::test]
async fn listener_rejects_an_encrypted_identity_that_is_not_protocol_eight() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer =
        peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa).with_encrypted_protocol_version(7);
    let dialer = tokio::spawn(async move { dial(address, HANDFAST_ID, &peer).await });

    let error = match listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
    {
        Ok(_) => panic!("a non-8 encrypted identity must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(error, NetError::Proto(ProtoError::Identity(_))),
        "{error}"
    );
    drop(dialer);
}

#[tokio::test]
async fn listener_times_out_a_silent_peer() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let silent = connect_silent(address)
        .await
        .expect("connect without sending a hello");
    let limits = LinkLimits {
        handshake_timeout: Duration::from_millis(200),
        ..LinkLimits::default()
    };

    let error = match listener
        .accept(local.as_ref(), &TrustTable::default(), &limits, &policy())
        .await
    {
        Ok(_) => panic!("a silent peer must time out"),
        Err(error) => error,
    };
    assert!(
        matches!(error, NetError::Timeout("plaintext hello")),
        "{error}"
    );
    drop(silent);
}

#[tokio::test]
async fn listener_closes_a_peer_that_declares_an_oversized_frame() {
    let local = local_endpoint(HANDFAST_ID);
    let (listener, address) = bind(&local).await;
    let peer = peer_config(TlsVersion::Tls13, CertificateKind::Ecdsa);
    let dialer = tokio::spawn(async move { dial(address, HANDFAST_ID, &peer).await });

    let link = listener
        .accept(local.as_ref(), &TrustTable::default(), &limits(), &policy())
        .await
        .expect("the handshake completes before the bad frame");
    let mut peer = dialer
        .await
        .expect("the dialer task completes")
        .expect("the dial succeeds");
    peer.send_oversized_frame_header((MAX_FRAME_SIZE + 1) as u32)
        .await
        .expect("write the oversized header");

    let (_, mut reader, _, _permit) = link.into_parts();
    let error = match reader.recv().await {
        Ok(_) => panic!("an oversized frame must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(error, NetError::Proto(ProtoError::FrameTooLarge { .. })),
        "{error}"
    );
}
