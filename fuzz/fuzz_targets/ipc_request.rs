//! Fuzz target: IPC request-frame decoding.
//!
//! Feeds arbitrary bytes through the length-delimited IPC codec used by every
//! local control-plane client. Must reject oversized frames before buffering
//! and never panic on malformed payloads.
#![no_main]

use libfuzzer_sys::fuzz_target;

use std::io::Cursor;

use tokio::io::AsyncReadExt;

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .ok();
    let Some(runtime) = runtime else { return };
    runtime.block_on(async move {
        let mut cursor = Cursor::new(data.to_vec());
        let _ = handfast_ipc::codec::read_frame::<_, handfast_ipc::Request>(&mut cursor).await;

        let mut cursor2 = Cursor::new(data.to_vec());
        let _ =
            handfast_ipc::codec::read_frame::<_, handfast_ipc::ServerEvent>(&mut cursor2).await;
    });
});