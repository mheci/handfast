//! Discovery, TCP links and pairing for Handfast.
//!
//! The link layer owns the plaintext hello, both TLS roles, the certificate
//! and identity checks, and the framed packet stream. Discovery is unauthenticated;
//! the encrypted identity exchange is authoritative.

#![deny(missing_docs)]

mod identity;
mod tls;

pub mod discovery;
pub mod error;
pub mod link;
pub mod pairing;

pub use discovery::{Discovery, DISCOVERY_PORT};
pub use error::{NetError, Result, TlsError};
pub use identity::{Fingerprint, LocalEndpoint, PeerCertificate, PeerTrust, TlsIdentity};
pub use link::{connect, Link, LinkLimits, LinkListener, LinkReader, LinkWriter, LinkedPeer};
pub use pairing::{PairingAction, PairingSession, PairingState, PAIRING_TIMEOUT_SECS};
