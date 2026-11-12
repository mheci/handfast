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

/// Lowest TCP port in the KDE Connect range.
pub const MIN_TCP_PORT: u16 = 1716;
/// Highest TCP port in the KDE Connect range.
pub const MAX_TCP_PORT: u16 = 1764;

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

/// The plaintext connection hello sent by the dialing device.
///
/// Android sends its full identity plus the target fields. Desktop sends only
/// `deviceId`, `deviceName`, `protocolVersion`, `targetDeviceId` and
/// `targetProtocolVersion`. Name, type, capabilities and TCP port are optional
/// here so both shapes parse. The encrypted [`Identity`] exchange stays strict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityHello {
    /// The dialing device's identity.
    pub identity: Identity,
    /// Device ID of the listener this hello targets.
    pub target_device_id: DeviceId,
    /// Protocol version the dialer expects from the listener.
    pub target_protocol_version: u8,
}

impl IdentityHello {
    /// Build a hello for a target device at protocol 8.
    #[must_use]
    pub fn new(identity: Identity, target_device_id: DeviceId) -> Self {
        Self {
            identity,
            target_device_id,
            target_protocol_version: PROTOCOL_VERSION,
        }
    }

    /// Encode the hello as a wire packet.
    ///
    /// # Errors
    ///
    /// Fails when the target protocol version is not 8 or the identity body
    /// cannot be constructed.
    pub fn to_packet(&self) -> Result<Packet> {
        if self.target_protocol_version != PROTOCOL_VERSION {
            return Err(ProtoError::Identity(format!(
                "unsupported target protocol version {}",
                self.target_protocol_version
            )));
        }
        let mut packet = self.identity.to_packet()?;
        packet.body.insert(
            "targetDeviceId".to_string(),
            json!(self.target_device_id.as_str()),
        );
        packet.body.insert(
            "targetProtocolVersion".to_string(),
            json!(self.target_protocol_version),
        );
        Ok(packet)
    }

    /// Decode a plaintext hello.
    ///
    /// Accepts `targetProtocolVersion` as a JSON number or a JSON string. A
    /// hello that targets its own device is rejected.
    ///
    /// # Errors
    ///
    /// Fails when the packet is not `kdeconnect.identity`, a required field is
    /// missing or malformed, the protocol version or target version is not 8,
    /// or the device targets itself.
    pub fn from_packet(packet: &Packet) -> Result<Self> {
        if packet.ptype != types::IDENTITY {
            return Err(ProtoError::Identity(format!(
                "expected {}, got {}",
                types::IDENTITY,
                packet.ptype
            )));
        }
        let identity = hello_identity(&packet.body)?;
        let target_device_id = DeviceId::new(required_str(&packet.body, "targetDeviceId")?)?;
        let target_protocol_version = target_protocol_version(&packet.body)?;
        if identity.device_id == target_device_id {
            return Err(ProtoError::Identity(
                "hello targets its own device".to_string(),
            ));
        }
        Ok(Self {
            identity,
            target_device_id,
            target_protocol_version,
        })
    }
}

fn hello_identity(body: &serde_json::Map<String, Value>) -> Result<Identity> {
    let device_name = optional_str(body, "deviceName")?.unwrap_or_default();
    let device_type = optional_str(body, "deviceType")?.unwrap_or_default();
    let incoming = optional_string_array(body, "incomingCapabilities")?.unwrap_or_default();
    let outgoing = optional_string_array(body, "outgoingCapabilities")?.unwrap_or_default();
    Ok(Identity {
        device_id: DeviceId::new(required_str(body, "deviceId")?)?,
        device_name: DeviceName::new(&device_name),
        device_type: DeviceType::from_wire(&device_type),
        capabilities: Capabilities::new(incoming, outgoing)?,
        protocol_version: protocol_version(body, "protocolVersion")?,
        tcp_port: optional_tcp_port(body)?,
    })
}

