//! Discovery, TCP links and pairing for Handfast.
//!
//! TLS is deliberately not implemented in this crate yet. The link layer here
//! establishes the frame contract that the TLS layer will wrap in Phase 1.

#![deny(missing_docs)]

pub mod discovery;
pub mod error;
pub mod link;
pub mod pairing;

pub use discovery::{Discovery, DISCOVERY_PORT};
pub use error::{NetError, Result};
pub use link::Link;
pub use pairing::{PairingAction, PairingSession, PairingState, PAIRING_TIMEOUT_SECS};
