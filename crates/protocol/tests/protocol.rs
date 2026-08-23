#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Integration tests: identity fixture parsing, frame round-trips,
//! split-buffer reads, oversize rejection and property-based round-trips.

use std::io::Cursor;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use proptest::prelude::*;
use tokio::io::{AsyncRead, ReadBuf};

use handfast_protocol::{
    error::Error, Identity, Packet, MAX_PACKET_LEN, PROTO_VERSION, TYPE_BATTERY,
    TYPE_BATTERY_REQUEST, TYPE_CLIPBOARD, TYPE_CLIPBOARD_CONNECT, TYPE_FINDMYPHONE, TYPE_IDENTITY,
    TYPE_MOUSEPAD, TYPE_MPRIS, TYPE_MPRIS_REQUEST, TYPE_NOTIFICATION, TYPE_NOTIFICATION_REQUEST,
    TYPE_PAIR, TYPE_PING, TYPE_RUNCOMMAND, TYPE_RUNCOMMAND_REQUEST, TYPE_SFTP, TYPE_SHARE,
    TYPE_SYSTEMVOLUME, TYPE_TELEPHONY,
};

const FIXTURE_PACKET_JSON: &str = include_str!("fixtures/identity_phone.json");

fn sample_identity() -> Identity {
    Identity {
        device_id: "1111222233334444555566667777888899990000".to_string(),
        name: "Handfast Desktop".to_string(),
        device_type: "desktop".to_string(),
        protocol_version: PROTO_VERSION,
        incoming: vec![TYPE_NOTIFICATION.to_string(), TYPE_SHARE.to_string()],
        outgoing: vec![TYPE_PING.to_string(), TYPE_NOTIFICATION_REQUEST.to_string()],
        tcp_source_port: 1716,
    }
}

