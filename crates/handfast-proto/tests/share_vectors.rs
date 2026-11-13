//! Golden share vectors shared with the independent testkit peer.

use std::path::{Path, PathBuf};

use handfast_proto::{Packet, PayloadOffer, PayloadSize, ShareBatch, ShareFile, ShareRequest};

fn vector_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../handfast-testkit/tests/vectors")
}

fn load(name: &str) -> Packet {
    let path = vector_dir().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let packet: Packet = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    packet
        .validate()
        .unwrap_or_else(|error| panic!("validate {}: {error}", path.display()));
    packet
}

#[test]
fn share_file_vector_parses() {
    let packet = load("share_file.json");
    assert_eq!(
        ShareRequest::from_packet(&packet).unwrap(),
        ShareRequest::File(ShareFile {
            filename: Some("image.png".to_string()),
            size: PayloadSize::Known(882),
            created_ms: None,
            modified_ms: None,
            open: false,
            batch: None,
        })
    );
    assert_eq!(
        packet.payload_offer().unwrap(),
        Some(PayloadOffer {
            size: PayloadSize::Known(882),
            port: 1739,
        })
    );
}

#[test]
fn share_text_vector_parses() {
    let packet = load("share_text.json");
    assert_eq!(
        ShareRequest::from_packet(&packet).unwrap(),
        ShareRequest::Text("Some text".to_string())
    );
    assert_eq!(packet.payload_offer().unwrap(), None);
}

#[test]
fn share_url_vector_parses() {
    let packet = load("share_url.json");
    assert_eq!(
        ShareRequest::from_packet(&packet).unwrap(),
        ShareRequest::Url("https://example.com/report".to_string())
    );
    assert_eq!(packet.payload_offer().unwrap(), None);
}

#[test]
fn share_file_empty_vector_parses() {
    let packet = load("share_file_empty.json");
    assert_eq!(
        ShareRequest::from_packet(&packet).unwrap(),
        ShareRequest::File(ShareFile {
            filename: Some("empty.txt".to_string()),
            size: PayloadSize::Known(0),
            created_ms: None,
            modified_ms: None,
            open: false,
            batch: None,
        })
    );
    assert_eq!(packet.payload_offer().unwrap(), None);
}

#[test]
fn share_update_vector_parses() {
    let packet = load("share_update.json");
    assert_eq!(
        ShareRequest::update_from_packet(&packet).unwrap(),
        ShareBatch {
            number_of_files: 2,
            total_payload_size: 1_048_576,
        }
    );
}
