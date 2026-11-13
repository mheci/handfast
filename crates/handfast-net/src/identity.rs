//! Certificate fingerprints and the peer trust policy.
//!
//! The policy here is pure. After the TLS handshake the link layer parses the
//! peer certificate, checks its common name and pin against the expected
//! device, and only then exchanges the encrypted identity. The store owns the
//! pins; this crate never depends on it.

use std::collections::HashMap;
use std::sync::Arc;

use handfast_proto::{DeviceId, Identity, PROTOCOL_VERSION};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use zeroize::Zeroizing;

use crate::error::TlsError;

/// Days of validity before generation. Android uses one year.
const VALIDITY_PAST_DAYS: i64 = 365;
/// Days of validity after generation. Android uses ten years.
const VALIDITY_FUTURE_DAYS: i64 = 3650;

/// SHA-256 of a full DER certificate.
///
/// The fingerprint is public. It is the only value the trust table stores for
/// a peer, and the pairing prompt shows it as lowercase hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Hash a DER certificate with SHA-256.
    #[must_use]
    pub fn of_certificate(der: &[u8]) -> Self {
        let digest = Sha256::digest(der);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_slice());
        Self(bytes)
    }

    /// Wrap an existing SHA-256 digest, as loaded from the store.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, 64 characters.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;

        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// The local TLS material: certificate, private key and device ID.
///
/// There is deliberately no `Debug` or `Display` implementation. The private
/// key is PKCS#8 DER and must never reach a log.
///
/// ```compile_fail
/// fn assert_debug<T: std::fmt::Debug>() {}
/// assert_debug::<handfast_net::TlsIdentity>();
/// ```
///
/// ```compile_fail
/// fn assert_display<T: std::fmt::Display>() {}
/// assert_display::<handfast_net::TlsIdentity>();
/// ```
pub struct TlsIdentity {
    device_id: DeviceId,
    cert_der: Vec<u8>,
    key_der: Zeroizing<Vec<u8>>,
    fingerprint: Fingerprint,
}

impl TlsIdentity {
    /// Generate an ECDSA P-256 key and a self-signed certificate whose single
    /// common name is the device ID.
    ///
    /// The validity window matches Android: one year back, ten years ahead.
    ///
    /// # Errors
    ///
    /// Fails when the provider cannot generate the key or the certificate.
    pub fn generate(device_id: DeviceId) -> Result<Self, TlsError> {
        let key_pair = KeyPair::generate()
            .map_err(|error| TlsError::Certificate(format!("key generation: {error}")))?;

        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, device_id.as_str());
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(VALIDITY_PAST_DAYS);
        params.not_after = now + Duration::days(VALIDITY_FUTURE_DAYS);

        let certificate = params
            .self_signed(&key_pair)
            .map_err(|error| TlsError::Certificate(format!("certificate: {error}")))?;

        Self::from_parts(
            device_id,
            certificate.der().as_ref().to_vec(),
            key_pair.serialize_der(),
        )
    }

    /// Load persisted DER material and validate it.
    ///
    /// The certificate must parse with exactly one common name equal to
    /// `device_id`, and the private key must match the certificate public key.
    /// A stored identity is never silently regenerated.
    ///
    /// # Errors
    ///
    /// Fails on empty, malformed or mismatched certificate or key bytes, or a
    /// common name that is not `device_id`.
    pub fn from_parts(
        device_id: DeviceId,
        cert_der: Vec<u8>,
        key_der: Vec<u8>,
    ) -> Result<Self, TlsError> {
        if cert_der.is_empty() {
            return Err(TlsError::Certificate("empty certificate".to_string()));
        }
        if key_der.is_empty() {
            return Err(TlsError::Certificate("empty private key".to_string()));
        }
        if pkcs8_der_len(&key_der) != Some(key_der.len()) {
            return Err(TlsError::Certificate(
                "private key is not one PKCS#8 DER sequence".to_string(),
            ));
        }

        let parsed = parse_certificate(&cert_der)?;
        if parsed.device_id != device_id {
            return Err(TlsError::CertificateDeviceId {
                expected: device_id,
                got: parsed.device_id,
            });
        }

        let key_pair = KeyPair::try_from(key_der.as_slice())
            .map_err(|error| TlsError::Certificate(format!("private key: {error}")))?;
        if key_pair.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
            return Err(TlsError::Certificate(
                "private key is not ECDSA P-256".to_string(),
            ));
        }
        if key_pair.public_key_raw() != parsed.public_key.as_slice() {
            return Err(TlsError::Certificate(
                "private key does not match certificate".to_string(),
            ));
        }

        let fingerprint = Fingerprint::of_certificate(&cert_der);
        Ok(Self {
            device_id,
            cert_der,
            key_der: Zeroizing::new(key_der),
            fingerprint,
        })
    }

    /// The local device ID the certificate common name carries.
    #[must_use]
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// SHA-256 of the DER certificate.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// The DER certificate.
    #[must_use]
    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// The PKCS#8 DER private key. The bytes are zeroized when this value is
    /// dropped.
    #[must_use]
    pub fn key_der(&self) -> &[u8] {
        self.key_der.as_slice()
    }
}