#[test]
fn fixture_parses_into_identity() {
    let packet: Packet = serde_json::from_str(FIXTURE_PACKET_JSON).unwrap();
    assert_eq!(packet.ptype, TYPE_IDENTITY);

    let ident: Identity = serde_json::from_value(packet.body).unwrap();
    assert_eq!(ident.device_id.len(), 40);
    assert!(ident.device_id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(ident.name, "Pixel 9");
    assert_eq!(ident.device_type, "phone");
    assert_eq!(ident.protocol_version, PROTO_VERSION);
    assert_eq!(ident.tcp_source_port, 17431);

    assert!(ident.supports_incoming(TYPE_NOTIFICATION_REQUEST));
    assert!(ident.supports_incoming(TYPE_MOUSEPAD));
    assert!(ident.supports_outgoing(TYPE_TELEPHONY));
    assert!(ident.supports_outgoing(TYPE_SFTP));
    assert!(!ident.supports_incoming(TYPE_TELEPHONY));
    assert!(!ident.supports_outgoing(TYPE_FINDMYPHONE));
}

#[test]
fn fixture_reserializes_semantically() {
    let original: Packet = serde_json::from_str(FIXTURE_PACKET_JSON).unwrap();
    let reserialized = serde_json::to_string(&original).unwrap();
    let reparsed: Packet = serde_json::from_str(&reserialized).unwrap();
    assert_eq!(original, reparsed);

    let ident: Identity = serde_json::from_value(original.body).unwrap();
    let ident_again: Identity =
        serde_json::from_str(&serde_json::to_string(&ident).unwrap()).unwrap();
    assert_eq!(ident, ident_again);
}

#[test]
fn identity_helper_wraps_identity_body() {
    let sample = sample_identity();
    let packet = Packet::identity(sample.clone());
    assert_eq!(packet.ptype, TYPE_IDENTITY);
    let decoded: Identity = serde_json::from_value(packet.body).unwrap();
    assert_eq!(decoded, sample);
}

#[tokio::test]
async fn framed_write_read_preserves_order_and_values() {
    let first = Packet::new(TYPE_PING, serde_json::json!({ "nonce": 7 }));
    let second = Packet::identity(sample_identity());

    let mut wire = Vec::new();
    first.write_to(&mut wire).await.unwrap();
    second.write_to(&mut wire).await.unwrap();

    let mut reader = Cursor::new(wire);
    let got_first = Packet::read_from(&mut reader).await.unwrap();
    let got_second = Packet::read_from(&mut reader).await.unwrap();

    assert_eq!(got_first.id, first.id);
    assert_eq!(got_first.ptype, TYPE_PING);
    assert_eq!(got_first.body, serde_json::json!({ "nonce": 7 }));
    assert_eq!(got_second.ptype, TYPE_IDENTITY);
    let ident: Identity = serde_json::from_value(got_second.body).unwrap();
    assert_eq!(ident, sample_identity());
}

/// Reader handing out at most 3 bytes per poll to prove framing tolerates
/// arbitrarily split buffers.
struct Trickle<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> AsyncRead for Trickle<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let chunk = buf.remaining().min(3);
        let mut scratch = [0u8; 3];
        let mut sub = ReadBuf::new(&mut scratch[..chunk]);
        match Pin::new(&mut this.inner).poll_read(cx, &mut sub) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Ready(Ok(())) => {
                let filled = sub.filled().len();
                buf.put_slice(&scratch[..filled]);
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[tokio::test]
async fn reads_survive_split_buffers() {
    let first = Packet::new(TYPE_PING, serde_json::Value::Null);
    let second = Packet::identity(sample_identity());

    let mut wire = Vec::new();
    first.write_to(&mut wire).await.unwrap();
    second.write_to(&mut wire).await.unwrap();

    let mut reader = Trickle {
        inner: Cursor::new(wire),
    };
    let got_first = Packet::read_from(&mut reader).await.unwrap();
    let got_second = Packet::read_from(&mut reader).await.unwrap();

    assert_eq!(got_first, first);
    assert_eq!(got_second, second);
}

#[tokio::test]
async fn oversized_frames_are_rejected_unbuffered() {
    let too_long = (MAX_PACKET_LEN as u32) + 1;
    let mut wire = Vec::new();
    wire.extend_from_slice(&too_long.to_be_bytes());
    wire.extend_from_slice(b"\"trailing garbage\"");

    let mut reader = Cursor::new(wire);
    let err = Packet::read_from(&mut reader).await.unwrap_err();
    match err {
        Error::Other(message) => assert!(
            message.contains("exceeds"),
            "unexpected rejection message: {message}"
        ),
        other => panic!("expected oversized-frame rejection, got: {other:?}"),
    }
}

#[tokio::test]
async fn frames_exactly_at_max_len_are_accepted() {
    // Deterministic sizing: every 'A' contributes exactly one byte to the JSON
    // encoding, so measuring the zero-pad base and adding the remainder lands
    // precisely on the cap without search.
    let probe = Packet::new(TYPE_PING, serde_json::json!({ "pad": "" }));
    let base = serde_json::to_string(&probe).unwrap().len();
    assert!(
        base < MAX_PACKET_LEN,
        "base frame must leave room for padding"
    );
    let pad = MAX_PACKET_LEN - base;

    let mut packet = Packet::new(TYPE_PING, serde_json::json!({ "pad": "A".repeat(pad) }));
    // Pin the id: Packet::new increments globally, and a digit-width change
    // between probe and final would shift the encoded size by one byte.
    packet.id = probe.id;
    let payload = serde_json::to_vec(&packet).unwrap();
    assert_eq!(
        payload.len(),
        MAX_PACKET_LEN,
        "encoding must be linear in pad length"
    );

    let mut wire = Vec::new();
    wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    wire.extend_from_slice(&payload);

    let mut reader = Cursor::new(wire);
    let decoded = Packet::read_from(&mut reader).await.unwrap();
    assert_eq!(decoded, packet);
}

fn dedup_caps(caps: Vec<&'static str>) -> Vec<String> {
    let mut unique: Vec<String> = caps.into_iter().map(str::to_owned).collect();
    unique.sort();
    unique.dedup();
    unique
}

fn known_packet_types() -> Vec<&'static str> {
    vec![
        TYPE_PING,
        TYPE_PAIR,
        TYPE_NOTIFICATION,
        TYPE_NOTIFICATION_REQUEST,
        TYPE_MOUSEPAD,
        TYPE_MPRIS,
        TYPE_MPRIS_REQUEST,
        TYPE_CLIPBOARD,
        TYPE_CLIPBOARD_CONNECT,
        TYPE_SHARE,
        TYPE_RUNCOMMAND,
        TYPE_RUNCOMMAND_REQUEST,
        TYPE_BATTERY,
        TYPE_BATTERY_REQUEST,
        TYPE_SFTP,
        TYPE_TELEPHONY,
        TYPE_SYSTEMVOLUME,
        TYPE_FINDMYPHONE,
    ]
}

fn arb_identity() -> impl Strategy<Value = Identity> {
    (
        "[0-9a-f]{8,40}",
        "[a-zA-Z0-9 ._-]{0,48}",
        proptest::sample::select(vec!["phone", "desktop", "laptop", "tablet", "tv"]),
        proptest::arbitrary::any::<u16>(),
        proptest::collection::vec(proptest::sample::select(known_packet_types()), 0..8),
        proptest::collection::vec(proptest::sample::select(known_packet_types()), 0..8),
    )
        .prop_map(
            |(device_id, name, device_type, port, incoming, outgoing)| Identity {
                device_id,
                name,
                device_type: device_type.to_string(),
                protocol_version: PROTO_VERSION,
                incoming: dedup_caps(incoming),
                outgoing: dedup_caps(outgoing),
                tcp_source_port: port,
            },
        )
}

proptest! {
    #[test]
    fn arbitrary_identity_packets_roundtrip(ident in arb_identity()) {
        let packet = Packet::identity(ident);
        let mut buf = BytesMut::new();
        packet.encode_into(&mut buf).unwrap();
        let frame: Bytes = buf.freeze();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        prop_assert!(len <= MAX_PACKET_LEN);
        prop_assert_eq!(frame.len(), len + 4);
        let decoded: Packet = serde_json::from_slice(&frame[4..]).unwrap();
        prop_assert_eq!(&decoded, &packet);
    }
}