fn target_protocol_version(body: &serde_json::Map<String, Value>) -> Result<u8> {
    let raw = body
        .get("targetProtocolVersion")
        .ok_or_else(|| ProtoError::Identity("missing targetProtocolVersion".to_string()))?;
    let parsed = match raw {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse::<u64>().ok(),
        _ => None,
    }
    .ok_or_else(|| ProtoError::Identity("invalid targetProtocolVersion".to_string()))?;
    let version = u8::try_from(parsed)
        .map_err(|_| ProtoError::Identity("targetProtocolVersion out of range".to_string()))?;
    if version != PROTOCOL_VERSION {
        return Err(ProtoError::Identity(format!(
            "unsupported target protocol version {version}"
        )));
    }
    Ok(version)
}

fn optional_str(body: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ProtoError::Identity(format!("{key} must be a string"))),
    }
}

fn optional_string_array(
    body: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>> {
    let Some(value) = body.get(key) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| ProtoError::Identity(format!("{key} must be an array")))?;
    let values = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| ProtoError::Identity(format!("{key} contains a non-string")))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(values))
}

fn protocol_version(body: &serde_json::Map<String, Value>, key: &str) -> Result<u8> {
    let version = body
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| ProtoError::Identity(format!("missing {key}")))?;
    let version =
        u8::try_from(version).map_err(|_| ProtoError::Identity(format!("{key} out of range")))?;
    if version != PROTOCOL_VERSION {
        return Err(ProtoError::Identity(format!(
            "unsupported protocol version {version}"
        )));
    }
    Ok(version)
}

fn optional_tcp_port(body: &serde_json::Map<String, Value>) -> Result<Option<u16>> {
    let Some(value) = body.get("tcpPort") else {
        return Ok(None);
    };
    let port = value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| ProtoError::Identity("invalid tcpPort".to_string()))?;
    if port == 0 {
        return Err(ProtoError::Identity("tcpPort must not be 0".to_string()));
    }
    Ok(Some(port))
}

/// The reduced identity packet broadcast over UDP discovery.
///
/// Desktop sends only `deviceId`, `deviceName`, `protocolVersion` and
/// `tcpPort`. Android sends its full identity plus `tcpPort`. Extra fields are
/// ignored, so the announcement is unauthenticated and never trusted for
/// pairing or naming a connected device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryAnnouncement {
    /// Announcing device identifier.
    pub device_id: DeviceId,
    /// Sanitized display name.
    pub device_name: DeviceName,
    /// Device type hint, or `Unknown` when the peer omits it.
    pub device_type: DeviceType,
    /// Announced protocol version. Always 8.
    pub protocol_version: u8,
    /// TCP port where the peer accepts KDE Connect links.
    pub tcp_port: u16,
}

impl DiscoveryAnnouncement {
    /// Build an announcement for this device.
    #[must_use]
    pub fn new(
        device_id: DeviceId,
        device_name: DeviceName,
        device_type: DeviceType,
        tcp_port: u16,
    ) -> Self {
        Self {
            device_id,
            device_name,
            device_type,
            protocol_version: PROTOCOL_VERSION,
            tcp_port,
        }
    }