/// The length of one DER SEQUENCE at the start of `der`, or `None` when the
/// header is malformed. Stored keys must consume the whole record, so trailing
/// bytes are rejected before the key parser sees them.
fn pkcs8_der_len(der: &[u8]) -> Option<usize> {
    if der.len() < 2 || der[0] != 0x30 {
        return None;
    }
    let first = der[1];
    if first < 0x80 {
        return Some(2 + usize::from(first));
    }
    let count = usize::from(first & 0x7f);
    if count == 0 || count > 4 || der.len() < 2 + count {
        return None;
    }
    let mut length = 0usize;
    for byte in &der[2..2 + count] {
        length = (length << 8) | usize::from(*byte);
    }
    length.checked_add(2 + count)
}

/// The local TLS material plus the full local identity.
///
/// The link APIs use one object so the plaintext hello and the encrypted
/// identity cannot disagree about this device.
pub struct LocalEndpoint {
    tls: Arc<TlsIdentity>,
    identity: Identity,
}

impl LocalEndpoint {
    /// Combine the TLS material with the local identity.
    ///
    /// # Errors
    ///
    /// Fails when `identity.device_id` does not match the certificate device ID.
    pub fn new(tls: Arc<TlsIdentity>, identity: Identity) -> Result<Self, TlsError> {
        if &identity.device_id != tls.device_id() {
            return Err(TlsError::CertificateDeviceId {
                expected: tls.device_id().clone(),
                got: identity.device_id,
            });
        }
        Ok(Self { tls, identity })
    }

    /// The certificate and private key.
    #[must_use]
    pub fn tls(&self) -> &Arc<TlsIdentity> {
        &self.tls
    }

    /// The full local identity.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The local device ID.
    #[must_use]
    pub fn device_id(&self) -> &DeviceId {
        &self.identity.device_id
    }
}

/// A validated peer certificate.
///
/// Construction parses the DER and extracts the device ID from the common
/// name. The raw DER is kept for the pairing SAS and for the duplicate-link
/// comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCertificate {
    device_id: DeviceId,
    fingerprint: Fingerprint,
    der: Vec<u8>,
}

impl PeerCertificate {
    /// Parse exactly one X.509 certificate.
    ///
    /// The subject must contain exactly one common name that is a valid device
    /// ID. Trailing bytes after the certificate are rejected.
    ///
    /// # Errors
    ///
    /// Fails on corrupt DER, trailing bytes, a missing, duplicate or empty
    /// common name, or a common name that is not a valid device ID.
    pub fn parse(der: &[u8]) -> Result<Self, TlsError> {
        let parsed = parse_certificate(der)?;
        Ok(Self {
            device_id: parsed.device_id,
            fingerprint: Fingerprint::of_certificate(der),
            der: der.to_vec(),
        })
    }

