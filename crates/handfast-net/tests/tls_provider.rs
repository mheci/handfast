//! Proves the rustls ring provider completes TLS 1.2 and TLS 1.3 handshakes.
//!
//! The Windows CI job builds the dependencies with the static CRT. The Linux
//! test job runs this handshake, so the provider, the certificate generator and
//! the async TLS adapter are proven on a real handshake and built for Windows.

use std::sync::Arc;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, ProtocolVersion, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const DEVICE_ID: &str = "0123456789abcdef0123456789abcdef";

/// The spike has no trust store. It checks only that the ring provider proves
/// possession of the peer key, which is the part the link layer keeps.
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn server_config(
    provider: &Arc<rustls::crypto::CryptoProvider>,
    cert: &CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    version: &'static rustls::SupportedProtocolVersion,
) -> rustls::ServerConfig {
    rustls::ServerConfig::builder_with_provider(Arc::clone(provider))
        .with_protocol_versions(&[version])
        .expect("the ring provider has cipher suites for the requested TLS version")
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .expect("the generated certificate and key are a valid pair")
}

fn client_config(
    provider: &Arc<rustls::crypto::CryptoProvider>,
    version: &'static rustls::SupportedProtocolVersion,
) -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(Arc::clone(provider))
        .with_protocol_versions(&[version])
        .expect("the ring provider has cipher suites for the requested TLS version")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(Arc::clone(provider))))
        .with_no_client_auth()
}

async fn assert_loopback(
    version: &'static rustls::SupportedProtocolVersion,
    expected: ProtocolVersion,
) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key_pair = KeyPair::generate().expect("ring generates an ECDSA P-256 key");
    let mut params = CertificateParams::new(vec!["localhost".to_string()])
        .expect("localhost is a valid subject alternative name");
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, DEVICE_ID);
    let certificate = params
        .self_signed(&key_pair)
        .expect("ring signs the self-signed certificate");
    let cert_der = certificate.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind a loopback listener");
    let address = listener.local_addr().expect("read the bound address");
    let server = tokio::spawn({
        let config = Arc::new(server_config(&provider, &cert_der, key_der, version));
        async move {
            let acceptor = TlsAcceptor::from(config);
            let (tcp, _) = listener.accept().await.expect("accept the loopback client");
            let mut stream = acceptor
                .accept(tcp)
                .await
                .expect("server side of the TLS handshake");
            let negotiated = stream.get_ref().1.protocol_version();
            let mut request = [0u8; 4];
            stream
                .read_exact(&mut request)
                .await
                .expect("read the request");
            stream.write_all(&request).await.expect("echo the request");
            stream.shutdown().await.expect("close the server stream");
            negotiated
        }
    });

    let connector = TlsConnector::from(Arc::new(client_config(&provider, version)));
    let tcp = TcpStream::connect(address)
        .await
        .expect("connect to the loopback listener");
    let mut stream = connector
        .connect(
            ServerName::try_from("localhost").expect("localhost is a valid server name"),
            tcp,
        )
        .await
        .expect("client side of the TLS handshake");
    assert_eq!(stream.get_ref().1.protocol_version(), Some(expected));
    stream.write_all(b"ping").await.expect("send the request");
    let mut echoed = [0u8; 4];
    stream.read_exact(&mut echoed).await.expect("read the echo");
    assert_eq!(&echoed, b"ping");

    assert_eq!(
        server.await.expect("the server task completes"),
        Some(expected)
    );
}

#[tokio::test]
async fn tls12_loopback_negotiates_tls12() {
    assert_loopback(&rustls::version::TLS12, ProtocolVersion::TLSv1_2).await;
}

#[tokio::test]
async fn tls13_loopback_negotiates_tls13() {
    assert_loopback(&rustls::version::TLS13, ProtocolVersion::TLSv1_3).await;
}
