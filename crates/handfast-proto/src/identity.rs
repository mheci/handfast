//! Device identity, capabilities and pairing bodies.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::{ProtoError, Result};
use crate::packet::{is_valid_packet_type, Packet};
use crate::types;

/// Protocol version implemented by this crate.
pub const PROTOCOL_VERSION: u8 = 8;

/// Minimum accepted device ID length.
pub const MIN_DEVICE_ID_LEN: usize = 32;
/// Maximum accepted device ID length.
pub const MAX_DEVICE_ID_LEN: usize = 38;
/// Maximum device name length.
pub const MAX_DEVICE_NAME_LEN: usize = 32;

/// A validated KDE Connect device identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(String);

impl DeviceId {
    /// Validate an existing identifier.
    ///
    /// # Errors
    ///
    /// Fails when the identifier is outside 32-38 characters or contains
    /// characters outside ASCII alphanumerics, `-` and `_`.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !(MIN_DEVICE_ID_LEN..=MAX_DEVICE_ID_LEN).contains(&value.len()) {
            return Err(ProtoError::Identity(format!(
                "device id length {} outside {MIN_DEVICE_ID_LEN}-{MAX_DEVICE_ID_LEN}",
                value.len()
            )));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ProtoError::Identity(
                "device id contains invalid characters".to_string(),
            ));
        }
        Ok(Self(value))
    }

    /// Generate a new identifier from a random UUID.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }

    /// Borrow the identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A human-readable device name sanitized for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceName(String);

impl DeviceName {
    /// Sanitize and validate a peer-provided name.
    ///
    /// Forbidden punctuation is removed, control characters are dropped and
    /// the result is truncated to 32 characters. An empty result becomes
    /// `Unnamed device`.
    #[must_use]
    pub fn new(value: &str) -> Self {
        const FORBIDDEN: &[char] = &[
            '"', '\'', ',', ';', ':', '.', '!', '?', '(', ')', '[', ']', '<', '>',
        ];
        let mut cleaned: String = value
            .chars()
            .filter(|ch| !is_unsafe_name_char(*ch) && !FORBIDDEN.contains(ch))
            .collect();
        cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        cleaned = cleaned.chars().take(MAX_DEVICE_NAME_LEN).collect();
        if cleaned.trim().is_empty() {
            cleaned = "Unnamed device".to_string();
        }
        Self(cleaned)
    }

    /// Borrow the sanitized name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_unsafe_name_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{200B}'..='\u{200D}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

impl std::fmt::Display for DeviceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Device type from the identity packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    /// Desktop computer.
    Desktop,
    /// Laptop.
    Laptop,
    /// Phone.
    Phone,
    /// Tablet.
    Tablet,
    /// Television.
    Tv,
    /// A type this version does not know. Used for display only.
    Unknown,
}

impl DeviceType {
    /// The wire name of this device type.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Laptop => "laptop",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Tv => "tv",
            Self::Unknown => "unknown",
        }
    }

    fn from_wire(value: &str) -> Self {
        match value {
            "desktop" => Self::Desktop,
            "laptop" => Self::Laptop,
            "phone" => Self::Phone,
            "tablet" => Self::Tablet,
            "tv" => Self::Tv,
            _ => Self::Unknown,
        }
    }
}

/// Packet types a device can consume and provide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    incoming: BTreeSet<String>,
    outgoing: BTreeSet<String>,
}

impl Capabilities {
    /// Build capabilities from packet type lists.
    ///
    /// # Errors
    ///
    /// Fails when any entry is not a valid `kdeconnect.*` packet type.
    pub fn new<I, J>(incoming: I, outgoing: J) -> Result<Self>
    where
        I: IntoIterator,
        I::Item: Into<String>,
        J: IntoIterator,
        J::Item: Into<String>,
    {
        let incoming = collect_types(incoming)?;
        let outgoing = collect_types(outgoing)?;
        Ok(Self { incoming, outgoing })
    }

    /// Packet types this device consumes.
    #[must_use]
    pub fn incoming(&self) -> &BTreeSet<String> {
        &self.incoming
    }

    /// Packet types this device provides.
    #[must_use]
    pub fn outgoing(&self) -> &BTreeSet<String> {
        &self.outgoing
    }

    /// Whether the peer accepts a packet type.
    #[must_use]
    pub fn accepts(&self, packet_type: &str) -> bool {
        self.incoming.contains(packet_type)
    }
}

fn collect_types<I>(types: I) -> Result<BTreeSet<String>>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut result = BTreeSet::new();
    for packet_type in types {
        let packet_type = packet_type.into();
        if !is_valid_packet_type(&packet_type) {
            return Err(ProtoError::InvalidPacketType(packet_type));
        }
        result.insert(packet_type);
    }
    Ok(result)
}

