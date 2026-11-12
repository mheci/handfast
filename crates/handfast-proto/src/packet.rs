//! Packet model shared by every transport.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{ProtoError, Result};

/// A decoded KDE Connect packet.
///
/// `id` is deprecated by the protocol but must still be populated by senders.
/// `body` is always a JSON object; unknown fields are preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Packet {
    /// Deprecated packet identifier. Senders populate it with a timestamp.
    #[serde(default)]
    pub id: i64,
    /// Packet type, for example `kdeconnect.ping`.
    #[serde(rename = "type")]
    pub ptype: String,
    /// Packet body. Always a JSON object.
    #[serde(default)]
    pub body: Map<String, Value>,
    /// Declared payload size in bytes.
    #[serde(
        rename = "payloadSize",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_size: Option<i64>,
    /// Payload transfer channel information.
    #[serde(
        rename = "payloadTransferInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_transfer_info: Option<PayloadTransferInfo>,
}

/// Where a peer should connect to read a payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PayloadTransferInfo {
    /// TCP port carrying the payload.
    pub port: u16,
    /// Unknown fields are preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl PayloadTransferInfo {
    /// Create transfer information with no extra fields.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            port,
            extra: Map::new(),
        }
    }
}

impl Packet {
    /// Build a packet with an object body.
    ///
    /// # Errors
    ///
    /// Fails when the packet type is malformed or the body is not an object.
    pub fn new(ptype: impl Into<String>, body: Value) -> Result<Self> {
        let ptype = ptype.into();
        if !is_valid_packet_type(&ptype) {
            return Err(ProtoError::InvalidPacketType(ptype));
        }
        let Value::Object(body) = body else {
            return Err(ProtoError::BodyNotObject);
        };
        Ok(Self {
            id: 0,
            ptype,
            body,
            payload_size: None,
            payload_transfer_info: None,
        })
    }

    /// Build a packet with an empty body.
    ///
    /// # Errors
    ///
    /// Fails when the packet type is malformed.
    pub fn empty(ptype: impl Into<String>) -> Result<Self> {
        Self::new(ptype, Value::Object(Map::new()))
    }

    /// Build a packet from a serializable body.
    ///
    /// # Errors
    ///
    /// Fails when the packet type is malformed or the body does not serialize to
    /// an object.
    pub fn from_body<T: Serialize>(ptype: impl Into<String>, body: &T) -> Result<Self> {
        Self::new(ptype, serde_json::to_value(body)?)
    }

    /// Attach payload transfer information.
    ///
    /// # Errors
    ///
    /// Fails when the payload size is negative other than the streaming marker
    /// `-1`, or when the port is missing.
    pub fn with_payload(mut self, size: i64, port: u16) -> Result<Self> {
        if size < -1 {
            return Err(ProtoError::Identity(format!("invalid payload size {size}")));
        }
        self.payload_size = Some(size);
        self.payload_transfer_info = Some(PayloadTransferInfo::new(port));
        Ok(self)
    }

    /// Whether this packet announces a payload.
    #[must_use]
    pub fn has_payload(&self) -> bool {
        self.payload_size.is_some()
    }

    /// Read the payload size and port.
    ///
    /// # Errors
    ///
    /// Fails when either field is missing or the size is invalid.
    pub fn payload(&self) -> Result<(u64, u16)> {
        let size = self.payload_size.ok_or(ProtoError::MissingPayloadInfo)?;
        let info = self
            .payload_transfer_info
            .as_ref()
            .ok_or(ProtoError::MissingPayloadInfo)?;
        if size < 0 {
            return Err(ProtoError::Identity(format!(
                "unsupported payload size {size}"
            )));
        }
        Ok((size as u64, info.port))
    }

    /// Validate structural invariants after deserialization.
    ///
    /// # Errors
    ///
    /// Fails when the packet type is malformed or a payload size is impossible.
    pub fn validate(&self) -> Result<()> {
        if !is_valid_packet_type(&self.ptype) {
            return Err(ProtoError::InvalidPacketType(self.ptype.clone()));
        }
        if self.payload_size.is_some_and(|size| size < -1) {
            return Err(ProtoError::Identity(format!(
                "invalid payload size {}",
                self.payload_size.unwrap_or_default()
            )));
        }
        Ok(())
    }
}

/// Check a packet type against `kdeconnect.<lowercase>[.<lowercase>]...`.
#[must_use]
pub fn is_valid_packet_type(ptype: &str) -> bool {
    let Some(rest) = ptype.strip_prefix("kdeconnect.") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    rest.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_types_pass() {
        for ty in crate::types::ALL {
            assert!(is_valid_packet_type(ty), "{ty}");
        }
    }

    #[test]
    fn invalid_types_are_rejected() {
        for ty in [
            "",
            "kdeconnect",
            "kdeconnect.",
            "kdeconnect..ping",
            "kdeconnect.Ping",
            "kdeconnect.ping!",
            "other.ping",
        ] {
            assert!(!is_valid_packet_type(ty), "{ty}");
        }
    }

    #[test]
    fn packet_requires_object_body() {
        assert!(Packet::new(crate::types::PING, json!(null)).is_err());
        assert!(Packet::new(crate::types::PING, json!([])).is_err());
        assert!(Packet::new(crate::types::PING, json!("text")).is_err());
        assert!(Packet::new(crate::types::PING, json!({})).is_ok());
    }

    #[test]
    fn payload_round_trip() {
        let packet = Packet::new(crate::types::SHARE_REQUEST, json!({"filename": "a.png"}))
            .unwrap()
            .with_payload(882, 1739)
            .unwrap();
        assert_eq!(packet.payload().unwrap(), (882, 1739));
        assert!(packet.has_payload());
    }

    #[test]
    fn negative_payload_is_rejected() {
        let packet = Packet::empty(crate::types::PING).unwrap();
        assert!(packet.with_payload(-2, 1).is_err());
    }

    #[test]
    fn missing_transfer_info_is_rejected() {
        let mut packet = Packet::empty(crate::types::PING).unwrap();
        packet.payload_size = Some(10);
        assert!(packet.payload().is_err());
    }

    #[test]
    fn unknown_transfer_fields_are_preserved() {
        let value = json!({
            "type": "kdeconnect.share.request",
            "body": {},
            "payloadSize": 4,
            "payloadTransferInfo": {"port": 42, "future": true}
        });
        let packet: Packet = serde_json::from_value(value).unwrap();
        packet.validate().unwrap();
        let back = serde_json::to_value(&packet).unwrap();
        assert_eq!(back["payloadTransferInfo"]["future"], json!(true));
    }
}
