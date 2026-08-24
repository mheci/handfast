//! Length-prefixed JSON frame codec.
//!
//! A frame is a little-endian `u32` byte count followed by exactly that many
//! bytes of UTF-8 JSON. [`write_frame`] serializes and sends a value;
//! [`read_frame`] reads one frame and decodes it, enforcing
//! [`MAX_FRAME_BYTES`] before buffering the payload so hostile peers cannot
//! make us allocate gigabytes from a single header.

use bytes::BytesMut;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::MAX_FRAME_BYTES;

/// Number of bytes in the frame length prefix.
const LENGTH_PREFIX: usize = 4;

/// Serialize `value` as JSON and send it as one framed message.
///
/// Fails with [`Error::FrameTooLarge`] if the encoded payload exceeds
/// [`MAX_FRAME_BYTES`]; the stream is left untouched in that case.
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    let len = payload.len();
    if len > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge {
            size: len,
            max: MAX_FRAME_BYTES,
        });
    }

    w.write_all(&(len as u32).to_le_bytes()).await?;
    w.write_all(&payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame's raw JSON payload.
///
/// Blocks until a complete frame is available. Returns [`Error::Closed`] on a
/// clean end-of-stream (including truncated frames) and
/// [`Error::FrameTooLarge`] when the declared length exceeds
/// [`MAX_FRAME_BYTES`].
///
/// One-shot convenience wrapper: bytes past the first frame are dropped.
/// Production loops that may receive pipelined frames (the daemon writes a
/// `Hello` event and a response back-to-back) must hold a [`FrameReader`]
/// instead, or coalesced frames are silently lost.
pub async fn read_raw_frame<R>(r: &mut R) -> Result<BytesMut>
where
    R: AsyncRead + Unpin,
{
    FrameReader::new().next(r).await
}

/// Stateful frame reader preserving bytes past the current frame.
///
/// Holds the partially-filled read buffer across calls so frames that arrive
/// coalesced in one socket read are queued, not discarded. One decoder per
/// connection/stream half.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: BytesMut,
}

impl FrameReader {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(1024),
        }
    }

    /// Read one frame's raw JSON payload, blocking until complete.
    ///
    /// Returns [`Error::Closed`] on a clean end-of-stream (including
    /// truncated frames) and [`Error::FrameTooLarge`] when the declared
    /// length exceeds [`MAX_FRAME_BYTES`]. Bytes past the current frame stay
    /// buffered and are returned by subsequent calls.
    pub async fn next<R>(&mut self, r: &mut R) -> Result<BytesMut>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            if self.buf.len() >= LENGTH_PREFIX {
                let declared =
                    u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                        as usize;
                if declared > MAX_FRAME_BYTES {
                    return Err(Error::FrameTooLarge {
                        size: declared,
                        max: MAX_FRAME_BYTES,
                    });
                }
                if self.buf.len() >= LENGTH_PREFIX + declared {
                    // Carve [prefix..frame] off the front; leftover pipelined
                    // bytes remain buffered for the next call.
                    let mut tail = self.buf.split_off(LENGTH_PREFIX);
                    let frame = tail.split_to(declared);
                    self.buf = tail;
                    return Ok(frame);
                }
            }

            if r.read_buf(&mut self.buf).await? == 0 {
                // EOF: clean close or truncated frame.
                return Err(Error::Closed);
            }
        }
    }
}

/// Read one frame and decode it into `T`.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let payload = read_raw_frame(r).await?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frame_roundtrip_preserves_values() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        let writer = tokio::spawn(async move {
            write_frame(&mut client, &json!({ "hello": "world", "n": 7 }))
                .await
                .expect("write_frame");
        });

        let decoded: serde_json::Value = read_frame(&mut server).await.expect("read_frame");
        writer.await.expect("writer task");
        assert_eq!(decoded, json!({ "hello": "world", "n": 7 }));
    }

    #[tokio::test]
    async fn oversized_declared_length_is_rejected() {
        let (mut client, mut server) = tokio::io::duplex(64);

        // Claim a gigantic frame without sending any payload.
        client
            .write_all(&(u32::MAX).to_le_bytes())
            .await
            .expect("header write");

        match read_frame::<_, serde_json::Value>(&mut server).await {
            Err(Error::FrameTooLarge { size, max }) => {
                assert_eq!(size, u32::MAX as usize);
                assert_eq!(max, MAX_FRAME_BYTES);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_serialized_payload_is_rejected_before_writing() {
        let (mut client, _server) = tokio::io::duplex(16);
        let huge = "x".repeat(MAX_FRAME_BYTES + 1);

        match write_frame(&mut client, &huge).await {
            Err(err @ Error::FrameTooLarge { .. }) => {
                assert!(err.to_string().contains("frame too large"));
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clean_close_surfaces_as_closed_error() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);

        match read_frame::<_, serde_json::Value>(&mut server).await {
            Err(Error::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    // Regression: frames arriving coalesced in one read must queue, not be
    // discarded. The daemon pipelines Hello + response back-to-back; the old
    // one-shot reader silently dropped the second frame and hung the client
    // until its request timeout (CI run 32764368276).
    #[tokio::test]
    async fn pipelined_frames_are_queued_not_dropped() {
        use std::io::Cursor;

        let mut frame_one = serde_json::to_vec(&json!({ "event": "hello" })).unwrap();
        let mut frame_two = serde_json::to_vec(&json!({ "status": "ok", "result": 1 })).unwrap();
        let mut wire = Vec::new();
        wire.extend_from_slice(&(frame_one.len() as u32).to_le_bytes());
        wire.append(&mut frame_one);
        wire.extend_from_slice(&(frame_two.len() as u32).to_le_bytes());
        wire.append(&mut frame_two);

        let mut cursor = Cursor::new(wire);
        let mut reader = FrameReader::new();

        let first = reader.next(&mut cursor).await.expect("first frame");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&first).unwrap(),
            json!({ "event": "hello" })
        );

        let second = reader.next(&mut cursor).await.expect("second frame");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&second).unwrap(),
            json!({ "status": "ok", "result": 1 })
        );

        // Stream exhausted: next call reports a clean close.
        match reader.next(&mut cursor).await {
            Err(Error::Closed) => {}
            other => panic!("expected Closed after drained pipeline, got {other:?}"),
        }
    }
}
