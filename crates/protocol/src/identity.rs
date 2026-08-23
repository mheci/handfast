//! Device identity advertisement model (`kdeconnect.identity`).
//!
//! An [`Identity`] describes a KDE Connect device on the network. It is
//! broadcast as a UDP datagram during discovery and exchanged again inside the
//! TLS channel immediately after connecting. The `deviceId` doubles as the
//! pairing identifier and equals the CN of the device certificate
//! ([`crate::tls`]).

/// Advertisement describing a KDE Connect device.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Identity {
    /// Globally unique device identifier (upstream Android peers send the
    /// 40-character lowercase hex SHA-256 of the certificate public key).
    #[serde(rename = "deviceId")]
    pub device_id: String,
    /// Human-readable device name shown to users.
    pub name: String,
    /// Device form factor: `"phone" | "desktop" | "laptop" | "tablet" | "tv"`.
    #[serde(rename = "deviceType")]
    pub device_type: String,
    /// Upstream protocol version; peers reporting a different major are
    /// incompatible. Older peers omit the field, hence the default.
    #[serde(rename = "ProtocolVersion", default)]
    pub protocol_version: u16,
    /// Packet types this device knows how to receive.
    #[serde(rename = "incomingCapabilities", default)]
    pub incoming: Vec<String>,
    /// Packet types this device knows how to send.
    #[serde(rename = "outgoingCapabilities", default)]
    pub outgoing: Vec<String>,
    /// TCP port this device listens on; `0` when unknown (very old peers).
    #[serde(rename = "tcpSourcePort", default)]
    pub tcp_source_port: u16,
}

impl Identity {
    /// Returns `true` when `pkt_type` appears in `incomingCapabilities`.
    ///
    /// This is strict list membership: an empty capability list yields `false`
    /// for everything. Deciding whether legacy peers without advertised
    /// capabilities should be treated as accepting everything is daemon policy,
    /// not protocol policy.
    pub fn supports_incoming(&self, pkt_type: &str) -> bool {
        self.incoming.iter().any(|cap| cap == pkt_type)
    }

    /// Returns `true` when `pkt_type` appears in `outgoingCapabilities`.
    ///
    /// See [`Identity::supports_incoming`] for the empty-list caveat.
    pub fn supports_outgoing(&self, pkt_type: &str) -> bool {
        self.outgoing.iter().any(|cap| cap == pkt_type)
    }
}
