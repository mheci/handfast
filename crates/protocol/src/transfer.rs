//! File-transfer payload plumbing.
//!
//! Matches upstream KDE Connect byte-for-byte. A `kdeconnect.share.request`
//! header packet announces a file over the control connection with the
//! top-level `payloadSize` and `payloadTransferInfo: {"port": <u16>}` fields
//! (see [`crate::Packet`]); the file bytes then stream **raw** — no chunk
//! headers, no base64 — over a second TLS connection that the *receiver*
//! dials to the sender's announced port. Upstream's `UploadJob` writes 4 KiB
//! at a time and `CompositeUploadJob` opens the data port in
//! [`PAYLOAD_PORT_MIN`]..=[`PAYLOAD_PORT_MAX`].
//!
//! [`TransferMeta`] and [`TransferDirection`] are the control-plane metadata
//! the daemon's receive engine tracks for staging, collision handling and
//! progress reporting.

/// Upper bound on one control-packet body chunk handled by the receive engine
/// (64 KiB). Payload *streams* on the data connection are unbounded; this only
/// caps how much of them we hand to the engine at a time.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Read/write granularity for payload streams (upstream's `UploadJob` buffer).
/// The wire carries no chunk framing, so the exact value is not part of the
/// protocol — this exists to mirror upstream behaviour and to size buffers.
pub const PAYLOAD_CHUNK_SIZE: usize = 4096;

/// First port the sender tries for the data connection (upstream
/// `UploadJob::MIN_PORT`).
pub const PAYLOAD_PORT_MIN: u16 = 1739;

/// Last port the sender tries for the data connection (upstream
/// `UploadJob::MAX_PORT`).
pub const PAYLOAD_PORT_MAX: u16 = 1764;

/// How long a sender waits for the receiver to dial the data connection
/// (upstream Android uses a 10 s `ServerSocket` accept window).
pub const PAYLOAD_ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a receiver waits for the next payload bytes before aborting
/// (generous; upstream relies on TCP timeouts alone, we want a hard bound so
/// a stalled peer cannot pin a staging file forever).
pub const PAYLOAD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Metadata describing a single file transfer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TransferMeta {
    /// Unique id correlating the control packets with the streamed payload.
    pub transfer_id: String,
    /// Peer device id this transfer belongs to.
    pub device_id: String,
    /// Name of the file being transferred (no path components).
    pub file_name: String,
    /// Total size of the file in bytes. `u64::MAX` means "unknown" (upstream
    /// `payloadSize: -1`); the engine then skips size validation.
    pub file_size: u64,
}

/// Whether a transfer moves data towards or away from the peer.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum TransferDirection {
    /// We are sending the file to the peer.
    Upload,
    /// The peer is sending the file to us.
    Download,
}

/// Sentinel for "the sender did not announce a size" (upstream `-1`).
pub const UNKNOWN_SIZE: u64 = u64::MAX;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_range_is_upstreams() {
        assert_eq!(PAYLOAD_PORT_MIN, 1739);
        assert_eq!(PAYLOAD_PORT_MAX, 1764);
        assert_eq!(PAYLOAD_CHUNK_SIZE, 4096);
        assert_eq!(CHUNK_SIZE, 64 * 1024);
    }

    #[test]
    fn meta_round_trips() {
        let meta = TransferMeta {
            transfer_id: "t1".into(),
            device_id: "dev".into(),
            file_name: "photo.jpg".into(),
            file_size: 1024,
        };
        let decoded: TransferMeta =
            serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();
        assert_eq!(decoded, meta);
    }
}
