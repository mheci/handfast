//! An independent TLS peer for link tests.
//!
//! The dialing side is the TLS server and the accepting side is the TLS client,
//! matching KDE Connect. The peer builds its own certificates, configs and
//! frames, so a link test never shares code with the production link.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName as RustlsDistinguishedName,
    ProtocolVersion, ServerConfig, SignatureScheme,
};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::frame::{self, Result, TestkitError, WirePacket};
use crate::peer::PeerConfig;

/// Device ID of the checked-in RSA fixture.
pub const RSA_DEVICE_ID: &str = "rsa0rsa0rsa0rsa0rsa0rsa0rsa0rsa0";

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const RSA_CERT_DER: &[u8] = include_bytes!("../tests/fixtures/rsa-cert.der");
const RSA_KEY_DER: &[u8] = include_bytes!("../tests/fixtures/rsa-key.der");

/// TLS protocol versions a test peer offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsVersion {
    /// TLS 1.2 only.
    Tls12,
    /// TLS 1.3 only.
    Tls13,
    /// Both versions, preferring TLS 1.3.
    Both,
}

impl TlsVersion {
    fn protocol_versions(self) -> &'static [&'static rustls::SupportedProtocolVersion] {
        const TLS12: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS12];
        const TLS13: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
        const BOTH: &[&rustls::SupportedProtocolVersion] =
            &[&rustls::version::TLS13, &rustls::version::TLS12];
        match self {
            Self::Tls12 => TLS12,
            Self::Tls13 => TLS13,
            Self::Both => BOTH,
        }
    }
}

/// Certificate family a test peer presents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CertificateKind {
    /// Generated ECDSA P-256 certificate.
    Ecdsa,
    /// Checked-in RSA certificate and PKCS#8 key fixture.
    Rsa,
}

struct TestCredentials {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl TestCredentials {
    fn ecdsa(device_id: &str) -> Result<Self> {
        let key_pair = KeyPair::generate()
            .map_err(|error| TestkitError::Protocol(format!("key generation: {error}")))?;
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, device_id);
        let certificate = params
            .self_signed(&key_pair)
            .map_err(|error| TestkitError::Protocol(format!("certificate: {error}")))?;
        Ok(Self {
            cert_der: certificate.der().as_ref().to_vec(),
            key_der: key_pair.serialize_der(),
        })
    }

    fn rsa() -> Self {
        Self {
            cert_der: RSA_CERT_DER.to_vec(),
            key_der: RSA_KEY_DER.to_vec(),
        }
    }
}

/// One test peer's identity, TLS versions and certificate.
pub struct TlsPeerConfig {
    /// Identity fields this peer sends.
    pub identity: PeerConfig,
    /// TLS versions this peer offers.
    pub version: TlsVersion,
    credentials: TestCredentials,
}

impl TlsPeerConfig {
    /// Build an ECDSA P-256 peer whose certificate CN is the identity device ID.
    ///
    /// # Errors
    ///
    /// Fails when the provider cannot generate the key or certificate.
    pub fn ecdsa(identity: PeerConfig, version: TlsVersion) -> Result<Self> {
        let credentials = TestCredentials::ecdsa(&identity.device_id)?;
        Ok(Self {
            identity,
            version,
            credentials,
        })
    }

    /// Build a peer that presents the checked-in RSA fixture.
    ///
    /// The fixture CN is fixed, so the identity device ID becomes
    /// [`RSA_DEVICE_ID`].
    #[must_use]
    pub fn rsa(version: TlsVersion) -> Self {
        let identity = PeerConfig {
            device_id: RSA_DEVICE_ID.to_string(),
            device_name: "Handfast RSA test peer".to_string(),
            ..PeerConfig::client()
        };
        Self {
            identity,
            version,
            credentials: TestCredentials::rsa(),
        }
    }

    fn hello_packet(&self, target_device_id: &str) -> WirePacket {
        let mut hello = self.identity.identity_packet();
        hello
            .body
            .insert("targetDeviceId".to_string(), json!(target_device_id));
        hello.body.insert(
            "targetProtocolVersion".to_string(),
            json!(self.identity.protocol_version),
        );
        hello
    }

