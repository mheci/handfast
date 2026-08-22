//! TLS transport for KDE Connect connections.
//!
//! # Trust model (matches upstream)
//!
//! Every device presents a self-signed certificate whose CN is its device id.
//! There is no CA: the TLS layer validates *structure and handshake
//! signatures* but deliberately accepts any self-signed identity. Trust is
//! established out-of-band by the pairing flow, after which the SHA-256
//! fingerprint of the peer certificate is **pinned** in SQLite. Connections
//! presenting a different fingerprint for a known device are rejected before
//! any plugin traffic flows ([`Transport::peer_fingerprint`] feeds that check).
//!
//! This mirrors kdeconnect-kde's model; hardening TODOs live in docs/PROTOCOL.md.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use handfast_core::error::{Error, Result};
use handfast_protocol::tls::{cert_fingerprint, CertPair};
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, HandshakeSignatureValid, ServerConfig,
    SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::tcp::ToSocketAddrs;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::debug;

/// Verifier used on the client side: accept any self-signed server cert.
///
/// Signature checks inside the TLS handshake still run through rustls' state
/// machine; we simply do not enforce a web-of-trust. Pinning happens later.
#[derive(Debug)]
struct AcceptAnyServer;

impl ServerCertVerifier for AcceptAnyServer {
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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

/// Verifier used on the server side: request a client cert, accept any.
#[derive(Debug)]
struct AcceptAnyClient {
    /// Empty hint list; clients present their self-signed cert regardless.
    root_hints: Vec<DistinguishedName>,
}

impl ClientCertVerifier for AcceptAnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.root_hints
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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        AcceptAnyServer.supported_verify_schemes()
    }
}

fn client_config(pair: &CertPair) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| Error::Other(format!("rustls protocol versions: {err}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
        .with_client_auth_cert(
            vec![CertificateDer::from(pair.cert_der.clone())],
            private_key(pair)?,
        )
        .map_err(|err| Error::Other(format!("client auth cert: {err}")))?;
    Ok(config)
}

fn server_config(pair: &CertPair) -> Result<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = AcceptAnyClient {
        root_hints: Vec::new(),
    };
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| Error::Other(format!("rustls protocol versions: {err}")))?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(
            vec![CertificateDer::from(pair.cert_der.clone())],
            private_key(pair)?,
        )
        .map_err(|err| Error::Other(format!("server cert: {err}")))?;
    Ok(config)
}

fn private_key(pair: &CertPair) -> Result<PrivateKeyDer<'static>> {
    Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        pair.key_der_pkcs8.clone(),
    )))
}

fn fingerprint_of_first(certs: Option<&[CertificateDer<'static>]>) -> Result<[u8; 32]> {
    let Some(certs) = certs else {
        return Err(Error::Other("peer presented no certificate".into()));
    };
    let Some(cert) = certs.first() else {
        return Err(Error::Other("peer certificate list empty".into()));
    };
    cert_fingerprint(cert.as_ref())
}

/// Either TLS role over one TCP connection, readable/writable as one unit so
/// [`tokio::io::split`] can carve independent halves.
enum Wire {
    Client(Box<ClientTlsStream<TcpStream>>),
    Server(Box<ServerTlsStream<TcpStream>>),
}

impl AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Wire::Client(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            Wire::Server(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Wire::Client(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            Wire::Server(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Wire::Client(s) => Pin::new(s.as_mut()).poll_flush(cx),
            Wire::Server(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Wire::Client(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            Wire::Server(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

type ReaderHalf = ReadHalf<Wire>;
type WriterHalf = WriteHalf<Wire>;

/// An established, encrypted connection to one remote device.
pub struct Transport {
    reader: ReaderHalf,
    writer: WriterHalf,
    peer_fingerprint: [u8; 32],
    peer_addr: std::net::SocketAddr,
}

impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transport")
            .field("peer_addr", &self.peer_addr)
            .finish_non_exhaustive()
    }
}

impl Transport {
    /// Dial a remote device and complete the TLS handshake (client role).
    pub async fn connect(addr: impl ToSocketAddrs, pair: Arc<CertPair>) -> Result<Self> {
        let tcp = TcpStream::connect(addr).await?;
        let peer_addr = tcp.peer_addr()?;
        let connector = TlsConnector::from(Arc::new(client_config(&pair)?));
        // IP-address ServerName: no DNS involved; CN checking is not the trust
        // mechanism (fingerprints are), so an IpAddress name is correct here.
        let name = ServerName::IpAddress(peer_addr.ip().to_canonical());
        let tls: ClientTlsStream<TcpStream> = connector.connect(name, tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Client(Box::new(tls)));
        debug!(%peer_addr, "outbound tls established");
        Ok(Self {
            reader,
            writer,
            peer_fingerprint,
            peer_addr,
        })
    }

    /// Accept an inbound TCP stream and complete the TLS handshake (server role).
    pub async fn accept(tcp: TcpStream, pair: Arc<CertPair>) -> Result<Self> {
        let peer_addr = tcp.peer_addr()?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config(&pair)?));
        let tls: ServerTlsStream<TcpStream> = acceptor.accept(tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Server(Box::new(tls)));
        debug!(%peer_addr, "inbound tls accepted");
        Ok(Self {
            reader,
            writer,
            peer_fingerprint,
            peer_addr,
        })
    }

    /// SHA-256 of the peer end-entity certificate (DER), for pinning.
    pub fn peer_fingerprint(&self) -> [u8; 32] {
        self.peer_fingerprint
    }

    /// Remote socket address.
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        self.peer_addr
    }

    /// Read one length-prefixed JSON packet from the wire.
    pub async fn read_packet(&mut self) -> Result<handfast_protocol::Packet> {
        Ok(handfast_protocol::Packet::read_from(&mut self.reader).await?)
    }

    /// Write one packet; flushes immediately (control-plane latency matters).
    pub async fn write_packet(&mut self, packet: &handfast_protocol::Packet) -> Result<()> {
        packet.write_to(&mut self.writer).await?;
        Ok(self.writer.flush().await?)
    }

    /// Split into halves so reader/writer loops can own each direction.
    pub fn into_parts(self) -> (ReaderHalf, WriterHalf) {
        (self.reader, self.writer)
    }
}
