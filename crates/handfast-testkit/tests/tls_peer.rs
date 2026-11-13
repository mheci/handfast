//! Independent TLS peer behavior.

use std::path::PathBuf;

use handfast_testkit::frame::TestkitError;
use handfast_testkit::peer::PeerConfig;
use handfast_testkit::tls_peer::{
    accept, dial, run_tls_self_test, CertificateKind, TlsPeerConfig, TlsVersion, RSA_DEVICE_ID,
};
use tokio::net::TcpListener;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[tokio::test]
async fn tls_self_test_covers_versions_and_certificates() {
    let report = run_tls_self_test().await.unwrap();
    assert_eq!(report.cases.len(), 6);

    for case in &report.cases {
        assert!(case.ping_received);
        match case.version {
            TlsVersion::Tls12 => assert_eq!(case.negotiated, "TLSv1.2"),
            TlsVersion::Tls13 => assert_eq!(case.negotiated, "TLSv1.3"),
            TlsVersion::Both => panic!("self-test must pin a TLS version"),
        }
    }

    for version in [TlsVersion::Tls12, TlsVersion::Tls13] {
        assert!(
            report
                .cases
                .iter()
                .any(|case| case.version == version
                    && case.dialer_certificate == CertificateKind::Rsa)
        );
        assert!(report.cases.iter().any(
            |case| case.version == version && case.listener_certificate == CertificateKind::Rsa
        ));
    }
}

#[tokio::test]
async fn mismatched_tls_versions_do_not_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let listener_config = TlsPeerConfig::ecdsa(PeerConfig::server(), TlsVersion::Tls13).unwrap();
    let dialer_config = TlsPeerConfig::ecdsa(PeerConfig::client(), TlsVersion::Tls12).unwrap();
    let target = listener_config.identity.device_id.clone();

    let listener_task = tokio::spawn(async move { accept(&listener, &listener_config).await });
    let error = dial(address, &target, &dialer_config).await.unwrap_err();
    assert!(
        !matches!(error, TestkitError::Timeout),
        "version mismatch must fail in the handshake, got {error}"
    );
    assert!(listener_task.await.unwrap().is_err());
}

#[tokio::test]
async fn a_second_link_with_the_same_certificate_succeeds() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let listener_config = TlsPeerConfig::ecdsa(PeerConfig::server(), TlsVersion::Both).unwrap();
    let dialer_config = TlsPeerConfig::ecdsa(PeerConfig::client(), TlsVersion::Both).unwrap();
    let target = listener_config.identity.device_id.clone();

    let listener_task = tokio::spawn(async move {
        let first = accept(&listener, &listener_config).await?;
        let second = accept(&listener, &listener_config).await?;
        Ok::<_, TestkitError>((first, second))
    });

    let first = dial(address, &target, &dialer_config).await.unwrap();
    let second = dial(address, &target, &dialer_config).await.unwrap();
    first.close().await.unwrap();

    let (listener_first, listener_second) = listener_task.await.unwrap().unwrap();
    assert_eq!(listener_first.protocol_version(), second.protocol_version());
    assert_eq!(
        listener_second.protocol_version(),
        second.protocol_version()
    );
    second.close().await.unwrap();
}

#[test]
fn rsa_fixture_is_a_pkcs8_rsa_key_and_certificate() {
    const RSA_ENCRYPTION_OID: &[u8] = &[
        0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01,
    ];

    let certificate = fixture("rsa-cert.der");
    assert!(certificate
        .windows(RSA_ENCRYPTION_OID.len())
        .any(|window| window == RSA_ENCRYPTION_OID));
    assert!(certificate
        .windows(RSA_DEVICE_ID.len())
        .any(|window| window == RSA_DEVICE_ID.as_bytes()));

    let key = fixture("rsa-key.der");
    assert_eq!(key.first(), Some(&0x30), "PKCS#8 key is a DER sequence");
    assert_eq!(key.get(1), Some(&0x82), "PKCS#8 key uses a long length");

    let config = TlsPeerConfig::rsa(TlsVersion::Both);
    assert_eq!(config.identity.device_id, RSA_DEVICE_ID);
}
