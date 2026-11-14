//! KDE Connect wire protocol for Handfast v2.
//!
//! This crate is deliberately transport-free. It owns packet framing, identity,
//! capability negotiation, pairing bodies, the plaintext connection hello and
//! the UDP discovery announcement. Network IO lives in `handfast-net`.

#![deny(missing_docs)]

pub mod error;
pub mod frame;
pub mod identity;
pub mod packet;
pub mod share;
pub mod types;

pub use error::{ProtoError, Result};
pub use frame::{decode, encode, encode_into, read_frame, write_frame, MAX_FRAME_SIZE};
pub use identity::{
    Capabilities, DeviceId, DeviceName, DeviceType, DiscoveryAnnouncement, Identity, IdentityHello,
    PairingRequest, MAX_TCP_PORT, MIN_TCP_PORT, PROTOCOL_VERSION,
};
pub use packet::{Packet, PayloadTransferInfo};
pub use share::{
    file_request, text_request, update_request, url_request, PayloadOffer, PayloadSize, ShareBatch,
    ShareError, ShareFile, ShareRequest, PAYLOAD_PORT_MAX, PAYLOAD_PORT_MIN,
};
