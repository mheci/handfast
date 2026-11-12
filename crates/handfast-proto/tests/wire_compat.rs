//! Wire compatibility against real Android and desktop packets.

use handfast_proto::{
    DeviceId, DeviceName, DeviceType, DiscoveryAnnouncement, Identity, IdentityHello, Packet,
    MIN_TCP_PORT, PROTOCOL_VERSION,
};

const ANDROID_CONNECTION: &str = include_str!("fixtures/android_connection.json");
const DESKTOP_CONNECTION: &str = include_str!("fixtures/desktop_connection.json");
const DESKTOP_UDP: &str = include_str!("fixtures/desktop_udp_announcement.json");
const ANDROID_UDP: &str = include_str!("fixtures/android_udp_announcement.json");

const ANDROID_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
const DESKTOP_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

fn packet(fixture: &str) -> Packet {
    let packet: Packet = serde_json::from_str(fixture).expect("fixture is JSON");
    packet.validate().expect("fixture is a valid packet");
    packet
}

#[test]
fn hello_accepts_the_android_connection_fixture() {
    let packet = packet(ANDROID_CONNECTION);
    assert_eq!(packet.body["targetProtocolVersion"], serde_json::json!("8"));

    let hello = IdentityHello::from_packet(&packet).unwrap();
    assert_eq!(hello.identity.device_id.as_str(), ANDROID_ID);
    assert_eq!(hello.identity.device_name.as_str(), "Pixel 7");
    assert_eq!(hello.identity.device_type, DeviceType::Phone);
    assert_eq!(hello.identity.protocol_version, PROTOCOL_VERSION);
    assert_eq!(hello.target_device_id.as_str(), DESKTOP_ID);
    assert_eq!(hello.target_protocol_version, PROTOCOL_VERSION);
}

#[test]
fn hello_accepts_the_desktop_connection_fixture() {
    let packet = packet(DESKTOP_CONNECTION);
    assert_eq!(packet.body["targetProtocolVersion"], serde_json::json!(8));

    let hello = IdentityHello::from_packet(&packet).unwrap();
    assert_eq!(hello.identity.device_id.as_str(), DESKTOP_ID);
    assert_eq!(hello.identity.device_name.as_str(), "kde-desktop");
    assert_eq!(hello.identity.device_type, DeviceType::Unknown);
    assert!(hello.identity.capabilities.incoming().is_empty());
    assert!(hello.identity.capabilities.outgoing().is_empty());
    assert_eq!(hello.target_device_id.as_str(), ANDROID_ID);
    assert_eq!(hello.target_protocol_version, PROTOCOL_VERSION);
}

#[test]
fn hello_round_trips_the_android_identity_with_a_numeric_target() {
    let hello = IdentityHello::from_packet(&packet(ANDROID_CONNECTION)).unwrap();
    let sent = hello.to_packet().unwrap();
    assert_eq!(sent.body["targetProtocolVersion"], serde_json::json!(8));
    assert!(sent.body["targetProtocolVersion"].is_number());
    assert_eq!(IdentityHello::from_packet(&sent).unwrap(), hello);
}

#[test]
fn discovery_accepts_the_desktop_udp_fixture() {
    let announcement = DiscoveryAnnouncement::from_packet(&packet(DESKTOP_UDP)).unwrap();
    assert_eq!(announcement.device_id.as_str(), DESKTOP_ID);
    assert_eq!(announcement.device_name.as_str(), "kde-desktop");
    assert_eq!(announcement.device_type, DeviceType::Unknown);
    assert_eq!(announcement.protocol_version, PROTOCOL_VERSION);
    assert_eq!(announcement.tcp_port, MIN_TCP_PORT);
}

#[test]
fn discovery_accepts_the_android_udp_fixture() {
    let announcement = DiscoveryAnnouncement::from_packet(&packet(ANDROID_UDP)).unwrap();
    assert_eq!(announcement.device_id.as_str(), ANDROID_ID);
    assert_eq!(announcement.device_name.as_str(), "Pixel 7");
    assert_eq!(announcement.device_type, DeviceType::Phone);
    assert_eq!(announcement.tcp_port, 1717);
}

#[test]
fn discovery_emits_the_minimal_desktop_shape() {
    let announcement = DiscoveryAnnouncement::new(
        DeviceId::new(DESKTOP_ID).unwrap(),
        DeviceName::new("kde-desktop"),
        DeviceType::Desktop,
        1739,
    );
    let packet = announcement.to_packet().unwrap();
    let keys: std::collections::BTreeSet<&str> = packet.body.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["deviceId", "deviceName", "protocolVersion", "tcpPort"]
            .into_iter()
            .collect()
    );
    assert_eq!(packet.body["tcpPort"], serde_json::json!(1739));

    let decoded = DiscoveryAnnouncement::from_packet(&packet).unwrap();
    assert_eq!(decoded.device_id, announcement.device_id);
    assert_eq!(decoded.device_name, announcement.device_name);
    assert_eq!(decoded.device_type, DeviceType::Unknown);
    assert_eq!(decoded.protocol_version, announcement.protocol_version);
    assert_eq!(decoded.tcp_port, announcement.tcp_port);
}

#[test]
fn strict_identity_rejects_the_desktop_discovery_packets() {
    for (name, fixture) in [
        ("desktop udp", DESKTOP_UDP),
        ("desktop connection", DESKTOP_CONNECTION),
    ] {
        assert!(
            Identity::from_packet(&packet(fixture)).is_err(),
            "{name} must stay rejected by the strict parser"
        );
    }
}
