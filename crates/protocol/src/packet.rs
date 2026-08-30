//! Newline-delimited JSON packet framing ([`Packet`]).
//!
//! Every KDE Connect packet travels as a single line of compact JSON
//! terminated by `\n` (both kdeconnect-kde and the Android app do exactly
//! this — see `NetworkPacket::serialize()` upstream):
//!
//! ```json
//! {"id":42,"type":"kdeconnect.ping","body":{}}
//! ```
//!
//! Payload-bearing packets (file transfers) additionally carry top-level
//! `payloadSize` and `payloadTransferInfo` fields; the payload bytes
//! themselves stream over a *separate* TLS connection whose endpoint the
//! receiver learns from `payloadTransferInfo` (see
//! [`crate::PAYLOAD_TRANSFER_MIN_PORT`]).
//!
//! [`Packet::read_frame`] enforces [`crate::MAX_PACKET_LEN`] *before* growing
//! the frame buffer past the cap.

use std::sync::atomic::{AtomicI64, Ordering};

use bytes::BytesMut;
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
    /// Total payload bytes announced for a transfer (`payloadSize`); absent
    /// on ordinary packets.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "payloadSize"
    )]
    pub payload_size: Option<i64>,
    /// Sender-side endpoint the payload must be fetched from
    /// (`payloadTransferInfo`, e.g. `{"port": 1740}`); absent unless the
    /// packet carries a payload.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "payloadTransferInfo"
    )]
    pub payload_transfer_info: Option<serde_json::Value>,
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

    /// Marks this packet as announcing a payload of `size` bytes, to be
    /// fetched from TCP `port` on the *sender's* address.
    ///
    /// The receiver connects back to that port, wraps the socket in TLS (it
    /// is the TLS client; the sender is the TLS server), and reads exactly
    /// `size` raw bytes — matching upstream KDE Connect behavior.
    #[must_use]
    pub fn with_payload(mut self, size: u64, port: u16) -> Self {
        self.payload_size = Some(i64::try_from(size).unwrap_or(i64::MAX));
        self.payload_transfer_info = Some(serde_json::json!({ "port": port }));
        self
    }

    /// Whether this packet announces a payload channel.
    #[must_use]
    pub fn has_payload(&self) -> bool {
        self.payload_size.is_some()
    }

    /// The `port` the payload must be fetched from, when announced.
    #[must_use]
    pub fn payload_port(&self) -> Option<u16> {
        let info = self.payload_transfer_info.as_ref()?;
        let port = info.get("port")?.as_u64()?;
        u16::try_from(port).ok()
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

    /// Encodes the packet into `buf` as a frame: the compact UTF-8 JSON
    /// serialization of the whole packet object followed by `\n`.
    ///
    /// The trailing newline is the KDE Connect frame delimiter; JSON escapes
    /// any literal newline inside strings, so a raw `\n` never appears within
    /// a frame body.
    pub fn encode_into(&self, buf: &mut BytesMut) -> Result<()> {
        let mut payload = serde_json::to_vec(self)?;
        payload.push(b'\n');
        tracing::trace!(ptype = %self.ptype, id = self.id, len = payload.len(), "encoding packet");
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
    /// *without* the terminating `\n`.
    ///
    /// Reading stops at the first `\n`; bytes after it stay buffered for the
    /// next call (callers should pass a buffered reader). Returns
    /// [`Error::Other`] once the accumulated frame exceeds
    /// [`crate::MAX_PACKET_LEN`].
    pub async fn read_frame<R>(r: &mut R) -> Result<BytesMut>
    where
        R: AsyncBufRead + Unpin,
    {
        let mut frame = BytesMut::with_capacity(256);
        loop {
            let buf = r.fill_buf().await?;
            if buf.is_empty() {
                // EOF. A clean end-of-stream yields an empty frame; a partial
                // line means the peer died mid-packet.
                if frame.is_empty() {
                    return Ok(frame);
                }
                return Err(Error::Other("connection closed mid-frame".into()));
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(n) => {
                    frame.extend_from_slice(&buf[..n]);
                    r.consume(n + 1);
                    if frame.len() > crate::MAX_PACKET_LEN {
                        tracing::warn!(
                            len = frame.len(),
                            max = crate::MAX_PACKET_LEN,
                            "rejecting oversized frame"
                        );
                        return Err(Error::Other(format!(
                            "frame exceeds MAX_PACKET_LEN ({} bytes)",
                            crate::MAX_PACKET_LEN
                        )));
                    }
                    return Ok(frame);
                }
                None => {
                    frame.extend_from_slice(buf);
                    let consumed = buf.len();
                    r.consume(consumed);
                    if frame.len() > crate::MAX_PACKET_LEN {
                        tracing::warn!(
                            len = frame.len(),
                            max = crate::MAX_PACKET_LEN,
                            "rejecting oversized frame"
                        );
                        return Err(Error::Other(format!(
                            "frame exceeds MAX_PACKET_LEN ({} bytes)",
                            crate::MAX_PACKET_LEN
                        )));
                    }
                }
            }
        }
    }

    /// Reads and parses one packet, guarding against frames longer than
    /// [`crate::MAX_PACKET_LEN`]. Works with incrementally delivered data;
    /// callers never need to buffer whole frames themselves.
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
        assert_eq!(second.id, first.id + 1);
    }

    #[test]
    fn encode_terminates_with_newline() {
        let packet = Packet::new(crate::TYPE_PING, serde_json::json!({ "v": 1 }));
        let mut buf = BytesMut::new();
        packet.encode_into(&mut buf).unwrap();
        assert!(buf.ends_with(b"\n"), "frame must be newline-terminated");
        let parsed: serde_json::Value = serde_json::from_slice(&buf[..buf.len() - 1]).unwrap();
        assert_eq!(parsed["type"], "kdeconnect.ping");
        assert_eq!(parsed["id"], packet.id);
    }

    #[test]
    fn payload_metadata_serializes_as_top_level_keys() {
        let packet = Packet::new(
            crate::TYPE_SHARE,
            serde_json::json!({ "filename": "photo.jpg" }),
        )
        .with_payload(1_048_576, 1743);
        let mut buf = BytesMut::new();
        packet.encode_into(&mut buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&buf[..buf.len() - 1]).unwrap();
        assert_eq!(parsed["payloadSize"], 1_048_576);
        assert_eq!(parsed["payloadTransferInfo"]["port"], 1743);
        assert!(parsed["body"]["filename"].is_string());
    }

    #[tokio::test]
    async fn newline_framing_round_trips_through_buffered_reader() {
        use tokio::io::BufReader;

        let first = Packet::new(crate::TYPE_PING, serde_json::json!({ "n": 1 }));
        let second = Packet::new(
            crate::TYPE_SHARE,
            serde_json::json!({ "text": "multi\nline" }),
        )
        .with_payload(4096, 1745);
        let mut frame = BytesMut::new();
        first.encode_into(&mut frame).unwrap();
        second.encode_into(&mut frame).unwrap();

        // Feed everything in one write; the reader must still split frames on
        // newline boundaries and keep the embedded escaped newline intact.
        let mut reader = BufReader::new(&frame[..]);
        let got_first = Packet::read_from(&mut reader).await.unwrap();
        let got_second = Packet::read_from(&mut reader).await.unwrap();
        assert_eq!(got_first, first);
        assert_eq!(got_second, second);
        assert_eq!(got_second.body["text"], "multi\nline");
        assert_eq!(got_second.payload_port(), Some(1745));
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        use tokio::io::BufReader;

        let mut big = vec![b'a'; crate::MAX_PACKET_LEN + 1];
        big.push(b'\n');
        let mut reader = BufReader::new(&big[..]);
        let err = Packet::read_from(&mut reader).await.unwrap_err();
        assert!(
            err.to_string().contains("MAX_PACKET_LEN"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn payload_helpers_agree_with_fields() {
        let packet = Packet::new(crate::TYPE_PING, serde_json::Value::Null);
        assert!(!packet.has_payload());
        assert_eq!(packet.payload_port(), None);

        let packet = packet.with_payload(0, 1739);
        assert!(packet.has_payload());
        assert_eq!(packet.payload_port(), Some(1739));
        assert_eq!(packet.payload_size, Some(0));
    }
}
