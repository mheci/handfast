//! Network errors.

use handfast_proto::{DeviceId, ProtoError};
use thiserror::Error;

use crate::identity::Fingerprint;

/// Errors from discovery, links and pairing.
#[derive(Debug, Error)]
pub enum NetError {
    /// IO failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Protocol parsing failed.
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),

    /// JSON decoding failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Discovery setup or packet handling failed.
    #[error("discovery: {0}")]
    Discovery(String),

    /// Pairing state machine rejected a transition.
    #[error("pairing: {0}")]
    Pairing(String),

    /// TLS configuration, certificate or identity policy failed.
    #[error("tls: {0}")]
    Tls(#[from] TlsError),

    /// A handshake phase did not finish in time.
    #[error("timeout: {0}")]
    Timeout(&'static str),

    /// The peer closed before the handshake finished.
    #[error("link closed")]
    LinkClosed,

    /// The admission policy rejected a link before the TLS handshake.
    #[error("admission: {0}")]
    Admission(String),
}

/// Errors from the certificate and peer trust policy.
///
/// These are typed rather than stringly because the daemon logs them as
/// security events and the tests assert on the exact failure.
#[derive(Debug, Error)]
pub enum TlsError {
    /// The certificate or key bytes are malformed or unusable.
    #[error("certificate: {0}")]
    Certificate(String),

    /// The certificate common name is not the expected device ID.
    #[error("certificate device id mismatch: expected {expected}, got {got}")]
    CertificateDeviceId {
        /// Device ID the caller expected.
        expected: DeviceId,
        /// Device ID in the certificate common name.
        got: DeviceId,
    },

    /// The certificate fingerprint is not the pinned fingerprint.
    #[error("certificate pin mismatch: expected {expected}, got {got}")]
    PinMismatch {
        /// Fingerprint in the trust table.
        expected: Fingerprint,
        /// Fingerprint of the presented certificate.
        got: Fingerprint,
    },

    /// The encrypted identity names a different device than the plaintext
    /// hello.
    #[error("identity device id mismatch: plaintext {plaintext}, encrypted {encrypted}")]
    IdentityDeviceId {
        /// Device ID from the plaintext hello.
        plaintext: DeviceId,
        /// Device ID from the encrypted identity.
        encrypted: DeviceId,
    },

    /// The encrypted identity is not protocol version 8.
    #[error("protocol version mismatch: expected {expected}, got {got}")]
    ProtocolVersion {
        /// Protocol version this build requires.
        expected: u8,
        /// Protocol version in the packet.
        got: u8,
    },
}

/// Result alias for network operations.
pub type Result<T> = std::result::Result<T, NetError>;