    /// The device ID from the certificate common name.
    #[must_use]
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// SHA-256 of the full DER certificate.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// The full DER certificate.
    #[must_use]
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The one certificate trust policy.
    ///
    /// The common name must equal `expected`. For [`PeerTrust::Pinned`] the
    /// certificate fingerprint must equal the pin. [`PeerTrust::Unpaired`]
    /// compares no pin, so a missing pin is never treated as trust here.
    ///
    /// # Errors
    ///
    /// Fails with [`TlsError::CertificateDeviceId`] on a common name mismatch
    /// and [`TlsError::PinMismatch`] when a pin does not match.
    pub fn check(&self, expected: &DeviceId, trust: PeerTrust) -> Result<(), TlsError> {
        if &self.device_id != expected {
            return Err(TlsError::CertificateDeviceId {
                expected: expected.clone(),
                got: self.device_id.clone(),
            });
        }
        if let PeerTrust::Pinned(pin) = trust {
            if pin != self.fingerprint {
                return Err(TlsError::PinMismatch {
                    expected: pin,
                    got: self.fingerprint,
                });
            }
        }
        Ok(())
    }

    /// Correlate the plaintext hello, the certificate common name and the
    /// encrypted identity.
    ///
    /// All three must name the same device and the encrypted identity must be
    /// protocol 8. This is the CVE-2025-66270 guard: a peer cannot claim one
    /// device in the plaintext hello and another after the handshake.
    ///
    /// # Errors
    ///
    /// Fails with [`TlsError::CertificateDeviceId`] when the common name does
    /// not match the hello, [`TlsError::IdentityDeviceId`] when the encrypted
    /// identity does not match the hello, and [`TlsError::ProtocolVersion`]
    /// when the encrypted version is not 8.
    pub fn check_identity(&self, hello: &DeviceId, encrypted: &Identity) -> Result<(), TlsError> {
        if &self.device_id != hello {
            return Err(TlsError::CertificateDeviceId {
                expected: hello.clone(),
                got: self.device_id.clone(),
            });
        }
        if &encrypted.device_id != hello {
            return Err(TlsError::IdentityDeviceId {
                plaintext: hello.clone(),
                encrypted: encrypted.device_id.clone(),
            });
        }
        if encrypted.protocol_version != PROTOCOL_VERSION {
            return Err(TlsError::ProtocolVersion {
                expected: PROTOCOL_VERSION,
                got: encrypted.protocol_version,
            });
        }
        Ok(())
    }
}

/// What the trust table knows about a peer before the certificate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTrust {
    /// No pin is stored. The peer may connect, but gets only the unpaired gate.
    Unpaired,
    /// The certificate fingerprint the user accepted.
    Pinned(Fingerprint),
}

/// The in-memory pin snapshot the link layer checks.
///
/// The store owns durable pins. The daemon loads them once at startup and is
/// the only writer of this table and the store, so the two cannot diverge. A
/// clone is a snapshot: a handshake task keeps checking the pins it started
/// with even if the table changes.
#[derive(Debug, Clone, Default)]
pub struct TrustTable(Arc<HashMap<DeviceId, Fingerprint>>);

impl TrustTable {
    /// The trust state for one peer. A missing pin is never trust.
    #[must_use]
    pub fn trust(&self, id: &DeviceId) -> PeerTrust {
        self.0
            .get(id)
            .copied()
            .map_or(PeerTrust::Unpaired, PeerTrust::Pinned)
    }

    /// Pin a peer certificate.
    pub fn set(&mut self, id: DeviceId, pin: Fingerprint) {
        Arc::make_mut(&mut self.0).insert(id, pin);
    }

    /// Remove a peer pin.
    pub fn remove(&mut self, id: &DeviceId) {
        Arc::make_mut(&mut self.0).remove(id);
    }
}

struct ParsedCertificate {
    device_id: DeviceId,
    public_key: Vec<u8>,
}

