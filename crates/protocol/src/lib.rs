#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! `handfast-protocol`: types and codecs for the KDE Connect network protocol.
//!
//! Handfast speaks the upstream KDE Connect protocol so it can pair and
//! interoperate with KDE Connect desktop clients and the Android app.
//!
//! # Discovery (UDP)
//!
//! A device announces itself by broadcasting a single frame of type
//! [`TYPE_IDENTITY`] to UDP port [`UDP_BROADCAST_PORT`]. The identity payload
//! ([`Identity`]) carries the device id, human-readable name, form factor,
//! protocol version, advertised capabilities and the TCP port the device
//! listens on for incoming connections.
//!
//! # Transport and framing (TCP + TLS)
//!
//! Peers connect to [`DEFAULT_TCP_PORT`] and immediately upgrade the socket to
//! TLS; both sides present self-signed device certificates (see [`tls`]) and
//! authenticate by comparing SHA-256 certificate fingerprints
//! (trust-on-first-use). Inside TLS every packet is one line of compact JSON
//! terminated by `\n`, exactly as upstream KDE Connect serializes packets:
//!
//! ```json
//! {"id":42,"type":"kdeconnect.ping","body":{}}
//! ```
//!
//! A frame longer than [`MAX_PACKET_LEN`] is rejected before more data is
//! buffered. File transfers do not send bytes inside these packets: the
//! sender announces `payloadSize` + `payloadTransferInfo` (`{"port": N}`),
//! the receiver dials the sender on that port, both wrap the socket in TLS
//! (receiver = TLS client, sender = TLS server), and exactly `payloadSize`
//! raw bytes stream over it — see [`PAYLOAD_TRANSFER_MIN_PORT`].
//!
//! # Capabilities
//!
//! `incomingCapabilities`/`outgoingCapabilities` gate which plugins may talk
//! to a peer; see [`Identity::supports_incoming`] and
//! [`Identity::supports_outgoing`].
//!
//! # Divergences from upstream
//!
//! * Device certificates are ECDSA P-256 (the `rcgen` default) instead of
//!   upstream RSA-2048. Modern TLS stacks accept either, and pairing security
//!   comes from fingerprint verification rather than a CA chain. See [`tls`].
//! * [`TYPE_PAIR_REQUEST`] is retained purely as a documentation alias for the
//!   legacy pairing request type; modern peers only use [`TYPE_PAIR`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod error;
pub mod identity;
pub mod packet;
pub mod tls;
pub mod transfer;

pub use error::{Error, Result};
pub use identity::Identity;
pub use packet::Packet;

/// Upstream KDE Connect protocol version we speak.
pub const PROTO_VERSION: u16 = 8;

/// TCP port KDE Connect listens on for incoming TLS connections.
pub const DEFAULT_TCP_PORT: u16 = 1716;

/// UDP port used for LAN discovery broadcasts.
pub const UDP_BROADCAST_PORT: u16 = 1716;

/// Hard cap on a single framed packet (upstream allows large payloads for shares; 512 KiB covers control packets; transfers stream raw TLS separately).
pub const MAX_PACKET_LEN: usize = 512 * 1024;

/// Identity announcement broadcast during discovery.
pub const TYPE_IDENTITY: &str = "kdeconnect.identity";

/// Pairing accept/reject negotiation.
pub const TYPE_PAIR: &str = "kdeconnect.pair";

/// Legacy alias for the pre-capability-era pairing request; kept for compat
/// documentation only.
pub const TYPE_PAIR_REQUEST: &str = "kdeconnect.pairingrequest";

/// Liveness probe.
pub const TYPE_PING: &str = "kdeconnect.ping";

/// Notification posted on (or mirrored from) a peer.
pub const TYPE_NOTIFICATION: &str = "kdeconnect.notification";

/// Request a peer's notifications or revoke one.
pub const TYPE_NOTIFICATION_REQUEST: &str = "kdeconnect.notification.request";

/// Remote input events (mouse movement, clicks, keyboard text).
pub const TYPE_MOUSEPAD: &str = "kdeconnect.mousepad.request";

/// Media player state reports from either side.
pub const TYPE_MPRIS: &str = "kdeconnect.mpris";

/// Query or command remote media players.
pub const TYPE_MPRIS_REQUEST: &str = "kdeconnect.mpris.request";

/// Synchronized clipboard content.
pub const TYPE_CLIPBOARD: &str = "kdeconnect.clipboard";

/// Advertises whether a peer participates in clipboard sync.
pub const TYPE_CLIPBOARD_CONNECT: &str = "kdeconnect.clipboard.connect";

/// Share text, URLs or file-transfer metadata.
pub const TYPE_SHARE: &str = "kdeconnect.share.request";

/// Composite-transfer progress totals sent alongside [`TYPE_SHARE`].
///
/// The sender uses this to announce how many files and how many payload
/// bytes a batch of transfers will carry; receivers may use it for UI
/// preallocation but must not require it (single-file sends omit it).
pub const TYPE_SHARE_UPDATE: &str = "kdeconnect.share.request.update";

/// Lowest port the *sender* of a file transfer listens on for the payload
/// channel (mirrors `LanLinkProvider.PAYLOAD_TRANSFER_MIN_PORT` upstream).
///
/// Senders bind the first free port at or above this value and announce it
/// in the packet's `payloadTransferInfo`; the receiver dials it back.
pub const PAYLOAD_TRANSFER_MIN_PORT: u16 = 1739;

/// Lists commands runnable on a peer.
pub const TYPE_RUNCOMMAND: &str = "kdeconnect.runcommand";

/// Requests execution of a previously advertised command.
pub const TYPE_RUNCOMMAND_REQUEST: &str = "kdeconnect.runcommand.request";

/// Battery status report.
pub const TYPE_BATTERY: &str = "kdeconnect.battery";

/// Requests a battery status update.
pub const TYPE_BATTERY_REQUEST: &str = "kdeconnect.battery.request";

/// Exposes an SSH/SFTP browse endpoint for the device filesystem.
pub const TYPE_SFTP: &str = "kdeconnect.sftp";

/// Telephony events (rings, SMS) between phone and desktop.
pub const TYPE_TELEPHONY: &str = "kdeconnect.telephony";

/// System volume reports and controls.
pub const TYPE_SYSTEMVOLUME: &str = "kdeconnect.systemvolume";

/// Rings a lost phone at full volume.
pub const TYPE_FINDMYPHONE: &str = "kdeconnect.findmyphone.request";
