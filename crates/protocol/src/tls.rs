//! Self-signed device certificates for the TLS transport.
//!
//! # Files on disk
//!
//! [`CertPair::load_or_generate`] works with plain DER files inside a
//! directory: `id_cert.der` (X.509 certificate) and `id_key.der` (PKCS#8
//! private key). Both are written through a same-directory temporary file and
//! renamed into place, so readers never observe torn files; on unix the files
//! are created with mode `0600`. If either file is missing or empty a fresh
//! pair is generated and persisted.
//!
//! # Trust model
//!
//! Handfast uses trust-on-first-use: during pairing each side pins the other's
//! SHA-256 certificate fingerprint ([`cert_fingerprint`]). Subsequent
//! connections verify that the presented certificate still matches the pin.
//!
//! # Divergence from upstream
//!
//! Upstream KDE Connect generates RSA-2048 certificates. We generate ECDSA
//! P-256 self-signed certificates instead (`rcgen` default): they are smaller,
//! faster to produce and accepted by every modern TLS 1.2/1.3 stack. Pairing
//! security rests on fingerprint comparison, not on the signature algorithm or
//! any CA chain, so this is a safe local policy choice.
//!
//! # Fingerprint encoding
//!
//! [`CertPair::fingerprint_hex`] renders fingerprints as plain lowercase hex
//! without separators (64 characters), e.g. `9af4...`. That exact encoding is
//! what Handfast stores in its config/database; do not introduce `:`-separated
//! variants there.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// File name of the DER-encoded device certificate inside the state directory.
const CERT_FILE_NAME: &str = "id_cert.der";
/// File name of the DER-encoded PKCS#8 device key inside the state directory.
const KEY_FILE_NAME: &str = "id_key.der";

/// Device TLS material: leaf certificate, private key and pinned fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertPair {
    /// DER-encoded self-signed X.509 certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded PKCS#8 private key matching `cert_der`.
    pub key_der_pkcs8: Vec<u8>,
    /// SHA-256 digest of [`CertPair::cert_der`]; the pairing identifier peers
    /// pin and compare.
    pub fingerprint_sha256: [u8; 32],
}

impl CertPair {
    /// Loads `id_cert.der`/`id_key.der` from `dir`, or generates and persists a
    /// fresh ECDSA P-256 self-signed pair with `CN = device_id` when either
    /// file is missing or empty.
    ///
    /// On reload the on-disk identity wins: `device_id` is only used for the
    /// freshly generated certificate's Common Name.
    pub fn load_or_generate(dir: &Path, device_id: &str) -> Result<CertPair> {
        let cert_path = dir.join(CERT_FILE_NAME);
        let key_path = dir.join(KEY_FILE_NAME);

        if let (Ok(cert_der), Ok(key_der_pkcs8)) = (fs::read(&cert_path), fs::read(&key_path)) {
            if !cert_der.is_empty() && !key_der_pkcs8.is_empty() {
                let fingerprint_sha256 = cert_fingerprint(&cert_der)?;
                tracing::debug!(dir = %dir.display(), "loaded existing device certificate");
                return Ok(CertPair {
                    cert_der,
                    key_der_pkcs8,
                    fingerprint_sha256,
                });
            }
        }

        tracing::info!(
            dir = %dir.display(),
            device_id,
            "generating new self-signed device certificate"
        );
        let (cert_der, key_der_pkcs8) = generate_self_signed(device_id)?;
        persist_atomic(&cert_path, &cert_der)?;
        persist_atomic(&key_path, &key_der_pkcs8)?;
        let fingerprint_sha256 = cert_fingerprint(&cert_der)?;
        Ok(CertPair {
            cert_der,
            key_der_pkcs8,
            fingerprint_sha256,
        })
    }

    /// Returns the certificate fingerprint as plain lowercase hex without
    /// separators (64 characters).
    pub fn fingerprint_hex(&self) -> String {
        use std::fmt::Write as _;
        let mut hex = String::with_capacity(64);
        for byte in self.fingerprint_sha256 {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// Computes the SHA-256 fingerprint over raw DER certificate bytes.
pub fn cert_fingerprint(cert_der: &[u8]) -> Result<[u8; 32]> {
    let digest = Sha256::digest(cert_der);
    let mut fingerprint = [0u8; 32];
    fingerprint.copy_from_slice(&digest);
    Ok(fingerprint)
}

fn generate_self_signed(device_id: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let key_pair = rcgen::KeyPair::generate()
        .map_err(|err| Error::Cert(format!("device key generation failed: {err}")))?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
        .map_err(|err| Error::Cert(format!("certificate parameters rejected: {err}")))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, device_id);
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|err| Error::Cert(format!("certificate self-signing failed: {err}")))?;
    let cert_der = certificate.der().as_ref().to_vec();
    let key_der_pkcs8 = key_pair.serialize_der();
    Ok((cert_der, key_der_pkcs8))
}

fn persist_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            Error::Cert(format!(
                "unsupported certificate file path {}",
                path.display()
            ))
        })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let tmp_path = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));

    let result = write_private_then_rename(&tmp_path, path, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn write_private_then_rename(tmp_path: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = open_exclusive_private(tmp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp_path, target)?;
    Ok(())
}

#[cfg(unix)]
fn open_exclusive_private(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

#[cfg(not(unix))]
fn open_exclusive_private(path: &Path) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?)
}
