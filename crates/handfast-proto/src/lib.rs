//! KDE Connect wire protocol for Handfast v2.
//!
//! This crate is deliberately transport-free. It owns packet framing, identity,
//! capability negotiation, pairing bodies and TLS certificate material. Network
//! IO lives in `handfast-net`.

#![deny(missing_docs)]

pub mod cert;
pub mod error;
pub mod frame;
pub mod identity;
pub mod packet;
pub mod types;

pub use cert::TlsIdentity;
pub use error::{ProtoError, Result};
pub use frame::{decode, encode, read_frame, write_frame, MAX_FRAME_SIZE};
pub use identity::{
    Capabilities, DeviceId, DeviceName, DeviceType, Identity, PairingRequest, PROTOCOL_VERSION,
};
pub use packet::{Packet, PayloadTransferInfo};
