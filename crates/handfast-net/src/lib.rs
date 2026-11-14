//! Discovery, TCP links, pairing and payload transfers for Handfast.
//!
//! The link layer owns the plaintext hello, both TLS roles, the certificate
//! and identity checks, the framed packet stream, and the manager that admits
//! and supervises links. Discovery is unauthenticated; the encrypted identity
//! exchange is authoritative. The payload layer moves share bytes over a
//! separate TLS session with the certificate the control link authenticated.

#![deny(missing_docs)]

mod identity;
mod payload;
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
    ShareEvent, SHARE_EVENT_QUEUE,
};
pub use pairing::{PairingAction, PairingSession, PairingState, PAIRING_TIMEOUT_SECS};
pub use payload::{
    PayloadError, PayloadLimits, PayloadPeer, PayloadService, PayloadSource, PendingPayload,
    PreparedSend, TransferOutcome,
};
pub use policy::{AdmissionPermit, LinkPolicy};
