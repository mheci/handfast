//! Integration tests for device certificate generation, persistence and
//! reloading ([`CertPair::load_or_generate`]).

use handfast_protocol::tls::{cert_fingerprint, CertPair};

#[test]
fn generates_persists_and_reloads_identical_material() {
    let dir = tempfile::tempdir().unwrap();
    let generated = CertPair::load_or_generate(dir.path(), "handfast-test-device").unwrap();

    assert!(!generated.cert_der.is_empty());
    assert!(!generated.key_der_pkcs8.is_empty());
    assert_eq!(
        generated.fingerprint_sha256,
        cert_fingerprint(&generated.cert_der).unwrap()
    );

    let hex = generated.fingerprint_hex();
    assert_eq!(hex.len(), 64);
    assert!(hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));

    assert!(dir.path().join("id_cert.der").is_file());
    assert!(dir.path().join("id_key.der").is_file());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["id_cert.der", "id_key.der"] {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name} must be created with mode 0600");
        }
    }

    let reloaded = CertPair::load_or_generate(dir.path(), "renamed-device").unwrap();
    assert_eq!(reloaded, generated);
}

#[test]
fn regenerates_when_stored_files_are_empty() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("id_cert.der"), b"").unwrap();
    std::fs::write(dir.path().join("id_key.der"), b"").unwrap();

    let pair = CertPair::load_or_generate(dir.path(), "handfast-test-device").unwrap();
    assert!(!pair.cert_der.is_empty());
    assert!(!pair.key_der_pkcs8.is_empty());
}
