//! Device identity advertisement model (`kdeconnect.identity`).
//!
//! An [`Identity`] describes a KDE Connect device on the network. It is
//! broadcast as a UDP datagram during discovery and exchanged again inside the
//! TLS channel immediately after connecting. The `deviceId` doubles as the
//! pairing identifier and equals the CN of the device certificate
//! ([`crate::tls`]).

/// Advertisement describing a KDE Connect device.
///
/// Field names match upstream byte-for-byte (`deviceId`, `deviceName`,
/// `protocolVersion`, `deviceType`, `incomingCapabilities`,
/// `outgoingCapabilities`, `tcpPort`) so the Android app and kdeconnect-kde
/// can parse our advertisements — and we theirs. The `alias` entries accept
/// the legacy Handfast spellings (`name`, `ProtocolVersion`, `tcpSourcePort`)
/// on *read* so mixed-version fleets still discover each other; writes always
/// emit the upstream names.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Identity {
    /// Globally unique device identifier. Upstream Android peers send a
    /// 32-character lowercase hex random UUID (`DeviceHelper.initializeDeviceId`),
    /// validated by Android against `^[a-zA-Z0-9_-]{32,38}$`; Handfast uses a
    /// canonical 36-character UUIDv4 (hyphens are within that charset).
    #[serde(rename = "deviceId")]
    pub device_id: String,
    /// Human-readable device name shown to users (`deviceName` upstream).
    /// Android rejects identity packets with a blank `deviceName`, so this
    /// field is mandatory on the wire.
    #[serde(rename = "deviceName", alias = "name")]
    pub name: String,
    /// Device form factor: `"phone" | "desktop" | "laptop" | "tablet" | "tv"`.
    #[serde(rename = "deviceType")]
    pub device_type: String,
    /// Upstream protocol version; peers reporting a different major are
    /// incompatible. Older peers omit the field, hence the default. Note the
    /// lowercase `protocolVersion` spelling — the Android app and
    /// kdeconnect-kde both read exactly this key (and reject mismatches
    /// during the post-TLS identity re-exchange).
    #[serde(rename = "protocolVersion", alias = "ProtocolVersion", default)]
    pub protocol_version: u16,
    /// Packet types this device knows how to receive.
    #[serde(rename = "incomingCapabilities", default)]
    pub incoming: Vec<String>,
    /// Packet types this device knows how to send.
    #[serde(rename = "outgoingCapabilities", default)]
    pub outgoing: Vec<String>,
    /// TCP port this device's control listener is bound to; `0` when unknown
    /// (very old peers). Upstream reads `tcpPort` and ignores values outside
    /// 1716..=1764, so the daemon binds `DEFAULT_TCP_PORT` and advertises it.
    #[serde(rename = "tcpPort", alias = "tcpSourcePort", default)]
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
