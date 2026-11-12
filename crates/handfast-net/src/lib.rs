//! Discovery, TCP links and pairing for Handfast.
//!
//! The TLS handshake is not implemented in this crate yet. The certificate
//! material and the peer trust policy are pure and tested without a socket.

#![deny(missing_docs)]

mod identity;

pub mod discovery;
pub mod error;
pub mod link;
pub mod pairing;

pub use discovery::{Discovery, DISCOVERY_PORT};
pub use error::{NetError, Result, TlsError};
pub use identity::{Fingerprint, LocalEndpoint, PeerCertificate, PeerTrust, TlsIdentity};
pub use link::Link;
pub use pairing::{PairingAction, PairingSession, PairingState, PAIRING_TIMEOUT_SECS};
