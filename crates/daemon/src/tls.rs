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
//! This mirrors kdeconnect-kde's model.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use handfast_core::error::{Error, Result};
use handfast_protocol::tls::{cert_fingerprint, CertPair};
use handfast_protocol::{Identity, Packet};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::net::ToSocketAddrs;
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
    cert_fingerprint(cert.as_ref()).map_err(|err| Error::Other(err.to_string()))
}

/// Either TLS role over one TCP connection, readable/writable as one unit so
/// [`tokio::io::split`] can carve independent halves.
pub(crate) enum Wire {
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

pub(crate) type ReaderHalf = ReadHalf<Wire>;
pub(crate) type WriterHalf = WriteHalf<Wire>;
/// Packet reader: a persistent buffer so newline-delimited frames are
/// assembled cheaply and any bytes read past a frame boundary (payload
/// traffic on the same stream) are preserved for later raw reads.
pub(crate) type PacketReader = tokio::io::BufReader<ReaderHalf>;

/// An established, encrypted connection to one remote device.
pub struct Transport {
    reader: PacketReader,
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
    /// Dial a remote device and complete the TLS handshake.
    ///
    /// **Payload (data) connections only.** For payloads the sender listens
    /// and plays TLS server, the receiver dials and plays TLS client — the
    /// standard convention, matching upstream's payload sockets.
    ///
    /// For *control* connections the roles are inverted (see
    /// [`Transport::connect_control`]).
    pub async fn connect(addr: impl ToSocketAddrs, pair: Arc<CertPair>) -> Result<Self> {
        let tcp = TcpStream::connect(addr).await?;
        let peer_addr = tcp.peer_addr()?;
        let connector = TlsConnector::from(Arc::new(client_config(&pair)?));
        // IP-address ServerName: no DNS involved; CN checking is not the trust
        // mechanism (fingerprints are), so an IpAddress name is correct here.
        let name = ServerName::from(peer_addr.ip());
        let tls: ClientTlsStream<TcpStream> = connector.connect(name, tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Client(Box::new(tls)));
        debug!(%peer_addr, "payload data connection established (client role)");
        Ok(Self {
            reader: tokio::io::BufReader::new(reader),
            writer,
            peer_fingerprint,
            peer_addr,
        })
    }

