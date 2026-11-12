//! TLS certificate material and fingerprints.
//!
//! Handfast pins self-signed certificates whose common name is the device ID.
//! Certificate generation and the rustls verifier land in P1 with the link
//! layer. This module owns the stored material and the stable fingerprint.

use sha2::{Digest, Sha256};

use crate::error::{ProtoError, Result};

/// A self-signed identity certificate and its private key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsIdentity {
    /// DER-encoded certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded private key.
    pub key_der: Vec<u8>,
    /// SHA-256 of `cert_der`.
    pub fingerprint: [u8; 32],
}

impl TlsIdentity {
    /// Rebuild identity material from stored DER bytes.
    ///
    /// # Errors
    ///
    /// Fails when either byte string is empty.
    pub fn from_parts(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<Self> {
        if cert_der.is_empty() || key_der.is_empty() {
            return Err(ProtoError::Certificate(
                "certificate and key must not be empty".to_string(),
            ));
        }
        let fingerprint = fingerprint(&cert_der);
        Ok(Self {
            cert_der,
            key_der,
            fingerprint,
        })
    }

    /// Hex representation of the certificate fingerprint.
    #[must_use]
    pub fn fingerprint_hex(&self) -> String {
        hex(&self.fingerprint)
    }
}

/// SHA-256 fingerprint of a DER-encoded certificate.
#[must_use]
pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(cert_der);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Lowercase hexadecimal encoding.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_recomputes_fingerprint() {
        let identity = TlsIdentity::from_parts(vec![1, 2, 3], vec![4, 5, 6]).unwrap();
        assert_eq!(identity.fingerprint, fingerprint(&identity.cert_der));
        assert_eq!(identity.fingerprint_hex().len(), 64);
    }

    #[test]
    fn empty_parts_are_rejected() {
        assert!(TlsIdentity::from_parts(Vec::new(), vec![1]).is_err());
        assert!(TlsIdentity::from_parts(vec![1], Vec::new()).is_err());
    }

    #[test]
    fn fingerprints_are_stable() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
    }
}
