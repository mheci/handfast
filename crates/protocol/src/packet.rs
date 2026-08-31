//! Newline-delimited JSON packet framing ([`Packet`]).
//!
//! Every KDE Connect packet travels as one compact JSON object terminated by
//! a single `\n` (0x0A) byte — byte-for-byte the framing upstream
//! `NetworkPacket` uses: `serialize()` appends `'\n'` and link readers loop
//! on `canReadLine()`/`readLine()`. There is **no length prefix** on the
//! wire; the JSON object is self-delimiting.
//!
//! The JSON object has the upstream shape:
//!
//! ```json
//! {"id": 42, "type": "kdeconnect.ping", "body": {}}
//! ```
//!
//! and, for transfers, the optional top-level `payloadSize` /
//! `payloadTransferInfo` fields that announce a payload streamed over a
//! separate TLS connection (see [`crate::transfer`]).
//!
//! [`Packet::read_frame`] incrementally scans for the newline so memory never
//! grows past [`crate::MAX_PACKET_LEN`] even if a peer floods bytes without
//! ever sending the terminator.

use std::sync::atomic::{AtomicI64, Ordering};

use bytes::{BufMut, BytesMut};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

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
    /// Announced size of the payload streamed over the data connection, when
    /// this packet carries one. Negative values mean "unknown/endless", which
    /// upstream encodes as `payloadSize: -1`.
    #[serde(
        default,
        rename = "payloadSize",
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_size: Option<i64>,
    /// How to reach the data connection (upstream: `{"port": <u16>}`).
    /// Absent when the payload rides no separate connection.
    #[serde(
        default,
        rename = "payloadTransferInfo",
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_transfer_info: Option<serde_json::Map<String, Value>>,
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
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    /// Marks this packet as announcing a payload of `size` bytes streamed over
    /// a data connection listening on local port `port` (the receiver dials
    /// the sender's address on that port).
    pub fn with_payload(mut self, size: i64, port: u16) -> Self {
        self.payload_size = Some(size);
        let mut info = serde_json::Map::new();
        info.insert("port".to_string(), Value::from(port));
        self.payload_transfer_info = Some(info);
        self
    }

    /// Announced payload size, if this packet carries one (`< 0` = unknown).
    #[must_use]
    pub fn payload_size(&self) -> Option<i64> {
        self.payload_size
    }

    /// Whether this packet announces a payload to stream (size present and
    /// non-zero; upstream's `hasPayload()` semantics).
    #[must_use]
    pub fn has_payload(&self) -> bool {
        self.payload_size.is_some_and(|size| size != 0)
    }

    /// Port the peer listens on for the data connection, if announced.
    #[must_use]
    pub fn payload_transfer_port(&self) -> Option<u16> {
        self.payload_transfer_info
            .as_ref()
            .and_then(|info| info.get("port"))
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
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
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    /// Encodes the packet into `buf` as a frame: the UTF-8 JSON serialization
    /// of the whole packet object followed by a single `\n` terminator.
    pub fn encode_into(&self, buf: &mut BytesMut) -> Result<()> {
        let json = serde_json::to_vec(self)?;
        if json.len() > crate::MAX_PACKET_LEN {
            return Err(Error::Other(format!(
                "packet of {} bytes exceeds MAX_PACKET_LEN ({})",
                json.len(),
                crate::MAX_PACKET_LEN
            )));
        }
        tracing::trace!(ptype = %self.ptype, id = self.id, len = json.len(), "encoding packet");
        buf.reserve(json.len() + 1);
        buf.extend_from_slice(&json);
        buf.put_u8(b'\n');
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
    /// *without* the trailing newline.
    ///
    /// Returns [`Error::Other`] when a frame exceeds
    /// [`crate::MAX_PACKET_LEN`]; the scan is incremental so no oversized
    /// buffer is ever allocated. An EOF before any byte is an error; an EOF
    /// mid-line is treated as a truncated frame.
    pub async fn read_frame<R>(r: &mut R) -> Result<BytesMut>
    where
        R: AsyncBufRead + Unpin,
    {
        let mut line = BytesMut::with_capacity(256);
        loop {
            let chunk = r.fill_buf().await?;
            if chunk.is_empty() {
                // EOF. A clean close after a complete frame is the caller's
                // concern; arriving here means we still owed a frame.
                if line.is_empty() {
                    return Err(Error::Other("connection closed before a frame".into()));
                }
                return Err(Error::Other(
                    "truncated frame: connection closed mid-line".into(),
                ));
            }
            if let Some(pos) = chunk.iter().position(|byte| *byte == b'\n') {
                if line.len() + pos > crate::MAX_PACKET_LEN {
                    return Err(Error::Other(format!(
                        "framed packet of {} bytes exceeds MAX_PACKET_LEN ({})",
                        line.len() + pos,
                        crate::MAX_PACKET_LEN
                    )));
                }
                line.extend_from_slice(&chunk[..pos]);
                r.consume(pos + 1); // swallow the newline too
                return Ok(line);
            }
            if line.len() + chunk.len() > crate::MAX_PACKET_LEN {
                return Err(Error::Other(format!(
                    "framed packet exceeds MAX_PACKET_LEN ({})",
                    crate::MAX_PACKET_LEN
                )));
            }
            line.extend_from_slice(chunk);
            let consumed = chunk.len();
            r.consume(consumed);
        }
    }

    /// Reads and parses one packet, guarding against frames longer than
    /// [`crate::MAX_PACKET_LEN`]. Works with incrementally delivered data;
    /// callers keep a single buffered reader so over-read bytes (including
    /// any that follow the newline) are never lost.
    pub async fn read_from<R>(r: &mut R) -> Result<Self>
    where
        R: AsyncBufRead + Unpin,
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
        // The id source is a process-wide atomic, so other test threads can
        // consume values between our two calls; only strict monotonicity is
        // guaranteed, not a gap of exactly one.
        assert!(
            second.id > first.id,
            "{} should be > {}",
            second.id,
            first.id
        );
    }

    #[test]
    fn encode_uses_newline_framing() {
        let packet = Packet::new(crate::TYPE_PING, serde_json::json!({ "v": 1 }));
        let mut buf = BytesMut::new();
        packet.encode_into(&mut buf).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'), "frame must end with a newline");
        assert_eq!(&buf[..buf.len() - 1], serde_json::to_vec(&packet).unwrap());
        assert!(!buf.windows(2).any(|w| w == *b"\n\n"));
    }

    #[test]
    fn payload_envelope_round_trips() {
        let packet = Packet::new(
            crate::TYPE_SHARE,
            serde_json::json!({ "filename": "a.bin" }),
        )
        .with_payload(42, 1745);
        let json = serde_json::to_string(&packet).unwrap();
        let decoded: Packet = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, packet);
        assert_eq!(decoded.payload_size(), Some(42));
        assert_eq!(decoded.payload_transfer_port(), Some(1745));
        assert!(decoded.has_payload());

        // Absent fields must round-trip as None (compat with plain packets).
        let plain = Packet::new(crate::TYPE_PING, serde_json::Value::Null);
        let json = serde_json::to_string(&plain).unwrap();
        let decoded: Packet = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, plain);
        assert_eq!(decoded.payload_size(), None);
        assert_eq!(decoded.payload_transfer_port(), None);
        assert!(!decoded.has_payload());
    }

    #[test]
    fn zero_size_payload_is_not_a_payload() {
        // Upstream treats payloadSize == 0 as "no payload" (empty file).
        let packet = Packet::new(
            crate::TYPE_SHARE,
            serde_json::json!({ "filename": "empty" }),
        )
        .with_payload(0, 1745);
        assert!(!packet.has_payload());
        assert_eq!(packet.payload_size(), Some(0));
    }
}