    /// Accept an inbound TCP stream and complete the TLS handshake.
    ///
    /// **Payload (data) connections only** — sender side, TLS server role.
    /// For *control* connections use [`Transport::accept_control`], where the
    /// acceptor plays the TLS *client* role per the KDE Connect convention.
    pub async fn accept(tcp: TcpStream, pair: Arc<CertPair>) -> Result<Self> {
        let peer_addr = tcp.peer_addr()?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config(&pair)?));
        let tls: ServerTlsStream<TcpStream> = acceptor.accept(tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Server(Box::new(tls)));
        debug!(%peer_addr, "payload data connection accepted (server role)");
        Ok(Self {
            reader: tokio::io::BufReader::new(reader),
            writer,
            peer_fingerprint,
            peer_addr,
        })
    }

    /// Establish a **control** connection to a remote device, following the
    /// upstream KDE Connect role convention.
    ///
    /// Upstream's rule is "if I'm the TCP server I will be the SSL client and
    /// vice-versa": the TCP *dialer* writes its plaintext identity packet to
    /// the raw socket first, then upgrades as the TLS **server**; the TCP
    /// *acceptor* reads that plaintext identity, then upgrades as the TLS
    /// **client**. Both the Android app (`LanLinkProvider`) and kdeconnect-kde
    /// (`LanLinkProvider` / `startServerEncryption()` on the outbound socket)
    /// implement exactly this, so Handfast must too or the TLS handshake
    /// deadlocks against them (both sides waiting on the other's ClientHello
    /// when roles collide).
    ///
    /// `mine` is written plaintext before the TLS handshake; the post-TLS
    /// secure identity exchange is driven separately via
    /// [`crate::handshake`].
    pub async fn connect_control(
        addr: impl ToSocketAddrs,
        pair: Arc<CertPair>,
        mine: &Identity,
    ) -> Result<Self> {
        use tokio::io::AsyncWriteExt;
        let mut tcp = TcpStream::connect(addr).await?;
        let peer_addr = tcp.peer_addr()?;

        // 1. Plaintext identity first (upstream reads this line before any TLS).
        let mut frame = serde_json::to_vec(&Packet::identity(mine.clone()))?;
        frame.push(b'\n');
        tcp.write_all(&frame).await?;
        tcp.flush().await?;

        // 2. Upgrade as the TLS *server* (dialer = server per upstream).
        let acceptor = TlsAcceptor::from(Arc::new(server_config(&pair)?));
        let tls: ServerTlsStream<TcpStream> = acceptor.accept(tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Server(Box::new(tls)));
        debug!(%peer_addr, "outbound control tls established (server role)");
        Ok(Self {
            reader: tokio::io::BufReader::new(reader),
            writer,
            peer_fingerprint,
            peer_addr,
        })
    }

    /// Accept an inbound **control** connection: the remote peer has already
    /// written its plaintext identity packet (the caller reads it before
    /// calling this — see [`crate::handshake::accept_control`]), and we now
    /// upgrade as the TLS **client** per the upstream role convention.
    pub async fn accept_control(tcp: TcpStream, pair: Arc<CertPair>) -> Result<Self> {
        let peer_addr = tcp.peer_addr()?;
        let connector = TlsConnector::from(Arc::new(client_config(&pair)?));
        let name = ServerName::from(peer_addr.ip());
        let tls: ClientTlsStream<TcpStream> = connector.connect(name, tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Client(Box::new(tls)));
        debug!(%peer_addr, "inbound control tls established (client role)");
        Ok(Self {
            reader: tokio::io::BufReader::new(reader),
            writer,
            peer_fingerprint,
            peer_addr,
        })
    }

    /// SHA-256 of the peer end-entity certificate (DER), for pinning.
    pub fn peer_fingerprint(&self) -> [u8; 32] {
        self.peer_fingerprint
    }

    /// Address of the peer end of the connection (used to dial the data
    /// connection a peer announces for payloads).
    #[must_use]
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        self.peer_addr
    }

    /// Read one newline-delimited JSON packet from the wire.
    pub async fn read_packet(&mut self) -> Result<handfast_protocol::Packet> {
        handfast_protocol::Packet::read_from(&mut self.reader)
            .await
            .map_err(|err| Error::Other(err.to_string()))
    }

    /// Write one packet; flushes immediately (control-plane latency matters).
    pub async fn write_packet(&mut self, packet: &handfast_protocol::Packet) -> Result<()> {
        packet
            .write_to(&mut self.writer)
            .await
            .map_err(|err| Error::Other(err.to_string()))?;
        Ok(self.writer.flush().await?)
    }

    /// Read exactly `buf.len()` raw payload bytes from the connection.
    pub async fn read_bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        use tokio::io::AsyncReadExt;
        self.reader.read_exact(buf).await?;
        Ok(())
    }

    /// Read up to `buf.len()` raw payload bytes; returns how many were read
    /// (0 at EOF). Payloads of unknown size use this until EOF.
    ///
    /// A peer that closes the TCP stream without a TLS `close_notify` (common
    /// on some stacks) surfaces as `UnexpectedEof`; for payload streams that
    /// *is* end-of-data, so it is normalized to `Ok(0)` here. The control
    /// channel never uses this method.
    pub async fn read_some(&mut self, buf: &mut [u8]) -> Result<usize> {
        use tokio::io::AsyncReadExt;
        match self.reader.read(buf).await {
            Ok(n) => Ok(n),
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
            Err(err) => Err(err.into()),
        }
    }

    /// Write raw payload bytes; flushes (progress-friendly on LAN links).
    pub async fn write_bytes(&mut self, buf: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.writer.write_all(buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Split into halves so reader/writer loops can own each direction.
    pub(crate) fn into_parts(self) -> (PacketReader, WriterHalf) {
        (self.reader, self.writer)
    }
}