    /// Encode the minimal desktop UDP shape.
    ///
    /// # Errors
    ///
    /// Fails when the protocol version is not 8, the port is outside
    /// 1716-1764, or the packet body cannot be constructed.
    pub fn to_packet(&self) -> Result<Packet> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtoError::Identity(format!(
                "unsupported protocol version {}",
                self.protocol_version
            )));
        }
        if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&self.tcp_port) {
            return Err(ProtoError::Identity(format!(
                "tcpPort {} outside {MIN_TCP_PORT}-{MAX_TCP_PORT}",
                self.tcp_port
            )));
        }
        Packet::new(
            types::IDENTITY,
            json!({
                "deviceId": self.device_id.as_str(),
                "deviceName": self.device_name.as_str(),
                "protocolVersion": self.protocol_version,
                "tcpPort": self.tcp_port,
            }),
        )
    }

    /// Decode a UDP discovery announcement.
    ///
    /// Accepts the minimal desktop packet and the Android full identity packet.
    ///
    /// # Errors
    ///
    /// Fails when the packet is not `kdeconnect.identity`, the device ID is
    /// missing or malformed, the protocol version is not 8, or `tcpPort` is
    /// missing or outside 1716-1764.
    pub fn from_packet(packet: &Packet) -> Result<Self> {
        if packet.ptype != types::IDENTITY {
            return Err(ProtoError::Identity(format!(
                "expected {}, got {}",
                types::IDENTITY,
                packet.ptype
            )));
        }
        let device_id = DeviceId::new(required_str(&packet.body, "deviceId")?)?;
        let device_name = optional_str(&packet.body, "deviceName")?.unwrap_or_default();
        let device_type = optional_str(&packet.body, "deviceType")?.unwrap_or_default();
        let protocol_version = protocol_version(&packet.body, "protocolVersion")?;
        let tcp_port = packet
            .body
            .get("tcpPort")
            .ok_or_else(|| ProtoError::Identity("missing tcpPort".to_string()))?
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .ok_or_else(|| ProtoError::Identity("invalid tcpPort".to_string()))?;
        if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&tcp_port) {
            return Err(ProtoError::Identity(format!(
                "tcpPort {tcp_port} outside {MIN_TCP_PORT}-{MAX_TCP_PORT}"
            )));
        }
        Ok(Self {
            device_id,
            device_name: DeviceName::new(&device_name),
            device_type: DeviceType::from_wire(&device_type),
            protocol_version,
            tcp_port,
        })
    }
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

    const OTHER_DEVICE_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const DESKTOP_DEVICE_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    fn hello() -> IdentityHello {
        IdentityHello::new(identity(), DeviceId::new(OTHER_DEVICE_ID).unwrap())
    }

    fn announcement() -> DiscoveryAnnouncement {
        DiscoveryAnnouncement::new(
            DeviceId::new(DESKTOP_DEVICE_ID).unwrap(),
            DeviceName::new("Test desktop"),
            DeviceType::Desktop,
            MIN_TCP_PORT,
        )
    }

    #[test]
    fn hello_round_trip() {
        let hello = hello();
        let packet = hello.to_packet().unwrap();
        assert_eq!(packet.body["targetProtocolVersion"], json!(8));
        assert_eq!(IdentityHello::from_packet(&packet).unwrap(), hello);
    }

    #[test]
    fn hello_defaults_optional_fields() {
        let mut packet = hello().to_packet().unwrap();
        for key in [
            "deviceName",
            "deviceType",
            "incomingCapabilities",
            "outgoingCapabilities",
        ] {
            packet.body.remove(key);
        }
        let decoded = IdentityHello::from_packet(&packet).unwrap();
        assert_eq!(decoded.identity.device_name.as_str(), "Unnamed device");
        assert_eq!(decoded.identity.device_type, DeviceType::Unknown);
        assert!(decoded.identity.capabilities.incoming().is_empty());
        assert!(decoded.identity.capabilities.outgoing().is_empty());
    }

    #[test]
    fn hello_accepts_string_target_version() {
        let mut packet = hello().to_packet().unwrap();
        packet.body["targetProtocolVersion"] = json!("8");
        assert_eq!(
            IdentityHello::from_packet(&packet)
                .unwrap()
                .target_protocol_version,
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn hello_rejects_bad_target_version() {
        for value in [
            json!("eight"),
            json!("8.0"),
            json!(8.5),
            json!(null),
            json!(true),
            json!(7),
            json!("7"),
            json!(256),
        ] {
            let mut packet = hello().to_packet().unwrap();
            packet.body["targetProtocolVersion"] = value.clone();
            assert!(IdentityHello::from_packet(&packet).is_err(), "{value}");
        }
    }

    #[test]
    fn hello_rejects_missing_fields() {
        for key in [
            "deviceId",
            "protocolVersion",
            "targetDeviceId",
            "targetProtocolVersion",
        ] {
            let mut packet = hello().to_packet().unwrap();
            packet.body.remove(key);
            assert!(IdentityHello::from_packet(&packet).is_err(), "{key}");
        }
    }

    #[test]
    fn hello_rejects_self_target() {
        let hello = hello();
        let mut packet = hello.to_packet().unwrap();
        packet.body["targetDeviceId"] = json!(hello.identity.device_id.as_str());
        assert!(IdentityHello::from_packet(&packet).is_err());
    }

    #[test]
    fn hello_rejects_invalid_ids() {
        let mut packet = hello().to_packet().unwrap();
        packet.body["targetDeviceId"] = json!("short");
        assert!(IdentityHello::from_packet(&packet).is_err());

        let mut packet = hello().to_packet().unwrap();
        packet.body["deviceId"] = json!("short");
        assert!(IdentityHello::from_packet(&packet).is_err());
    }

    #[test]
    fn hello_rejects_wrong_packet_type() {
        let mut packet = hello().to_packet().unwrap();
        packet.ptype = types::PAIR.to_string();
        assert!(IdentityHello::from_packet(&packet).is_err());
    }

    #[test]
    fn discovery_round_trip() {
        let announcement = announcement();
        let packet = announcement.to_packet().unwrap();
        assert_eq!(packet.body.len(), 4);
        let decoded = DiscoveryAnnouncement::from_packet(&packet).unwrap();
        assert_eq!(decoded.device_id, announcement.device_id);
        assert_eq!(decoded.device_name, announcement.device_name);
        assert_eq!(decoded.device_type, DeviceType::Unknown);
        assert_eq!(decoded.protocol_version, announcement.protocol_version);
        assert_eq!(decoded.tcp_port, announcement.tcp_port);
    }

    #[test]
    fn discovery_rejects_missing_fields() {
        for key in ["deviceId", "protocolVersion", "tcpPort"] {
            let mut packet = announcement().to_packet().unwrap();
            packet.body.remove(key);
            assert!(
                DiscoveryAnnouncement::from_packet(&packet).is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn discovery_rejects_bad_protocol_version() {
        let mut packet = announcement().to_packet().unwrap();
        packet.body["protocolVersion"] = json!(9);
        assert!(DiscoveryAnnouncement::from_packet(&packet).is_err());
    }

    #[test]
    fn discovery_rejects_port_outside_range() {
        for port in [0, MIN_TCP_PORT - 1, MAX_TCP_PORT + 1, u16::MAX] {
            let mut packet = announcement().to_packet().unwrap();
            packet.body["tcpPort"] = json!(port);
            assert!(
                DiscoveryAnnouncement::from_packet(&packet).is_err(),
                "{port}"
            );
        }
    }

    #[test]
    fn discovery_rejects_non_numeric_port() {
        let mut packet = announcement().to_packet().unwrap();
        packet.body["tcpPort"] = json!("1716");
        assert!(DiscoveryAnnouncement::from_packet(&packet).is_err());
    }

    #[test]
    fn discovery_ignores_extra_identity_fields() {
        let mut packet = announcement().to_packet().unwrap();
        packet.body.insert("deviceType".to_string(), json!("phone"));
        packet
            .body
            .insert("incomingCapabilities".to_string(), json!([types::PING]));
        packet
            .body
            .insert("outgoingCapabilities".to_string(), json!([]));
        let decoded = DiscoveryAnnouncement::from_packet(&packet).unwrap();
        assert_eq!(decoded.device_type, DeviceType::Phone);
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn discovery_to_packet_rejects_port_outside_range() {
        let announcement = DiscoveryAnnouncement {
            tcp_port: MAX_TCP_PORT + 1,
            ..announcement()
        };
        assert!(announcement.to_packet().is_err());
    }
}
