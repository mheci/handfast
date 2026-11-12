//! Cross-checks between the independent peer and the production codec.

use std::path::PathBuf;

use handfast_testkit::frame::{decode as testkit_decode, encode as testkit_encode, WirePacket};
use handfast_testkit::peer::run_self_test;
use handfast_testkit::vectors::load_dir;
use serde_json::json;

fn vector_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

#[test]
fn production_codec_decodes_every_golden_vector() {
    let vectors = load_dir(vector_dir()).unwrap();
    assert!(vectors.len() >= 10);
    for vector in vectors {
        let packet: handfast_proto::Packet = serde_json::from_value(vector.packet.clone())
            .unwrap_or_else(|error| panic!("{}: {error}", vector.name));
        packet
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", vector.name));
        let frame = handfast_proto::frame::encode(&packet).unwrap();
        let decoded = handfast_proto::frame::decode(&frame).unwrap();
        assert_eq!(decoded, packet, "{}", vector.name);
    }
}

#[test]
fn independent_and_production_frames_agree() {
    let wire = WirePacket {
        id: 0,
        ptype: "kdeconnect.ping".to_string(),
        body: json!({"message": "cross-check"})
            .as_object()
            .cloned()
            .unwrap(),
        payload_size: None,
        payload_transfer_info: None,
    };

    let testkit_frame = testkit_encode(&wire).unwrap();
    let production = handfast_proto::frame::decode(&testkit_frame).unwrap();
    assert_eq!(production.ptype, "kdeconnect.ping");
    assert_eq!(production.body["message"], json!("cross-check"));

    let production_frame = handfast_proto::frame::encode(&production).unwrap();
    let back = testkit_decode(&production_frame).unwrap();
    assert_eq!(back.ptype, wire.ptype);
    assert_eq!(back.body, wire.body);
}

#[test]
fn production_codec_rejects_independent_length_errors() {
    let wire = WirePacket {
        id: 0,
        ptype: "kdeconnect.ping".to_string(),
        body: serde_json::Map::new(),
        payload_size: None,
        payload_transfer_info: None,
    };
    let mut frame = testkit_encode(&wire).unwrap();
    frame.push(0);
    assert!(handfast_proto::frame::decode(&frame).is_err());
}

#[tokio::test]
async fn rust_peer_self_test_passes() {
    let report = run_self_test().await.unwrap();
    assert!(report.paired);
    assert!(report.ping_received);
    assert_eq!(report.client_sent, 3);
    assert_eq!(report.server_sent, 2);
}
