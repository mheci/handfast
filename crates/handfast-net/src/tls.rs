//! rustls configuration for both link roles.
//!
//! KDE Connect reverses the usual TLS roles: the dialer is the TLS server and
//! the listener is the TLS client. Both sides accept any certificate chain and
//! defer the identity decision to
//! [`PeerCertificate::check`](crate::PeerCertificate) after the handshake. The
//! accept-any verifiers still prove the peer owns its private key; they only
//! skip chain validation, which no KDE Connect peer can pass because every
//! certificate is self-signed. The listener does not know the expected peer ID
//! until the plaintext hello, so a per-connection verifier would duplicate the
//! policy.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};

use crate::error::TlsError;
use crate::identity::TlsIdentity;

/// TLS 1.3 first so a peer that offers both negotiates 1.3. TLS 1.2 stays for
/// the official Android client, whose `SSLContext` caps it at 1.2.
const PROTOCOL_VERSIONS: &[&rustls::SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The dialer's TLS server configuration.
///
/// The dialer presents its certificate and requires one from the listener.
pub(crate) fn server_config(tls: &TlsIdentity) -> Result<Arc<ServerConfig>, TlsError> {
    let provider = crypto_provider();
    let config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(PROTOCOL_VERSIONS)
        .map_err(|error| TlsError::Certificate(format!("TLS versions: {error}")))?
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert { provider }))
        .with_single_cert(
            vec![CertificateDer::from(tls.cert_der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(tls.key_der().to_vec())),
        )
        .map_err(|error| TlsError::Certificate(format!("server certificate: {error}")))?;
    Ok(Arc::new(config))
}

