//! Fuzz target: framed KDE Connect packet parsing.
//!
//! Feeds arbitrary bytes through the length-prefixed framing reader used on
//! every network connection. Must never panic, hang (length guards enforce a
//! 512 KiB cap before buffering) or allocate unbounded memory.

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_data;
use tokio::io::AsyncReadExt;

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .ok();
    let Some(runtime) = runtime else { return };
    runtime.block_on(async move {
        let mut cursor = Cursor::new(data.to_vec());
        // Errors are expected for almost all inputs; panics/hangs are bugs.
        let _ = handfast_protocol::Packet::read_from(&mut cursor).await;

        // Also exercise the raw-frame path with an independent cursor.
        let mut cursor2 = Cursor::new(data.to_vec());
        let _ = handfast_protocol::Packet::read_frame(&mut cursor2).await;
        drop(AsyncReadExt::by_ref(&mut cursor2));
    });
});
