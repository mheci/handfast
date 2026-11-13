#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = serde_json::from_slice::<handfast_proto::Packet>(data) {
        let _ = handfast_proto::DiscoveryAnnouncement::from_packet(&packet);
    }
});
