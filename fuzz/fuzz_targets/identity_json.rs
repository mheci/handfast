//! Fuzz target: identity packet JSON parsing.
//!
//! Arbitrary JSON must parse (or fail cleanly) into [`Identity`] and survive a
//! re-serialization round trip without panicking.

#![no_main]

use libfuzzer_sys::fuzz_data;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(identity) = serde_json::from_str::<handfast_protocol::Identity>(text) {
        let _ = identity.supports_incoming("kdeconnect.ping");
        let _ = identity.supports_outgoing("kdeconnect.battery");
        // Round trip: reserialize and parse again; result must be identical.
        if let Ok(bytes) = serde_json::to_vec(&identity) {
            let reparsed = serde_json::from_slice::<handfast_protocol::Identity>(&bytes);
            assert_eq!(reparsed.ok().as_ref(), Some(&identity));
        }
    }
});