fn parse_certificate(der: &[u8]) -> Result<ParsedCertificate, TlsError> {
    let (rest, certificate) = x509_parser::parse_x509_certificate(der)
        .map_err(|error| TlsError::Certificate(format!("malformed certificate: {error:?}")))?;
    if !rest.is_empty() {
        return Err(TlsError::Certificate(
            "trailing data after certificate".to_string(),
        ));
    }

    let mut common_names = certificate.subject().iter_common_name();
    let Some(common_name) = common_names.next() else {
        return Err(TlsError::Certificate(
            "certificate has no common name".to_string(),
        ));
    };
    if common_names.next().is_some() {
        return Err(TlsError::Certificate(
            "certificate has more than one common name".to_string(),
        ));
    }
    let common_name = common_name.as_str().map_err(|_| {
        TlsError::Certificate("certificate common name is not a UTF-8 string".to_string())
    })?;
    if common_name.is_empty() {
        return Err(TlsError::Certificate(
            "certificate common name is empty".to_string(),
        ));
    }
    let device_id = DeviceId::new(common_name)
        .map_err(|error| TlsError::Certificate(format!("certificate common name: {error}")))?;

    Ok(ParsedCertificate {
        device_id,
        public_key: certificate
            .tbs_certificate
            .subject_pki
            .subject_public_key
            .data
            .to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{Capabilities, DeviceName, DeviceType};
    use rcgen::{BasicConstraints, IsCa, Issuer};
    use sha2::{Digest, Sha256};
    use x509_parser::parse_x509_certificate;

    const DEVICE_A: &str = "0123456789abcdef0123456789abcdef";
    const DEVICE_B: &str = "fedcba9876543210fedcba9876543210";
    const DEVICE_C: &str = "00112233445566778899aabbccddeeff";

    fn device_id(value: &str) -> DeviceId {
        DeviceId::new(value).expect("the test device ID is valid")
    }

    fn identity(value: &str) -> Identity {
        Identity::new(
            device_id(value),
            DeviceName::new("Test device"),
            DeviceType::Desktop,
            Capabilities::new([handfast_proto::types::PING], Vec::<String>::new())
                .expect("the test capabilities are valid"),
            Some(1716),
        )
    }

    fn certificate_with(common_names: &[&str]) -> Vec<u8> {
        let key_pair = KeyPair::generate().expect("ring generates an ECDSA P-256 key");
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        for common_name in common_names {
            params
                .distinguished_name
                .push(DnType::CommonName, *common_name);
        }
        params
            .self_signed(&key_pair)
            .expect("ring signs the certificate")
            .der()
            .as_ref()
            .to_vec()
    }

    fn certificate_without_common_name() -> Vec<u8> {
        let key_pair = KeyPair::generate().expect("ring generates an ECDSA P-256 key");
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::OrganizationName, "No common name");
        params
            .self_signed(&key_pair)
            .expect("ring signs the certificate")
            .der()
            .as_ref()
            .to_vec()
    }

    fn certificate_with_foreign_issuer_and_expired_window(common_name: &str) -> Vec<u8> {
        let issuer_key = KeyPair::generate().expect("ring generates an ECDSA P-256 key");
        let mut issuer_params = CertificateParams::default();
        issuer_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        issuer_params.distinguished_name = DistinguishedName::new();
        issuer_params
            .distinguished_name
            .push(DnType::CommonName, "Test issuer");
        let issuer = Issuer::from_params(&issuer_params, &issuer_key);

        let key_pair = KeyPair::generate().expect("ring generates an ECDSA P-256 key");
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.not_before = OffsetDateTime::now_utc() - Duration::days(3650);
        params.not_after = OffsetDateTime::now_utc() - Duration::days(365);
        params
            .signed_by(&key_pair, &issuer)
            .expect("ring signs the certificate")
            .der()
            .as_ref()
            .to_vec()
    }

    #[test]
    fn fingerprint_matches_known_sha256_vector() {
        let fingerprint = Fingerprint::of_certificate(b"abc");
        assert_eq!(
            fingerprint.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(fingerprint, Fingerprint::from_bytes(fingerprint.0));
    }

    #[test]
    fn generated_fingerprint_is_sha256_of_der() {
        let identity = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        let digest = Sha256::digest(identity.cert_der());
        assert_eq!(&identity.fingerprint().as_bytes()[..], digest.as_slice());
        assert_eq!(
            identity.fingerprint(),
            Fingerprint::of_certificate(identity.cert_der())
        );
    }

    #[test]
    fn fingerprint_hex_is_64_lowercase_characters() {
        let identity = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        let hex = identity.fingerprint().to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert_eq!(hex, identity.fingerprint().to_string());
    }

    #[test]
    fn generated_certificate_has_device_id_common_name_and_android_window() {
        let identity = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        let (rest, certificate) =
            parse_x509_certificate(identity.cert_der()).expect("the generated certificate parses");
        assert!(rest.is_empty());

        let mut common_names = certificate.subject().iter_common_name();
        let common_name = common_names
            .next()
            .expect("the certificate has a common name")
            .as_str()
            .expect("the common name is a string");
        assert_eq!(common_name, DEVICE_A);
        assert!(common_names.next().is_none(), "exactly one common name");

        let issuer_common_name = certificate
            .issuer()
            .iter_common_name()
            .next()
            .and_then(|name| name.as_str().ok());
        let subject_common_name = certificate
            .subject()
            .iter_common_name()
            .next()
            .and_then(|name| name.as_str().ok());
        assert_eq!(
            issuer_common_name, subject_common_name,
            "the generated certificate is self-issued"
        );

        let now = OffsetDateTime::now_utc().unix_timestamp();
        let not_before = certificate.validity().not_before.timestamp();
        let not_after = certificate.validity().not_after.timestamp();
        let day = 24 * 60 * 60;
        assert!((now - not_before - VALIDITY_PAST_DAYS * day).abs() < 2 * day);
        assert!((not_after - now - VALIDITY_FUTURE_DAYS * day).abs() < 2 * day);
    }

    #[test]
    fn from_parts_round_trips_a_generated_identity() {
        let generated = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        let loaded = TlsIdentity::from_parts(
            device_id(DEVICE_A),
            generated.cert_der().to_vec(),
            generated.key_der().to_vec(),
        )
        .expect("the generated material loads");

        assert_eq!(loaded.device_id(), generated.device_id());
        assert_eq!(loaded.fingerprint(), generated.fingerprint());
        assert_eq!(loaded.cert_der(), generated.cert_der());
        assert_eq!(loaded.key_der(), generated.key_der());
    }

    #[test]
    fn from_parts_rejects_empty_material() {
        let generated = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");

        assert!(matches!(
            TlsIdentity::from_parts(
                device_id(DEVICE_A),
                Vec::new(),
                generated.key_der().to_vec()
            ),
            Err(TlsError::Certificate(_))
        ));
        assert!(matches!(
            TlsIdentity::from_parts(
                device_id(DEVICE_A),
                generated.cert_der().to_vec(),
                Vec::new()
            ),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn from_parts_rejects_malformed_certificate() {
        let generated = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        assert!(matches!(
            TlsIdentity::from_parts(
                device_id(DEVICE_A),
                b"not a certificate".to_vec(),
                generated.key_der().to_vec()
            ),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn from_parts_rejects_certificate_device_id_mismatch() {
        let generated = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        assert!(matches!(
            TlsIdentity::from_parts(
                device_id(DEVICE_B),
                generated.cert_der().to_vec(),
                generated.key_der().to_vec()
            ),
            Err(TlsError::CertificateDeviceId { .. })
        ));
    }

    #[test]
    fn from_parts_rejects_malformed_key() {
        let generated = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        assert!(matches!(
            TlsIdentity::from_parts(
                device_id(DEVICE_A),
                generated.cert_der().to_vec(),
                b"not a key".to_vec()
            ),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn from_parts_rejects_key_that_does_not_match_certificate() {
        let first = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate first identity");
        let second = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate second identity");
        assert!(matches!(
            TlsIdentity::from_parts(
                device_id(DEVICE_A),
                first.cert_der().to_vec(),
                second.key_der().to_vec()
            ),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn from_parts_rejects_a_non_p256_key() {
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)
            .expect("ring generates an ECDSA P-384 key");
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, DEVICE_A);
        let cert_der = params
            .self_signed(&key_pair)
            .expect("ring signs the P-384 certificate")
            .der()
            .as_ref()
            .to_vec();

        assert!(matches!(
            TlsIdentity::from_parts(device_id(DEVICE_A), cert_der, key_pair.serialize_der()),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn from_parts_rejects_trailing_key_bytes() {
        let generated = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        let mut key_der = generated.key_der().to_vec();
        key_der.extend_from_slice(&[0, 1, 2]);
        assert!(matches!(
            TlsIdentity::from_parts(device_id(DEVICE_A), generated.cert_der().to_vec(), key_der),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn local_endpoint_carries_the_full_identity() {
        let tls = Arc::new(TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity"));
        let local_identity = identity(DEVICE_A);
        let endpoint =
            LocalEndpoint::new(Arc::clone(&tls), local_identity.clone()).expect("matching IDs");

        assert_eq!(endpoint.device_id(), tls.device_id());
        assert_eq!(endpoint.identity(), &local_identity);
        assert_eq!(endpoint.tls().fingerprint(), tls.fingerprint());
    }

    #[test]
    fn local_endpoint_rejects_certificate_device_id_mismatch() {
        let tls = Arc::new(TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity"));
        assert!(matches!(
            LocalEndpoint::new(tls, identity(DEVICE_B)),
            Err(TlsError::CertificateDeviceId { .. })
        ));
    }

    #[test]
    fn local_endpoint_round_trips_generate_and_load() {
        let first_tls =
            Arc::new(TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity"));
        let first = LocalEndpoint::new(first_tls, identity(DEVICE_A)).expect("matching IDs");

        let loaded = TlsIdentity::from_parts(
            device_id(DEVICE_A),
            first.tls().cert_der().to_vec(),
            first.tls().key_der().to_vec(),
        )
        .expect("the persisted material loads");
        let second =
            LocalEndpoint::new(Arc::new(loaded), identity(DEVICE_A)).expect("matching IDs");

        assert_eq!(first.device_id(), second.device_id());
        assert_eq!(first.tls().fingerprint(), second.tls().fingerprint());
        assert_eq!(first.identity(), second.identity());
        assert_eq!(first.tls().cert_der(), second.tls().cert_der());
    }

    #[test]
    fn peer_certificate_parse_accepts_a_generated_certificate() {
        let generated = TlsIdentity::generate(device_id(DEVICE_A)).expect("generate identity");
        let certificate =
            PeerCertificate::parse(generated.cert_der()).expect("the certificate parses");

        assert_eq!(certificate.device_id(), generated.device_id());
        assert_eq!(certificate.fingerprint(), generated.fingerprint());
        assert_eq!(certificate.der(), generated.cert_der());
    }

    #[test]
    fn peer_certificate_parse_rejects_corrupt_der() {
        assert!(matches!(
            PeerCertificate::parse(b"not a certificate"),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn peer_certificate_parse_rejects_trailing_der() {
        let mut der = certificate_with(&[DEVICE_A]);
        der.push(0);
        assert!(matches!(
            PeerCertificate::parse(&der),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn peer_certificate_parse_rejects_missing_common_name() {
        assert!(matches!(
            PeerCertificate::parse(&certificate_without_common_name()),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn peer_certificate_parse_rejects_multiple_common_names() {
        // A fixed DER certificate with two CN RDNs. rcgen's distinguished name
        // map cannot express duplicates.
        let der = include_bytes!("../tests/fixtures/multiple-common-names.der");
        let error = PeerCertificate::parse(der).expect_err("two common names are rejected");
        assert!(
            error.to_string().contains("more than one common name"),
            "{error}"
        );
    }

    #[test]
    fn peer_certificate_parse_rejects_empty_common_name() {
        assert!(matches!(
            PeerCertificate::parse(&certificate_with(&[""])),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn peer_certificate_parse_rejects_invalid_device_id_common_name() {
        assert!(matches!(
            PeerCertificate::parse(&certificate_with(&["short"])),
            Err(TlsError::Certificate(_))
        ));
    }

    #[test]
    fn peer_check_accepts_unpaired_peer_with_matching_common_name() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        assert!(certificate
            .check(&device_id(DEVICE_A), PeerTrust::Unpaired)
            .is_ok());
    }

    #[test]
    fn peer_check_rejects_unpaired_peer_with_different_common_name() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        assert!(matches!(
            certificate.check(&device_id(DEVICE_B), PeerTrust::Unpaired),
            Err(TlsError::CertificateDeviceId { expected, got })
                if expected == device_id(DEVICE_B) && got == device_id(DEVICE_A)
        ));
    }

    #[test]
    fn peer_check_accepts_pinned_peer_with_matching_fingerprint() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        assert!(certificate
            .check(
                &device_id(DEVICE_A),
                PeerTrust::Pinned(certificate.fingerprint())
            )
            .is_ok());
    }

    #[test]
    fn peer_check_rejects_pinned_peer_whose_certificate_changed() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        let stored = PeerCertificate::parse(&certificate_with(&[DEVICE_A]))
            .expect("the stored certificate parses");

        assert!(matches!(
            certificate.check(&device_id(DEVICE_A), PeerTrust::Pinned(stored.fingerprint())),
            Err(TlsError::PinMismatch { expected, got })
                if expected == stored.fingerprint() && got == certificate.fingerprint()
        ));
        assert!(certificate
            .check(
                &device_id(DEVICE_A),
                PeerTrust::Pinned(certificate.fingerprint())
            )
            .is_ok());
    }

    #[test]
    fn peer_check_ignores_issuer_and_validity() {
        let certificate = PeerCertificate::parse(
            &certificate_with_foreign_issuer_and_expired_window(DEVICE_A),
        )
        .expect("the certificate parses");
        assert!(certificate
            .check(&device_id(DEVICE_A), PeerTrust::Unpaired)
            .is_ok());
    }

    #[test]
    fn identity_correlation_accepts_matching_ids() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        let encrypted = identity(DEVICE_A);
        assert!(certificate
            .check_identity(&device_id(DEVICE_A), &encrypted)
            .is_ok());
    }

    #[test]
    fn identity_correlation_rejects_certificate_common_name_mismatch() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        let encrypted = identity(DEVICE_B);
        assert!(matches!(
            certificate.check_identity(&device_id(DEVICE_B), &encrypted),
            Err(TlsError::CertificateDeviceId { .. })
        ));
    }

    #[test]
    fn identity_correlation_rejects_encrypted_identity_mismatch() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        let encrypted = identity(DEVICE_B);
        assert!(matches!(
            certificate.check_identity(&device_id(DEVICE_A), &encrypted),
            Err(TlsError::IdentityDeviceId { plaintext, encrypted })
                if plaintext == device_id(DEVICE_A) && encrypted == device_id(DEVICE_B)
        ));
    }

    #[test]
    fn identity_correlation_rejects_three_different_ids() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        let encrypted = identity(DEVICE_C);
        assert!(matches!(
            certificate.check_identity(&device_id(DEVICE_B), &encrypted),
            Err(TlsError::CertificateDeviceId { .. })
        ));
    }

    #[test]
    fn identity_correlation_rejects_wrong_protocol_version() {
        let certificate =
            PeerCertificate::parse(&certificate_with(&[DEVICE_A])).expect("the certificate parses");
        let mut encrypted = identity(DEVICE_A);
        encrypted.protocol_version = 7;
        assert!(matches!(
            certificate.check_identity(&device_id(DEVICE_A), &encrypted),
            Err(TlsError::ProtocolVersion {
                expected: 8,
                got: 7
            })
        ));
    }

    #[test]
    fn trust_table_starts_unpaired_and_pins_a_peer() {
        let mut trust = TrustTable::default();
        let id = device_id(DEVICE_A);
        assert_eq!(trust.trust(&id), PeerTrust::Unpaired);

        let pin = Fingerprint::of_certificate(b"certificate");
        trust.set(id.clone(), pin);
        assert_eq!(trust.trust(&id), PeerTrust::Pinned(pin));

        trust.remove(&id);
        assert_eq!(trust.trust(&id), PeerTrust::Unpaired);
    }

    #[test]
    fn trust_table_clone_is_a_snapshot() {
        let mut trust = TrustTable::default();
        let snapshot = trust.clone();
        let id = device_id(DEVICE_A);
        trust.set(id.clone(), Fingerprint::of_certificate(b"certificate"));

        assert_eq!(
            trust.trust(&id),
            PeerTrust::Pinned(Fingerprint::of_certificate(b"certificate"))
        );
        assert_eq!(
            snapshot.trust(&id),
            PeerTrust::Unpaired,
            "a handshake keeps the pins it started with"
        );
    }
}
