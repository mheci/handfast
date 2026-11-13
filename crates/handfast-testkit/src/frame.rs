//! Independent 4-byte length-prefixed JSON framing.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame size accepted by the peer.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// A packet shaped like the KDE Connect wire format, independent of production
/// types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WirePacket {
    /// Deprecated packet identifier.
    #[serde(default)]
    pub id: i64,
    /// Packet type.
    #[serde(rename = "type")]
    pub ptype: String,
    /// Packet body.
    #[serde(default)]
    pub body: Map<String, Value>,
    /// Payload size.
    #[serde(
        rename = "payloadSize",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_size: Option<i64>,
    /// Payload transfer information.
    #[serde(
        rename = "payloadTransferInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_transfer_info: Option<Value>,
}

/// Testkit errors.
#[derive(Debug, Error)]
pub enum TestkitError {
    /// IO failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// The packet or state is not valid.
    #[error("protocol: {0}")]
    Protocol(String),

    /// The peer did not finish in time.
    #[error("timed out")]
    Timeout,

    /// The payload exceeded the configured maximum.
    #[error("payload exceeds {0} bytes")]
    TooLarge(u64),

    /// The peer certificate did not match the expected certificate.
    #[error("peer certificate mismatch")]
    PeerCertificate,
}

/// Result alias.
pub type Result<T> = std::result::Result<T, TestkitError>;

impl WirePacket {
    /// Validate the type and body shape.
    ///
    /// # Errors
    ///
    /// Fails when the type is malformed or a payload size is impossible.
    pub fn validate(&self) -> Result<()> {
        if !is_valid_type(&self.ptype) {
            return Err(TestkitError::Protocol(format!(
                "invalid packet type: {}",
                self.ptype
            )));
        }
        if self.payload_size.is_some_and(|size| size < -1) {
            return Err(TestkitError::Protocol(format!(
                "invalid payload size: {:?}",
                self.payload_size
            )));
        }
        Ok(())
    }
}

/// Check `kdeconnect.<lowercase>...`.
#[must_use]
pub fn is_valid_type(ptype: &str) -> bool {
    let Some(rest) = ptype.strip_prefix("kdeconnect.") else {
        return false;
    };
    !rest.is_empty()
        && rest.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

/// Encode a packet into a complete frame.
///
/// # Errors
///
/// Fails when the packet is invalid or too large.
pub fn encode(packet: &WirePacket) -> Result<Vec<u8>> {
    packet.validate()?;
    let payload = serde_json::to_vec(packet)?;
    if payload.is_empty() {
        return Err(TestkitError::Protocol("empty payload".to_string()));
    }
    if payload.len() + 4 > MAX_FRAME_SIZE {
        return Err(TestkitError::Protocol("frame too large".to_string()));
    }
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode a complete frame.
///
/// # Errors
///
/// Fails on a short or oversized frame, a length mismatch or invalid JSON.
pub fn decode(frame: &[u8]) -> Result<WirePacket> {
    if frame.len() < 4 || frame.len() > MAX_FRAME_SIZE {
        return Err(TestkitError::Protocol("invalid frame length".to_string()));
    }
    let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let payload = &frame[4..];
    if declared == 0 || declared != payload.len() {
        return Err(TestkitError::Protocol("frame length mismatch".to_string()));
    }
    let packet: WirePacket = serde_json::from_slice(payload)?;
    packet.validate()?;
    Ok(packet)
}

/// Read one frame.
///
/// # Errors
///
/// Fails on a truncated or malformed frame.
pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<WirePacket>> {
    let mut header = [0u8; 4];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size + 4 > MAX_FRAME_SIZE {
        return Err(TestkitError::Protocol("invalid frame size".to_string()));
    }
    let mut payload = vec![0u8; size];
    reader.read_exact(&mut payload).await?;
    let packet: WirePacket = serde_json::from_slice(&payload)?;
    packet.validate()?;
    Ok(Some(packet))
}

/// Write one frame and flush it.
///
/// # Errors
///
/// Fails when the packet is invalid or the write fails.
pub async fn write<W: AsyncWrite + Unpin>(writer: &mut W, packet: &WirePacket) -> Result<()> {
    let frame = encode(packet)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::duplex;

    fn ping() -> WirePacket {
        WirePacket {
            id: 0,
            ptype: "kdeconnect.ping".to_string(),
            body: [("message".to_string(), json!("hi"))].into_iter().collect(),
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    #[test]
    fn frame_round_trip() {
        let packet = ping();
        let frame = encode(&packet).unwrap();
        assert_eq!(decode(&frame).unwrap(), packet);
    }

    #[test]
    fn invalid_types_are_rejected() {
        for ptype in [
            "",
            "kdeconnect",
            "kdeconnect.",
            "other.ping",
            "kdeconnect.Ping",
        ] {
            let packet = WirePacket {
                id: 0,
                ptype: ptype.to_string(),
                body: Map::new(),
                payload_size: None,
                payload_transfer_info: None,
            };
            assert!(packet.validate().is_err(), "{ptype}");
        }
    }

    #[tokio::test]
    async fn async_round_trip() {
        let (mut client, mut server) = duplex(1024);
        write(&mut client, &ping()).await.unwrap();
        assert_eq!(read(&mut server).await.unwrap().unwrap(), ping());
    }
}