/// The listener's TLS client configuration.
///
/// The listener presents its certificate and accepts any server chain. The
/// server name is ignored by the verifier; the peer identity comes from the
/// certificate common name after the handshake.
pub(crate) fn client_config(tls: &TlsIdentity) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = crypto_provider();
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(PROTOCOL_VERSIONS)
        .map_err(|error| TlsError::Certificate(format!("TLS versions: {error}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
        .with_client_auth_cert(
            vec![CertificateDer::from(tls.cert_der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(tls.key_der().to_vec())),
        )
        .map_err(|error| TlsError::Certificate(format!("client certificate: {error}")))?;
    Ok(Arc::new(config))
}

/// Accept-any server verifier. It checks proof of possession and defers the
/// identity decision to [`PeerCertificate::check`](crate::PeerCertificate).
#[derive(Debug)]
struct AcceptAnyServerCert {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Accept-any client verifier. It checks proof of possession and defers the
/// identity decision to [`PeerCertificate::check`](crate::PeerCertificate).
#[derive(Debug)]
struct AcceptAnyClientCert {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use handfast_proto::DeviceId;
    use handfast_testkit::peer::PeerConfig;
    use handfast_testkit::tls_peer::{TlsPeerConfig, TlsVersion};
    use rustls::pki_types::ServerName;
    use rustls::{ProtocolVersion, SignatureScheme};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::*;
    use crate::identity::{Fingerprint, PeerCertificate, PeerTrust};

    const HANDFAST_DEVICE_ID: &str = "0123456789abcdef0123456789abcdef";
    const IO_TIMEOUT: Duration = Duration::from_secs(5);

    #[derive(Clone, Copy)]
    enum HandfastRole {
        Server,
        Client,
    }

    fn device_id(value: &str) -> DeviceId {
        DeviceId::new(value).expect("the test device ID is valid")
    }

    fn local_tls() -> Arc<TlsIdentity> {
        Arc::new(
            TlsIdentity::generate(device_id(HANDFAST_DEVICE_ID))
                .expect("generate the local identity"),
        )
    }

    fn server_name() -> ServerName<'static> {
        ServerName::IpAddress("127.0.0.1".parse().expect("loopback is an IP address"))
    }

    fn peer_certificate<IO>(stream: &tokio_rustls::TlsStream<IO>) -> PeerCertificate {
        let certificates = stream
            .get_ref()
            .1
            .peer_certificates()
            .expect("the peer presented a certificate");
        assert_eq!(certificates.len(), 1, "exactly one peer certificate");
        PeerCertificate::parse(certificates[0].as_ref()).expect("the peer certificate parses")
    }

    async fn handfast_server(peer: TlsPeerConfig) -> (ProtocolVersion, PeerCertificate) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let address = listener.local_addr().expect("read the bound address");
        let acceptor =
            TlsAcceptor::from(server_config(&local_tls()).expect("build the server config"));
        let client = Arc::new(
            peer.client_config()
                .expect("build the testkit client config"),
        );

        let client_task = tokio::spawn(async move {
            let tcp = TcpStream::connect(address)
                .await
                .expect("connect to the listener");
            let connector = TlsConnector::from(client);
            let mut stream = connector
                .connect(server_name(), tcp)
                .await
                .expect("client side of the handshake");
            stream.write_all(b"ping").await.expect("send the request");
            let mut reply = [0u8; 4];
            stream.read_exact(&mut reply).await.expect("read the echo");
            assert_eq!(&reply, b"ping");
            stream.shutdown().await.expect("close the client stream");
        });

        let (tcp, _) = listener.accept().await.expect("accept the testkit client");
        let mut stream = acceptor
            .accept(tcp)
            .await
            .expect("server side of the handshake");
        let negotiated = stream
            .get_ref()
            .1
            .protocol_version()
            .expect("a negotiated TLS version");
        let certificate = peer_certificate(&stream);
        let mut request = [0u8; 4];
        stream
            .read_exact(&mut request)
            .await
            .expect("read the request");
        stream.write_all(&request).await.expect("echo the request");
        stream.shutdown().await.expect("close the server stream");
        client_task
            .await
            .expect("the testkit client task completes");
        (negotiated, certificate)
    }

    async fn handfast_client(peer: TlsPeerConfig) -> (ProtocolVersion, PeerCertificate) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let address = listener.local_addr().expect("read the bound address");
        let acceptor = TlsAcceptor::from(Arc::new(
            peer.server_config()
                .expect("build the testkit server config"),
        ));

        let server_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept the handfast client");
            let mut stream = acceptor
                .accept(tcp)
                .await
                .expect("server side of the handshake");
            let mut request = [0u8; 4];
            stream
                .read_exact(&mut request)
                .await
                .expect("read the request");
            stream.write_all(&request).await.expect("echo the request");
            stream.shutdown().await.expect("close the server stream");
        });

        let connector =
            TlsConnector::from(client_config(&local_tls()).expect("build the client config"));
        let tcp = TcpStream::connect(address)
            .await
            .expect("connect to the testkit listener");
        let mut stream = connector
            .connect(server_name(), tcp)
            .await
            .expect("client side of the handshake");
        let negotiated = stream
            .get_ref()
            .1
            .protocol_version()
            .expect("a negotiated TLS version");
        let certificate = peer_certificate(&stream);
        stream.write_all(b"ping").await.expect("send the request");
        let mut reply = [0u8; 4];
        stream.read_exact(&mut reply).await.expect("read the echo");
        assert_eq!(&reply, b"ping");
        stream.shutdown().await.expect("close the client stream");
        server_task
            .await
            .expect("the testkit server task completes");
        (negotiated, certificate)
    }

    async fn assert_case(role: HandfastRole, peer: TlsPeerConfig, expected: ProtocolVersion) {
        let peer_device_id = device_id(&peer.identity.device_id);
        let pin = Fingerprint::of_certificate(peer.cert_der());
        let (negotiated, certificate) = match role {
            HandfastRole::Server => handfast_server(peer).await,
            HandfastRole::Client => handfast_client(peer).await,
        };

        assert_eq!(negotiated, expected, "negotiated TLS version");
        assert_eq!(certificate.device_id(), &peer_device_id, "certificate CN");
        certificate
            .check(&peer_device_id, PeerTrust::Unpaired)
            .expect("an unpaired peer with the expected CN is accepted");
        certificate
            .check(&peer_device_id, PeerTrust::Pinned(pin))
            .expect("a pre-seeded pin matches the peer certificate");
    }

    async fn run_case(role: HandfastRole, peer: TlsPeerConfig, expected: ProtocolVersion) {
        tokio::time::timeout(IO_TIMEOUT, assert_case(role, peer, expected))
            .await
            .expect("the TLS-only handshake completes");
    }

    #[tokio::test]
    async fn server_config_negotiates_tls12_with_an_ecdsa_client() {
        let peer = TlsPeerConfig::ecdsa(PeerConfig::client(), TlsVersion::Tls12)
            .expect("build the ECDSA peer");
        run_case(HandfastRole::Server, peer, ProtocolVersion::TLSv1_2).await;
    }

    #[tokio::test]
    async fn server_config_negotiates_tls13_with_an_ecdsa_client() {
        let peer = TlsPeerConfig::ecdsa(PeerConfig::client(), TlsVersion::Tls13)
            .expect("build the ECDSA peer");
        run_case(HandfastRole::Server, peer, ProtocolVersion::TLSv1_3).await;
    }

    #[tokio::test]
    async fn client_config_negotiates_tls12_with_an_ecdsa_server() {
        let peer = TlsPeerConfig::ecdsa(PeerConfig::server(), TlsVersion::Tls12)
            .expect("build the ECDSA peer");
        run_case(HandfastRole::Client, peer, ProtocolVersion::TLSv1_2).await;
    }

    #[tokio::test]
    async fn client_config_negotiates_tls13_with_an_ecdsa_server() {
        let peer = TlsPeerConfig::ecdsa(PeerConfig::server(), TlsVersion::Tls13)
            .expect("build the ECDSA peer");
        run_case(HandfastRole::Client, peer, ProtocolVersion::TLSv1_3).await;
    }

    #[tokio::test]
    async fn server_config_negotiates_tls12_with_an_rsa_client() {
        run_case(
            HandfastRole::Server,
            TlsPeerConfig::rsa(TlsVersion::Tls12),
            ProtocolVersion::TLSv1_2,
        )
        .await;
    }

    #[tokio::test]
    async fn server_config_negotiates_tls13_with_an_rsa_client() {
        run_case(
            HandfastRole::Server,
            TlsPeerConfig::rsa(TlsVersion::Tls13),
            ProtocolVersion::TLSv1_3,
        )
        .await;
    }

    #[tokio::test]
    async fn client_config_negotiates_tls12_with_an_rsa_server() {
        run_case(
            HandfastRole::Client,
            TlsPeerConfig::rsa(TlsVersion::Tls12),
            ProtocolVersion::TLSv1_2,
        )
        .await;
    }

    #[tokio::test]
    async fn client_config_negotiates_tls13_with_an_rsa_server() {
        run_case(
            HandfastRole::Client,
            TlsPeerConfig::rsa(TlsVersion::Tls13),
            ProtocolVersion::TLSv1_3,
        )
        .await;
    }

    #[test]
    fn accept_any_verifiers_support_rsa_and_ecdsa_schemes() {
        let provider = crypto_provider();
        let server = AcceptAnyServerCert {
            provider: Arc::clone(&provider),
        };
        let client = AcceptAnyClientCert { provider };
        let schemes = [
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
        ];

        for scheme in schemes {
            assert!(
                server.supported_verify_schemes().contains(&scheme),
                "server verifier offers {scheme:?}"
            );
            assert!(
                client.supported_verify_schemes().contains(&scheme),
                "client verifier offers {scheme:?}"
            );
        }
    }

    #[tokio::test]
    async fn server_config_rejects_a_client_without_a_certificate() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback listener");
            let address = listener.local_addr().expect("read the bound address");
            let acceptor =
                TlsAcceptor::from(server_config(&local_tls()).expect("build the server config"));

            let provider = crypto_provider();
            let client = ClientConfig::builder_with_provider(Arc::clone(&provider))
                .with_protocol_versions(&[version])
                .expect("the ring provider supports the requested TLS version")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
                .with_no_client_auth();

            let client_task = tokio::spawn(async move {
                let tcp = TcpStream::connect(address)
                    .await
                    .expect("connect to the listener");
                let connector = TlsConnector::from(Arc::new(client));
                let _ = connector.connect(server_name(), tcp).await;
            });

            let (tcp, _) = listener.accept().await.expect("accept the client");
            let rejected = tokio::time::timeout(IO_TIMEOUT, acceptor.accept(tcp))
                .await
                .expect("the handshake finishes");
            assert!(
                rejected.is_err(),
                "a client without a certificate must be rejected"
            );
            client_task.await.expect("the client task completes");
        }
    }
}