/// A decoded identity packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Stable device identifier.
    pub device_id: DeviceId,
    /// Display name.
    pub device_name: DeviceName,
    /// Device type hint.
    pub device_type: DeviceType,
    /// Negotiated packet capabilities.
    pub capabilities: Capabilities,
    /// Protocol version, 7 or 8.
    pub protocol_version: u8,
    /// TCP control port announced over UDP discovery.
    pub tcp_port: Option<u16>,
}

impl Identity {
    /// Construct an identity for this device.
    #[must_use]
    pub fn new(
        device_id: DeviceId,
        device_name: DeviceName,
        device_type: DeviceType,
        capabilities: Capabilities,
        tcp_port: Option<u16>,
    ) -> Self {
        Self {
            device_id,
            device_name,
            device_type,
            capabilities,
            protocol_version: PROTOCOL_VERSION,
            tcp_port,
        }
    }

    /// Encode the identity as a wire packet.
    ///
    /// # Errors
    ///
    /// Fails only if the packet body cannot be constructed.
    pub fn to_packet(&self) -> Result<Packet> {
        let mut body = json!({
            "deviceId": self.device_id.as_str(),
            "deviceName": self.device_name.as_str(),
            "deviceType": self.device_type,
            "incomingCapabilities": self.capabilities.incoming(),
            "outgoingCapabilities": self.capabilities.outgoing(),
            "protocolVersion": self.protocol_version,
        });
        if let Some(port) = self.tcp_port {
            body["tcpPort"] = json!(port);
        }
        Packet::new(types::IDENTITY, body)
    }

    /// Decode an identity packet.
    ///
    /// # Errors
    ///
    /// Fails when required fields are missing, the protocol version is not 8,
    /// or the device identifier is malformed.
    pub fn from_packet(packet: &Packet) -> Result<Self> {
        if packet.ptype != types::IDENTITY {
            return Err(ProtoError::Identity(format!(
                "expected {}, got {}",
                types::IDENTITY,
                packet.ptype
            )));
        }
        let device_id = required_str(&packet.body, "deviceId")?;
        let device_id = DeviceId::new(device_id)?;
        let device_name = required_str(&packet.body, "deviceName")?;
        let device_name = DeviceName::new(&device_name);
        let device_type = DeviceType::from_wire(&required_str(&packet.body, "deviceType")?);
        let incoming = required_string_array(&packet.body, "incomingCapabilities")?;
        let outgoing = required_string_array(&packet.body, "outgoingCapabilities")?;
        let capabilities = Capabilities::new(incoming, outgoing)?;
        let protocol_version = packet
            .body
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProtoError::Identity("missing protocolVersion".to_string()))?;
        let protocol_version = u8::try_from(protocol_version)
            .map_err(|_| ProtoError::Identity("protocolVersion out of range".to_string()))?;
        if protocol_version != PROTOCOL_VERSION {
            return Err(ProtoError::Identity(format!(
                "unsupported protocol version {protocol_version}"
            )));
        }
        let tcp_port = packet
            .body
            .get("tcpPort")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok())
                    .ok_or_else(|| ProtoError::Identity("invalid tcpPort".to_string()))
            })
            .transpose()?;
        if tcp_port == Some(0) {
            return Err(ProtoError::Identity("tcpPort must not be 0".to_string()));
        }

        Ok(Self {
            device_id,
            device_name,
            device_type,
            capabilities,
            protocol_version,
            tcp_port,
        })
    }
}

fn required_str(body: &serde_json::Map<String, Value>, key: &str) -> Result<String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| ProtoError::Identity(format!("missing {key}")))
}

fn required_string_array(body: &serde_json::Map<String, Value>, key: &str) -> Result<Vec<String>> {
    let values = body
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| ProtoError::Identity(format!("missing {key}")))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| ProtoError::Identity(format!("{key} contains a non-string")))
        })
        .collect()
}

/// Pairing request, acceptance or rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    /// `true` requests or accepts pairing, `false` rejects or unpairs.
    pub pair: bool,
    /// Request timestamp in seconds since the Unix epoch. Required for a new
    /// request, absent on acceptance.
    pub timestamp: Option<i64>,
}

impl PairingRequest {
    /// Build a new pairing request.
    #[must_use]
    pub fn request(timestamp: i64) -> Self {
        Self {
            pair: true,
            timestamp: Some(timestamp),
        }
    }

    /// Build an acceptance response.
    #[must_use]
    pub fn accept() -> Self {
        Self {
            pair: true,
            timestamp: None,
        }
    }

    /// Build a rejection or unpair packet.
    #[must_use]
    pub fn reject() -> Self {
        Self {
            pair: false,
            timestamp: None,
        }
    }

