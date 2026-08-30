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
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf, ReadHalf, WriteHalf};
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

/// An established, encrypted connection to one remote device.
///
/// The reader half is buffered: control packets are newline-delimited JSON
/// (see [`handfast_protocol::packet::Packet`]), and a `BufReader` lets the
/// frame parser consume exactly one line while leaving the next packet's
/// bytes buffered for the following call.
pub struct Transport {
    reader: BufReader<ReaderHalf>,
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
        let name = ServerName::from(peer_addr.ip());
        let tls: ClientTlsStream<TcpStream> = connector.connect(name, tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Client(Box::new(tls)));
        let reader = BufReader::new(reader);
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
        let reader = BufReader::new(reader);
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

    /// Read one length-prefixed JSON packet from the wire.
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

    /// Split into halves so reader/writer loops can own each direction.
    ///
    /// The reader is buffered; callers read one newline-delimited packet per
    /// [`handfast_protocol::Packet::read_from`] call.
    pub(crate) fn into_parts(self) -> (BufReader<ReaderHalf>, WriterHalf) {
        (self.reader, self.writer)
    }
}

/// A raw (unframed) TLS stream used for file-transfer payloads.
///
/// Mirrors the KDE Connect payload channel: the *sender* listens and the
/// *receiver* dials back; the sender acts as the TLS server and the receiver
/// as the TLS client, then exactly `payloadSize` bytes stream unframed over
/// the connection (no length prefixes, no JSON). Both sides present their
/// device certificates; the peer fingerprint is available for the same
/// pinning checks the control channel uses.
pub struct PayloadChannel {
    reader: ReaderHalf,
    writer: WriterHalf,
    peer_fingerprint: [u8; 32],
}

impl PayloadChannel {
    /// Accept an inbound payload connection (TLS server role — the side that
    /// announced the transfer).
    pub async fn accept(tcp: TcpStream, pair: Arc<CertPair>) -> Result<Self> {
        let peer_addr = tcp.peer_addr()?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config(&pair)?));
        let tls: ServerTlsStream<TcpStream> = acceptor.accept(tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Server(Box::new(tls)));
        debug!(%peer_addr, "payload channel accepted (tls server)");
        Ok(Self {
            reader,
            writer,
            peer_fingerprint,
        })
    }

    /// Dial a payload endpoint (TLS client role — the side that fetches).
    pub async fn connect(addr: impl ToSocketAddrs, pair: Arc<CertPair>) -> Result<Self> {
        let tcp = TcpStream::connect(addr).await?;
        let peer_addr = tcp.peer_addr()?;
        let connector = TlsConnector::from(Arc::new(client_config(&pair)?));
        let name = ServerName::from(peer_addr.ip());
        let tls: ClientTlsStream<TcpStream> = connector.connect(name, tcp).await?;
        let (_, conn) = tls.get_ref();
        let peer_fingerprint = fingerprint_of_first(conn.peer_certificates())?;
        let (reader, writer) = tokio::io::split(Wire::Client(Box::new(tls)));
        debug!(%peer_addr, "payload channel connected (tls client)");
        Ok(Self {
            reader,
            writer,
            peer_fingerprint,
        })
    }

    /// SHA-256 of the peer end-entity certificate (DER), for pinning.
    pub fn peer_fingerprint(&self) -> [u8; 32] {
        self.peer_fingerprint
    }

    /// Split into raw halves for unframed streaming.
    pub(crate) fn into_raw(self) -> (ReaderHalf, WriterHalf) {
        (self.reader, self.writer)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use handfast_protocol::Packet;
    use std::path::PathBuf;
    use tokio::io::AsyncReadExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("handfast-tls-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn payload_channel_streams_raw_bytes_and_pins_fingerprints() {
        let alice_dir = temp_dir("alice");
        let bob_dir = temp_dir("bob");
        let alice = Arc::new(CertPair::load_or_generate(&alice_dir, "alice-device").unwrap());
        let bob = Arc::new(CertPair::load_or_generate(&bob_dir, "bob-device").unwrap());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let payload: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        let expected_len = payload.len();
        let alice_for_task = alice.clone();
        let payload_for_task = payload.clone();

        let alice_fp = handfast_protocol::tls::cert_fingerprint(&alice.cert_der).unwrap();
        let bob_fp = handfast_protocol::tls::cert_fingerprint(&bob.cert_der).unwrap();

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let channel = PayloadChannel::accept(tcp, alice_for_task).await.unwrap();
            let peer = channel.peer_fingerprint();
            let (mut reader, mut writer) = channel.into_raw();
            let mut received = Vec::with_capacity(expected_len);
            let mut buf = vec![0u8; 4096];
            while received.len() < expected_len {
                let n = reader.read(&mut buf).await.unwrap();
                assert!(n > 0, "payload stream ended early");
                received.extend_from_slice(&buf[..n]);
            }
            assert_eq!(received, payload_for_task);
            writer.write_all(b"ack").await.unwrap();
            writer.shutdown().await.unwrap();
            peer
        });

        let channel = PayloadChannel::connect(addr, bob).await.unwrap();
        assert_eq!(
            channel.peer_fingerprint(),
            alice_fp,
            "client must observe the server's certificate"
        );
        let (mut reader, mut writer) = channel.into_raw();
        writer.write_all(&payload).await.unwrap();
        writer.flush().await.unwrap();
        let mut ack = [0u8; 3];
        reader.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"ack");

        let server_seen = server.await.unwrap();
        assert_eq!(
            server_seen, bob_fp,
            "server must observe the client's certificate"
        );

        let _ = std::fs::remove_dir_all(alice_dir);
        let _ = std::fs::remove_dir_all(bob_dir);
    }

    #[tokio::test]
    async fn control_channel_round_trips_newline_framed_packets_over_tls() {
        let alice_dir = temp_dir("alice2");
        let bob_dir = temp_dir("bob2");
        let alice = Arc::new(CertPair::load_or_generate(&alice_dir, "alice-device").unwrap());
        let bob = Arc::new(CertPair::load_or_generate(&bob_dir, "bob-device").unwrap());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let alice_for_task = alice.clone();

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut transport = Transport::accept(tcp, alice_for_task).await.unwrap();
            let packet = transport.read_packet().await.unwrap();
            (packet, transport)
        });

        let mut transport = Transport::connect(addr, bob).await.unwrap();
        let announce = Packet::new(
            handfast_protocol::TYPE_SHARE,
            serde_json::json!({ "filename": "photo.jpg" }),
        )
        .with_payload(1_000_000, 1743);
        transport.write_packet(&announce).await.unwrap();

        let (received, mut server_transport) = server.await.unwrap();
        assert_eq!(received.ty(), handfast_protocol::TYPE_SHARE);
        assert_eq!(received.body["filename"], "photo.jpg");
        assert_eq!(received.payload_size, Some(1_000_000));
        assert_eq!(received.payload_port(), Some(1743));

        // Framing must still be intact for a follow-up packet on the same
        // connection (the reader stays aligned after the first line).
        let ping = Packet::new(handfast_protocol::TYPE_PING, serde_json::json!({ "n": 1 }));
        server_transport.write_packet(&ping).await.unwrap();
        let got_ping = transport.read_packet().await.unwrap();
        assert_eq!(got_ping.ty(), handfast_protocol::TYPE_PING);

        let _ = std::fs::remove_dir_all(alice_dir);
        let _ = std::fs::remove_dir_all(bob_dir);
    }
}