    fn check_hello(&self, hello: &WirePacket) -> Result<()> {
        expect_type(hello, "kdeconnect.identity")?;
        let target = hello
            .body
            .get("targetDeviceId")
            .and_then(Value::as_str)
            .ok_or_else(|| TestkitError::Protocol("hello has no targetDeviceId".to_string()))?;
        if target != self.identity.device_id {
            return Err(TestkitError::Protocol(format!(
                "hello targets {target}, expected {}",
                self.identity.device_id
            )));
        }
        let version = hello
            .body
            .get("targetProtocolVersion")
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .ok_or_else(|| {
                TestkitError::Protocol("hello has no targetProtocolVersion".to_string())
            })?;
        if version != u64::from(self.identity.protocol_version) {
            return Err(TestkitError::Protocol(format!(
                "hello targets protocol {version}, expected {}",
                self.identity.protocol_version
            )));
        }
        Ok(())
    }

    fn server_config(&self) -> Result<ServerConfig> {
        let provider = crypto_provider();
        ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(self.version.protocol_versions())
            .map_err(|error| TestkitError::Protocol(format!("TLS versions: {error}")))?
            .with_client_cert_verifier(Arc::new(AcceptAnyClientCert { provider }))
            .with_single_cert(
                vec![CertificateDer::from(self.credentials.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.credentials.key_der.clone())),
            )
            .map_err(|error| TestkitError::Protocol(format!("server certificate: {error}")))
    }

