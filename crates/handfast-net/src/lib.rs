//! Discovery, TCP links and pairing for Handfast.
//!
//! The link layer owns the plaintext hello, both TLS roles, the certificate
//! and identity checks, the framed packet stream, and the manager that admits
//! and supervises links. Discovery is unauthenticated; the encrypted identity
//! exchange is authoritative.

#![deny(missing_docs)]

mod identity;
mod tls;

pub mod discovery;
pub mod error;
pub mod link;
pub mod manager;
pub mod pairing;
pub mod policy;

pub use discovery::{Discovery, DISCOVERY_PORT};
pub use error::{NetError, Result, TlsError};
pub use identity::{
    Fingerprint, LocalEndpoint, PeerCertificate, PeerTrust, TlsIdentity, TrustTable,
};
pub use link::{connect, Link, LinkLimits, LinkListener, LinkReader, LinkWriter, LinkedPeer};
pub use manager::{
    is_unpaired_allowed, LinkEvent, LinkHandle, LinkManager, LinkManagerHandle, PendingLink,
};
pub use pairing::{PairingAction, PairingSession, PairingState, PAIRING_TIMEOUT_SECS};
pub use policy::{AdmissionPermit, LinkPolicy};
