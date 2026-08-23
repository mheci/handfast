//! Length-prefixed JSON packet framing ([`Packet`]).
//!
//! Every KDE Connect packet travels as a single frame:
//!
//! ```text
//! u32 (big-endian byte count) ++ UTF-8 JSON of the whole packet object
//! ```
//!
//! The JSON object has three fields:
//!
//! ```json
//! {"id": 42, "type": "kdeconnect.ping", "body": {}}
//! ```
//!
//! [`Packet::read_frame`] validates the length prefix against
//! [`crate::MAX_PACKET_LEN`] *before* reserving memory for the payload.

use std::sync::atomic::{AtomicI64, Ordering};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::TYPE_IDENTITY;

/// Source of monotonic [`Packet::id`] values within this process.
static NEXT_PACKET_ID: AtomicI64 = AtomicI64::new(0);

fn next_packet_id() -> i64 {
    NEXT_PACKET_ID
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

/// A single KDE Connect protocol packet.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Packet {
    /// Packet id unique within a connection; replies may reference it.
    pub id: i64,
    /// Packet type URI such as `kdeconnect.ping`.
    #[serde(rename = "type")]
    pub ptype: String,
    /// Type-specific payload object; `null` when a plugin sends none.
    #[serde(default)]
    pub body: serde_json::Value,
}

impl Packet {
    /// Convenience accessor for the packet type URI (`kdeconnect.*`).
    #[must_use]
    pub fn ty(&self) -> &str {
        &self.ptype
    }

    /// Creates a packet with a fresh process-wide monotonic id.
    pub fn new(ptype: &str, body: serde_json::Value) -> Self {
        Self {
            id: next_packet_id(),
            ptype: ptype.to_string(),
            body,
        }
    }

    /// Wraps an [`Identity`] into an outgoing [`crate::TYPE_IDENTITY`] packet.
    ///
    /// `Identity` serialization into a JSON object value cannot fail in
    /// practice; if it ever did, the body degrades to `null` with a warning
    /// rather than panicking.
    pub fn identity(body: Identity) -> Self {
        let value = match serde_json::to_value(&body) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(%err, "identity serialization failed; emitting null body");
                serde_json::Value::Null
            }
        };
        Self {
            id: next_packet_id(),
            ptype: TYPE_IDENTITY.to_string(),
            body: value,
        }
    }

    /// Encodes the packet into `buf` as a frame: a `u32` big-endian byte
    /// length prefix followed by the UTF-8 JSON serialization of the whole
    /// packet object.
    pub fn encode_into(&self, buf: &mut BytesMut) -> Result<()> {
        let payload = serde_json::to_vec(self)?;
        let len = u32::try_from(payload.len()).map_err(|_| {
            Error::Other(format!(
                "packet of {} bytes does not fit the u32 length prefix",
                payload.len()
            ))
        })?;
        tracing::trace!(ptype = %self.ptype, id = self.id, len = payload.len(), "encoding packet");
        buf.reserve(4 + payload.len());
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&payload);
        Ok(())
    }

    /// Encodes the packet and writes the whole frame to `w`.
    pub async fn write_to<W>(&self, w: &mut W) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let mut frame = BytesMut::new();
        self.encode_into(&mut frame)?;
        w.write_all(&frame).await?;
        w.flush().await?;
        Ok(())
    }

    /// Reads one raw frame from `r` and returns its payload: the JSON bytes
    /// *without* the 4-byte length prefix.
    ///
    /// Returns [`Error::Other`] when the declared length exceeds
    /// [`crate::MAX_PACKET_LEN`]; nothing is buffered in that case.
    pub async fn read_frame<R>(r: &mut R) -> Result<BytesMut>
    where
        R: AsyncRead + Unpin,
    {
        let mut prefix = [0u8; 4];
        r.read_exact(&mut prefix).await?;
        let len = u32::from_be_bytes(prefix) as usize;
        if len > crate::MAX_PACKET_LEN {
            tracing::warn!(
                len,
                max = crate::MAX_PACKET_LEN,
                "rejecting oversized frame"
            );
            return Err(Error::Other(format!(
                "framed packet of {len} bytes exceeds MAX_PACKET_LEN ({})",
                crate::MAX_PACKET_LEN
            )));
        }
        let mut payload = BytesMut::with_capacity(len);
        payload.resize(len, 0);
        r.read_exact(&mut payload[..]).await?;
        Ok(payload)
    }

    /// Reads and parses one packet, guarding against frames longer than
    /// [`crate::MAX_PACKET_LEN`]. Works with incrementally delivered data;
    /// callers never need to buffer whole frames themselves.
    pub async fn read_from<R>(r: &mut R) -> Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let payload = Self::read_frame(r).await?;
        let packet: Self = serde_json::from_slice(&payload)?;
        tracing::trace!(ptype = %packet.ptype, id = packet.id, len = payload.len(), "decoded packet");
        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_ids_are_monotonic() {
        let first = Packet::new(crate::TYPE_PING, serde_json::Value::Null);
        let second = Packet::new(crate::TYPE_PING, serde_json::Value::Null);
        assert_eq!(second.id, first.id + 1);
    }

    #[test]
    fn encode_writes_big_endian_length_prefix() {
        let packet = Packet::new(crate::TYPE_PING, serde_json::json!({ "v": 1 }));
        let mut buf = BytesMut::new();
        packet.encode_into(&mut buf).unwrap();
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(buf.len(), len + 4);
        assert_eq!(&buf[4..], serde_json::to_vec(&packet).unwrap());
    }
}