    fn client_config(&self) -> Result<ClientConfig> {
        let provider = crypto_provider();
        ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(self.version.protocol_versions())
            .map_err(|error| TestkitError::Protocol(format!("TLS versions: {error}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
            .with_client_auth_cert(
                vec![CertificateDer::from(self.credentials.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.credentials.key_der.clone())),
            )
            .map_err(|error| TestkitError::Protocol(format!("client certificate: {error}")))
    }
}

/// One established testkit TLS link.
#[derive(Debug)]
pub struct TlsPeer {
    stream: tokio_rustls::TlsStream<TcpStream>,
    protocol_version: ProtocolVersion,
    peer_identity: WirePacket,
    local_identity: WirePacket,
}

impl TlsPeer {
    /// Negotiated TLS version.
    #[must_use]
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    /// Identity the peer sent over TLS.
    #[must_use]
    pub fn peer_identity(&self) -> &WirePacket {
        &self.peer_identity
    }

    /// Identity this peer sent over TLS.
    #[must_use]
    pub fn local_identity(&self) -> &WirePacket {
        &self.local_identity
    }

    /// Write one framed packet.
    ///
    /// # Errors
    ///
    /// Fails when the packet is invalid or the TLS write fails.
    pub async fn send(&mut self, packet: &WirePacket) -> Result<()> {
        within_timeout(frame::write(&mut self.stream, packet)).await
    }

    /// Read one framed packet.
    ///
    /// # Errors
    ///
    /// Fails on a truncated or malformed frame.
    pub async fn recv(&mut self) -> Result<Option<WirePacket>> {
        within_timeout(frame::read(&mut self.stream)).await
    }

    /// Send TLS close-notify and close the link.
    ///
    /// # Errors
    ///
    /// Fails when the close-notify write fails.
    pub async fn close(mut self) -> std::io::Result<()> {
        self.stream.shutdown().await
    }
}

/// Dial a listener and act as the TLS server.
///
/// Sends the plaintext identity hello, completes the handshake, then exchanges
/// the encrypted identities.
///
/// # Errors
///
/// Fails on IO, a TLS handshake failure, a hello mismatch or a timeout.
pub async fn dial(
    address: SocketAddr,
    target_device_id: &str,
    config: &TlsPeerConfig,
) -> Result<TlsPeer> {
    let mut tcp = within_timeout(TcpStream::connect(address)).await?;
    tcp.set_nodelay(true)?;
    within_timeout(frame::write(
        &mut tcp,
        &config.hello_packet(target_device_id),
    ))
    .await?;

    let acceptor = TlsAcceptor::from(Arc::new(config.server_config()?));
    let stream = within_timeout(acceptor.accept(tcp)).await?;
    let mut stream: tokio_rustls::TlsStream<TcpStream> = stream.into();
    let protocol_version = negotiated_version(&stream)?;

    let local_identity = config.identity.identity_packet();
    within_timeout(frame::write(&mut stream, &local_identity)).await?;
    let peer_identity = within_timeout(frame::read(&mut stream))
        .await?
        .ok_or_else(|| TestkitError::Protocol("peer closed before its identity".to_string()))?;
    check_identity(&peer_identity, target_device_id)?;

    Ok(TlsPeer {
        stream,
        protocol_version,
        peer_identity,
        local_identity,
    })
}

/// Accept a dialer and act as the TLS client.
///
/// Reads the plaintext identity hello, completes the handshake, then exchanges
/// the encrypted identities.
///
/// # Errors
///
/// Fails on IO, a hello mismatch, a TLS handshake failure or a timeout.
pub async fn accept(listener: &TcpListener, config: &TlsPeerConfig) -> Result<TlsPeer> {
    let (mut tcp, _) = within_timeout(listener.accept()).await?;
    tcp.set_nodelay(true)?;
    let hello = within_timeout(frame::read(&mut tcp))
        .await?
        .ok_or_else(|| TestkitError::Protocol("dialer closed before its hello".to_string()))?;
    config.check_hello(&hello)?;
    let hello_device_id = device_id(&hello)?.to_string();

    let connector = TlsConnector::from(Arc::new(config.client_config()?));
    let server_name = ServerName::try_from("localhost")
        .map_err(|error| TestkitError::Protocol(format!("server name: {error}")))?;
    let stream = within_timeout(connector.connect(server_name, tcp)).await?;
    let mut stream: tokio_rustls::TlsStream<TcpStream> = stream.into();
    let protocol_version = negotiated_version(&stream)?;

    let peer_identity = within_timeout(frame::read(&mut stream))
        .await?
        .ok_or_else(|| TestkitError::Protocol("dialer closed before its identity".to_string()))?;
    check_identity(&peer_identity, &hello_device_id)?;
    let local_identity = config.identity.identity_packet();
    within_timeout(frame::write(&mut stream, &local_identity)).await?;

    Ok(TlsPeer {
        stream,
        protocol_version,
        peer_identity,
        local_identity,
    })
}

/// Result of one TLS self-test case.
#[derive(Debug, Clone, Serialize)]
pub struct TlsSelfTestCase {
    /// TLS version both peers were restricted to.
    pub version: TlsVersion,
    /// Certificate the dialing peer presented.
    pub dialer_certificate: CertificateKind,
    /// Certificate the accepting peer presented.
    pub listener_certificate: CertificateKind,
    /// Negotiated TLS version.
    pub negotiated: String,
    /// Whether the dialer received the listener's ping.
    pub ping_received: bool,
}

/// Report from [`run_tls_self_test`].
#[derive(Debug, Clone, Serialize)]
pub struct TlsSelfTestReport {
    /// One case per TLS version and certificate arrangement.
    pub cases: Vec<TlsSelfTestCase>,
}

/// Run two testkit peers over loopback with TLS 1.2 and TLS 1.3.
///
/// Each version runs with ECDSA on both sides, an RSA dialer and an RSA
/// listener. Each case completes the plaintext hello, the TLS handshake, the
/// encrypted identity exchange and one ping.
///
/// # Errors
///
/// Fails on the first case that does not complete, naming the case.
pub async fn run_tls_self_test() -> Result<TlsSelfTestReport> {
    let mut cases = Vec::new();
    for version in [TlsVersion::Tls12, TlsVersion::Tls13] {
        for (dialer_certificate, listener_certificate) in [
            (CertificateKind::Ecdsa, CertificateKind::Ecdsa),
            (CertificateKind::Rsa, CertificateKind::Ecdsa),
            (CertificateKind::Ecdsa, CertificateKind::Rsa),
        ] {
            let case = run_case(version, dialer_certificate, listener_certificate)
                .await
                .map_err(|error| {
                    TestkitError::Protocol(format!(
                        "{version:?} {dialer_certificate:?} to {listener_certificate:?}: {error}"
                    ))
                })?;
            cases.push(case);
        }
    }
    Ok(TlsSelfTestReport { cases })
}

async fn run_case(
    version: TlsVersion,
    dialer_certificate: CertificateKind,
    listener_certificate: CertificateKind,
) -> Result<TlsSelfTestCase> {
    let listener_config = config(PeerConfig::server(), version, listener_certificate)?;
    let dialer_config = config(PeerConfig::client(), version, dialer_certificate)?;
    let target = listener_config.identity.device_id.clone();
    let ping = listener_config
        .identity
        .ping_packet("hello from the testkit TLS listener");

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let listener_task = tokio::spawn(async move {
        let mut peer = accept(&listener, &listener_config).await?;
        peer.send(&ping).await?;
        Ok::<_, TestkitError>(peer.protocol_version())
    });

    let mut dialer = dial(address, &target, &dialer_config).await?;
    let message = dialer
        .recv()
        .await?
        .ok_or_else(|| TestkitError::Protocol("listener closed before its ping".to_string()))?;
    expect_type(&message, "kdeconnect.ping")?;
    let listener_version = listener_task
        .await
        .map_err(|error| TestkitError::Protocol(format!("listener task: {error}")))??;
    if listener_version != dialer.protocol_version() {
        return Err(TestkitError::Protocol(
            "peers negotiated different TLS versions".to_string(),
        ));
    }

    Ok(TlsSelfTestCase {
        version,
        dialer_certificate,
        listener_certificate,
        negotiated: negotiated_name(dialer.protocol_version()),
        ping_received: true,
    })
}

fn config(
    identity: PeerConfig,
    version: TlsVersion,
    certificate: CertificateKind,
) -> Result<TlsPeerConfig> {
    match certificate {
        CertificateKind::Ecdsa => TlsPeerConfig::ecdsa(identity, version),
        CertificateKind::Rsa => Ok(TlsPeerConfig::rsa(version)),
    }
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn negotiated_version(stream: &tokio_rustls::TlsStream<TcpStream>) -> Result<ProtocolVersion> {
    stream
        .get_ref()
        .1
        .protocol_version()
        .ok_or_else(|| TestkitError::Protocol("TLS handshake without a version".to_string()))
}

fn negotiated_name(version: ProtocolVersion) -> String {
    match version {
        ProtocolVersion::TLSv1_2 => "TLSv1.2".to_string(),
        ProtocolVersion::TLSv1_3 => "TLSv1.3".to_string(),
        other => format!("{other:?}"),
    }
}

fn device_id(packet: &WirePacket) -> Result<&str> {
    packet
        .body
        .get("deviceId")
        .and_then(Value::as_str)
        .ok_or_else(|| TestkitError::Protocol("identity has no deviceId".to_string()))
}

fn check_identity(packet: &WirePacket, expected_device_id: &str) -> Result<()> {
    expect_type(packet, "kdeconnect.identity")?;
    let device_id = device_id(packet)?;
    if device_id != expected_device_id {
        return Err(TestkitError::Protocol(format!(
            "encrypted identity is {device_id}, expected {expected_device_id}"
        )));
    }
    if packet.body.get("protocolVersion").and_then(Value::as_u64) != Some(8) {
        return Err(TestkitError::Protocol(
            "encrypted identity is not protocol 8".to_string(),
        ));
    }
    Ok(())
}

fn expect_type(packet: &WirePacket, expected: &str) -> Result<()> {
    if packet.ptype != expected {
        return Err(TestkitError::Protocol(format!(
            "expected {expected}, got {}",
            packet.ptype
        )));
    }
    Ok(())
}

async fn within_timeout<F, T, E>(future: F) -> Result<T>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: Into<TestkitError>,
{
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .map_err(|_| TestkitError::Timeout)?
        .map_err(Into::into)
}

/// Accept-any server verifier. It checks proof of possession and defers the
/// identity decision to the caller, matching the production link policy.
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
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
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
/// identity decision to the caller, matching the production link policy.
#[derive(Debug)]
struct AcceptAnyClientCert {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[RustlsDistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
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
