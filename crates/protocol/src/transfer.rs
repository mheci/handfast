//! File-transfer payload types.
//!
//! Control-plane metadata ([`TransferMeta`], [`ChunkHeader`]) travels inside
//! framed JSON packets (e.g. [`crate::TYPE_SHARE`]); the file bytes themselves
//! stream over the separate raw-TLS transfer channel in fixed
//! [`CHUNK_SIZE`] blocks. [`split_into_chunks`] and [`reassemble`] implement
//! that blocking on the sending and receiving sides respectively.

/// Fixed payload size of one transfer chunk (64 KiB).
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Metadata describing a single file transfer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TransferMeta {
    /// Unique id correlating the control packets with the streamed chunks.
    pub transfer_id: String,
    /// Peer device id this transfer belongs to.
    pub device_id: String,
    /// Name of the file being transferred (no path components).
    pub file_name: String,
    /// Total size of the file in bytes.
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

/// Prefix sent ahead of every chunk's bytes on the transfer stream.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ChunkHeader {
    /// Id of the transfer this chunk belongs to.
    pub transfer_id: String,
    /// Zero-based position of this chunk within the transfer.
    pub chunk_index: u32,
    /// Total number of chunks the transfer is split into.
    pub total_chunks: u32,
}

/// Splits `data` into consecutive chunks of at most [`CHUNK_SIZE`] bytes.
///
/// Returns an empty vector when `data` is empty; otherwise every chunk except
/// possibly the last is exactly [`CHUNK_SIZE`] bytes.
#[must_use]
pub fn split_into_chunks(data: &[u8]) -> Vec<Vec<u8>> {
    data.chunks(CHUNK_SIZE).map(<[u8]>::to_vec).collect()
}

/// Concatenates `chunks` back into a single byte vector.
#[must_use]
pub fn reassemble(chunks: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_split_reassemble() {
        let data: Vec<u8> = (0..=255u8).cycle().take(CHUNK_SIZE * 2 + 12345).collect();
        let chunks = split_into_chunks(&data);
        assert_eq!(chunks.len(), 3);
        assert_eq!(reassemble(&chunks), data);
    }

    #[test]
    fn exact_size_boundary_yields_single_full_chunk() {
        let data = vec![7u8; CHUNK_SIZE];
        let chunks = split_into_chunks(&data);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), CHUNK_SIZE);

        let one_over = vec![9u8; CHUNK_SIZE + 1];
        let chunks = split_into_chunks(&one_over);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), CHUNK_SIZE);
        assert_eq!(chunks[1].len(), 1);
        assert_eq!(reassemble(&chunks), one_over);
    }

    #[test]
    fn empty_input_produces_no_chunks() {
        let chunks = split_into_chunks(&[]);
        assert!(chunks.is_empty());
        assert!(reassemble(&chunks).is_empty());
    }
}