    /// Whether this is a new request rather than an acceptance.
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.pair && self.timestamp.is_some()
    }

    /// Encode as a wire packet.
    ///
    /// # Errors
    ///
    /// Fails only if the packet body cannot be constructed.
    pub fn to_packet(&self) -> Result<Packet> {
        let mut body = json!({ "pair": self.pair });
        if let Some(timestamp) = self.timestamp {
            body["timestamp"] = json!(timestamp);
        }
        Packet::new(types::PAIR, body)
    }

    /// Decode a pairing packet.
    ///
    /// # Errors
    ///
    /// Fails when `pair` is missing or the timestamp is not an integer.
    pub fn from_packet(packet: &Packet) -> Result<Self> {
        if packet.ptype != types::PAIR {
            return Err(ProtoError::Identity(format!(
                "expected {}, got {}",
                types::PAIR,
                packet.ptype
            )));
        }
        let pair = packet
            .body
            .get("pair")
            .and_then(Value::as_bool)
            .ok_or_else(|| ProtoError::Identity("missing pair".to_string()))?;
        let timestamp = packet
            .body
            .get("timestamp")
            .map(|value| {
                value
                    .as_i64()
                    .ok_or_else(|| ProtoError::Identity("invalid timestamp".to_string()))
            })
            .transpose()?;
        Ok(Self { pair, timestamp })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> Identity {
        let capabilities = Capabilities::new(
            [types::PING, types::BATTERY],
            [types::PING, types::CLIPBOARD],
        )
        .unwrap();
        Identity::new(
            DeviceId::generate(),
            DeviceName::new("Test desktop"),
            DeviceType::Desktop,
            capabilities,
            Some(1716),
        )
    }

    #[test]
    fn generated_ids_are_valid() {
        for _ in 0..64 {
            let id = DeviceId::generate();
            assert_eq!(id.as_str().len(), 32);
            assert!(DeviceId::new(id.as_str()).is_ok());
        }
    }

    #[test]
    fn invalid_ids_are_rejected() {
        assert!(DeviceId::new("short").is_err());
        assert!(DeviceId::new("x".repeat(39)).is_err());
        assert!(DeviceId::new("!".repeat(32)).is_err());
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(DeviceName::new("  My: Phone!  ").as_str(), "My Phone");
        assert_eq!(DeviceName::new("").as_str(), "Unnamed device");
        assert_eq!(DeviceName::new(&"a".repeat(64)).as_str().len(), 32);
        assert!(!DeviceName::new("a\nb").as_str().contains('\n'));
    }

    #[test]
    fn identity_round_trip() {
        let identity = identity();
        let packet = identity.to_packet().unwrap();
        let decoded = Identity::from_packet(&packet).unwrap();
        assert_eq!(decoded, identity);
        assert_eq!(packet.body["protocolVersion"], json!(8));
    }

    #[test]
    fn identity_rejects_bad_protocol() {
        let mut packet = identity().to_packet().unwrap();
        packet.body["protocolVersion"] = json!(9);
        assert!(Identity::from_packet(&packet).is_err());
    }

    #[test]
    fn identity_rejects_protocol_seven() {
        let mut packet = identity().to_packet().unwrap();
        packet.body["protocolVersion"] = json!(7);
        assert!(Identity::from_packet(&packet).is_err());
    }

    #[test]
    fn bidi_controls_are_removed_from_names() {
        let name = DeviceName::new("Bank \u{202E}gpj");
        assert!(!name.as_str().contains('\u{202E}'));
        assert_eq!(name.as_str(), "Bank gpj");
    }

    #[test]
    fn device_types_have_wire_names() {
        assert_eq!(DeviceType::Desktop.as_str(), "desktop");
        assert_eq!(DeviceType::Laptop.as_str(), "laptop");
        assert_eq!(DeviceType::Phone.as_str(), "phone");
        assert_eq!(DeviceType::Tablet.as_str(), "tablet");
        assert_eq!(DeviceType::Tv.as_str(), "tv");
        assert_eq!(DeviceType::Unknown.as_str(), "unknown");
    }

    #[test]
    fn unknown_device_types_are_kept_for_display() {
        let mut packet = identity().to_packet().unwrap();
        packet.body["deviceType"] = json!("watch");
        let decoded = Identity::from_packet(&packet).unwrap();
        assert_eq!(decoded.device_type, DeviceType::Unknown);
    }

    #[test]
    fn zero_tcp_port_is_rejected() {
        let mut packet = identity().to_packet().unwrap();
        packet.body["tcpPort"] = json!(0);
        assert!(Identity::from_packet(&packet).is_err());
    }

    #[test]
    fn identity_rejects_missing_fields() {
        let mut packet = identity().to_packet().unwrap();
        packet.body.remove("deviceId");
        assert!(Identity::from_packet(&packet).is_err());
    }

    #[test]
    fn pairing_round_trip() {
        let request = PairingRequest::request(1_700_000_000);
        let packet = request.to_packet().unwrap();
        let decoded = PairingRequest::from_packet(&packet).unwrap();
        assert_eq!(decoded, request);
        assert!(decoded.is_request());

        let accept = PairingRequest::accept();
        assert!(!PairingRequest::from_packet(&accept.to_packet().unwrap())
            .unwrap()
            .is_request());
    }

    #[test]
    fn pairing_rejects_missing_flag() {
        let packet = Packet::empty(types::PAIR).unwrap();
        assert!(PairingRequest::from_packet(&packet).is_err());
    }
}
