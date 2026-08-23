#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Criterion benchmarks for identity encoding, packet decoding and framed
//! round-trips over the bundled upstream-identity fixture.

use std::hint::black_box;

use bytes::BytesMut;
use criterion::{criterion_group, criterion_main, Criterion};

use handfast_protocol::{Identity, Packet};

const FIXTURE_PACKET_JSON: &str = include_str!("../tests/fixtures/identity_phone.json");

fn fixture_packet() -> Packet {
    serde_json::from_str(FIXTURE_PACKET_JSON).expect("bundled identity fixture must parse")
}

fn bench_identity_encode(c: &mut Criterion) {
    let packet = fixture_packet();
    let identity: Identity =
        serde_json::from_value(packet.body.clone()).expect("fixture body is an identity");
    c.bench_function("identity_encode", |b| {
        b.iter(|| {
            serde_json::to_vec(black_box(&identity)).expect("identity serialization cannot fail")
        })
    });
}

fn bench_packet_decode(c: &mut Criterion) {
    let raw = serde_json::to_vec(&fixture_packet()).expect("packet serialization cannot fail");
    c.bench_function("packet_decode", |b| {
        b.iter(|| {
            let decoded: Packet =
                serde_json::from_slice(black_box(raw.as_slice())).expect("wire JSON decodes");
            decoded
        })
    });
}

fn bench_framed_roundtrip(c: &mut Criterion) {
    let packet = fixture_packet();
    c.bench_function("framed_roundtrip", |b| {
        b.iter(|| {
            let mut buf = BytesMut::new();
            packet.encode_into(&mut buf).expect("frame encode");
            let frame = buf.freeze();
            let len = u32::from_be_bytes(frame[..4].try_into().expect("prefix present")) as usize;
            serde_json::from_slice::<Packet>(&frame[4..4 + len]).expect("frame decode")
        })
    });
}

criterion_group!(
    benches,
    bench_identity_encode,
    bench_packet_decode,
    bench_framed_roundtrip
);
criterion_main!(benches);
