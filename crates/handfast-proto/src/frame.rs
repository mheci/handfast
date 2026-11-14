//! Length-prefixed JSON framing.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{ProtoError, Result};
use crate::packet::Packet;

/// Maximum accepted frame size, including the header.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Encode a packet into a complete frame.
///
/// # Errors
///
/// Fails when the packet cannot be serialized or exceeds the frame limit.
pub fn encode(packet: &Packet) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    encode_into(packet, &mut frame)?;
    Ok(frame)
}

/// Encode a packet into a complete frame, reusing `frame`.
///
/// The buffer is cleared first. On success it holds exactly the bytes
/// [`encode`] returns. On error it is empty. A frame rejected as too large
/// also restores the buffer's capacity from before the call.
///
/// # Errors
///
/// Fails when the packet cannot be serialized or exceeds the frame limit.
pub fn encode_into(packet: &Packet, frame: &mut Vec<u8>) -> Result<()> {
    let capacity = frame.capacity();
    frame.clear();
    packet.validate()?;
    frame.extend_from_slice(&[0u8; 4]);
    if let Err(error) = serde_json::to_writer(&mut *frame, packet) {
        frame.clear();
        return Err(error.into());
    }
    let size = frame.len() - 4;
    if size == 0 {
        frame.clear();
        return Err(ProtoError::EmptyFrame);
    }
    if size + 4 > MAX_FRAME_SIZE {
        // clear() keeps the capacity the oversized body grew, so drop it.
        *frame = Vec::with_capacity(capacity);
        return Err(ProtoError::FrameTooLarge {
            size: size + 4,
            max: MAX_FRAME_SIZE,
        });
    }
    frame[..4].copy_from_slice(&(size as u32).to_be_bytes());
    Ok(())
}

/// Decode a complete frame.
///
/// # Errors
///
/// Fails on a short header, a declared length that does not match the buffer,
/// an oversized frame or malformed JSON.
pub fn decode(frame: &[u8]) -> Result<Packet> {
    if frame.len() < 4 {
        return Err(ProtoError::EmptyFrame);
    }
    if frame.len() > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge {
            size: frame.len(),
            max: MAX_FRAME_SIZE,
        });
    }
    let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let payload = &frame[4..];
    if declared == 0 {
        return Err(ProtoError::EmptyFrame);
    }
    if declared != payload.len() {
        return Err(ProtoError::Identity(format!(
            "frame length mismatch: header says {declared}, got {}",
            payload.len()
        )));
    }
    let packet: Packet = serde_json::from_slice(payload)?;
    packet.validate()?;
    Ok(packet)
}

/// Read one frame from an async stream.
///
/// Returns `Ok(None)` on a clean EOF before the header.
///
/// # Errors
///
/// Fails on a truncated header or body, an oversized frame or malformed JSON.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Packet>> {
    let mut header = [0u8; 4];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 {
        return Err(ProtoError::EmptyFrame);
    }
    if size + 4 > MAX_FRAME_SIZE {
        return Err(ProtoError::FrameTooLarge {
            size: size + 4,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut payload = vec![0u8; size];
    reader.read_exact(&mut payload).await?;
    let packet: Packet = serde_json::from_slice(&payload)?;
    packet.validate()?;
    Ok(Some(packet))
}

/// Write one packet as a frame and flush it.
///
/// # Errors
///
/// Fails when the packet cannot be encoded or the stream rejects the write.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, packet: &Packet) -> Result<()> {
    let frame = encode(packet)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types;
    use serde_json::json;
    use tokio::io::duplex;

    fn ping() -> Packet {
        Packet::from_body(types::PING, &json!({"message": "hello"})).unwrap()
    }

    #[test]
    fn encode_decode_round_trip() {
        let packet = ping();
        let frame = encode(&packet).unwrap();
        assert_eq!(
            &frame[..4],
            &u32::try_from(frame.len() - 4).unwrap().to_be_bytes()
        );
        assert_eq!(decode(&frame).unwrap(), packet);
    }

    #[test]
    fn decode_rejects_length_mismatch() {
        let mut frame = encode(&ping()).unwrap();
        frame.push(0);
        assert!(decode(&frame).is_err());
    }

    #[test]
    fn decode_rejects_empty_payload() {
        assert!(decode(&[0, 0, 0, 0]).is_err());
    }

    #[test]
    fn decode_rejects_oversized_frame() {
        let frame = vec![0u8; MAX_FRAME_SIZE + 1];
        assert!(decode(&frame).is_err());
    }

    #[test]
    fn rejects_oversized_packet() {
        let packet = Packet::new(types::PING, json!({"blob": "x".repeat(MAX_FRAME_SIZE)})).unwrap();
        assert!(encode(&packet).is_err());
    }

    #[test]
    fn oversized_packet_restores_frame_capacity() {
        let mut frame = Vec::with_capacity(64);
        let packet = Packet::new(types::PING, json!({"blob": "x".repeat(MAX_FRAME_SIZE)})).unwrap();
        let error = encode_into(&packet, &mut frame).unwrap_err();
        assert!(matches!(error, ProtoError::FrameTooLarge { .. }));
        assert!(frame.is_empty());
        assert_eq!(frame.capacity(), 64);
    }

    #[tokio::test]
    async fn async_round_trip() {
        let (mut client, mut server) = duplex(4096);
        let packet = ping();
        write_frame(&mut client, &packet).await.unwrap();
        let received = read_frame(&mut server).await.unwrap().unwrap();
        assert_eq!(received, packet);
    }

    #[tokio::test]
    async fn async_eof_is_none() {
        let (client, mut server) = duplex(64);
        drop(client);
        assert!(read_frame(&mut server).await.unwrap().is_none());
    }
}
