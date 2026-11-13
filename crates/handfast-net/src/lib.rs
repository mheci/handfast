//! Discovery, TCP links and pairing for Handfast.
//!
//! The rustls configuration for both link roles lives here. The plaintext
//! hello, the identity re-exchange and the framed link are not implemented
//! yet.

#![deny(missing_docs)]

mod identity;
// The link unit is the production caller; until it lands, only the TLS-only
// tests build these configs.
#[allow(dead_code)]
mod tls;

pub mod discovery;
pub mod error;
pub mod link;
pub mod pairing;

pub use discovery::{Discovery, DISCOVERY_PORT};
pub use error::{NetError, Result, TlsError};
pub use identity::{Fingerprint, LocalEndpoint, PeerCertificate, PeerTrust, TlsIdentity};
pub use link::Link;
pub use pairing::{PairingAction, PairingSession, PairingState, PAIRING_TIMEOUT_SECS};
