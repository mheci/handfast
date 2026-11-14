//! An independent payload sender and receiver for share tests.
//!
//! The sender binds a TCP listener and is the TLS server; the receiver dials and
//! is the TLS client. This reverses the control link's roles, matching KDE
//! Connect. The payload has no application framing: the sender writes raw bytes
//! and closes, and the receiver reads to EOF.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::frame::{Result, TestkitError};
use crate::peer::PeerConfig;
use crate::tls_peer::{
    negotiated_name, negotiated_version, CertificateKind, TlsPeerConfig, TlsVersion,
};

/// Default deadline for the sender to accept one payload connection.
pub const DEFAULT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default payload TLS handshake and idle write timeout.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(8);
/// Default receiver read and handshake timeout.
pub const DEFAULT_RECEIVER_TIMEOUT: Duration = Duration::from_secs(10);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BUFFER_SIZE: usize = 64 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Configuration for one testkit payload sender.
#[derive(Debug, Clone)]
pub struct PayloadSenderConfig {
    /// Identity used for the peer certificate CN and TLS configuration.
    pub identity: PeerConfig,
    /// TLS versions the sender offers.
    pub version: TlsVersion,
    /// Certificate family the sender presents.
    pub certificate: CertificateKind,
    /// Present a different ECDSA certificate with a `wrong-` CN.
    pub wrong_certificate: bool,
    /// Write only this many payload bytes. `None` writes the whole payload.
    pub truncate_after: Option<u64>,
    /// Write this many extra zero bytes after the payload.
    pub extra_bytes: u64,
    /// Deadline for a receiver to connect, measured from bind.
    pub accept_timeout: Duration,
    /// TLS handshake and idle write timeout.
    pub idle_timeout: Duration,
    /// Bind this exact loopback port instead of an ephemeral one. Tests use a
    /// port in the payload range so the production receiver accepts it.
    pub bind_port: Option<u16>,
    /// Wait this long after the handshake before writing payload bytes. Tests
    /// use it to make the production receiver's idle read timeout fire.
    pub stall_before_write: Option<Duration>,
    /// Hold the second payload until the first `accept_and_send` returns.
    pub pause_second_until_first_completes: bool,
    /// Reject a peer certificate other than this DER certificate.
    pub expected_peer_certificate: Option<Vec<u8>>,
}

impl PayloadSenderConfig {
    /// Build a sender config with the default knobs and timeouts.
    #[must_use]
    pub fn new(identity: PeerConfig, version: TlsVersion) -> Self {
        Self {
            identity,
            version,
            ..Self::default()
        }
    }
}

impl Default for PayloadSenderConfig {
    fn default() -> Self {
        Self {
            identity: PeerConfig::client(),
            version: TlsVersion::Both,
            certificate: CertificateKind::Ecdsa,
            wrong_certificate: false,
            truncate_after: None,
            extra_bytes: 0,
            accept_timeout: DEFAULT_ACCEPT_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            bind_port: None,
            stall_before_write: None,
            pause_second_until_first_completes: false,
            expected_peer_certificate: None,
        }
    }
}

/// A bound testkit payload sender. It is the TLS server.
pub struct PayloadSender {
    listener: TcpListener,
    config: PayloadSenderConfig,
    acceptor: TlsAcceptor,
    cert_der: Vec<u8>,
    port: u16,
    bound_at: Instant,
    gate: Option<Mutex<()>>,
}

impl PayloadSender {
    /// Bind a loopback listener and build the TLS server configuration.
    ///
    /// # Errors
    ///
    /// Fails when the listener cannot bind, the certificate cannot be built, or
    /// the certificate and key are not a valid TLS pair.
    pub fn new(config: PayloadSenderConfig) -> Result<Self> {
        let tls = tls_config(
            &config.identity,
            config.version,
            config.certificate,
            config.wrong_certificate,
        )?;
        let acceptor = TlsAcceptor::from(Arc::new(tls.server_config()?));
        let cert_der = tls.cert_der().to_vec();
        let listener = std::net::TcpListener::bind(("127.0.0.1", config.bind_port.unwrap_or(0)))?;
        let port = listener.local_addr()?.port();
        let bound_at = Instant::now();
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let gate = if config.pause_second_until_first_completes {
            Some(Mutex::new(()))
        } else {
            None
        };
        Ok(Self {
            listener,
            config,
            acceptor,
            cert_der,
            port,
            bound_at,
            gate,
        })
    }

    /// The bound port, available before the sender accepts.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The DER certificate this sender presents.
    #[must_use]
    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// Accept one receiver, complete the TLS server handshake, and write the
    /// payload as raw bytes.
    ///
    /// # Errors
    ///
    /// Fails on the accept deadline, a TLS handshake failure or timeout, a
    /// certificate pin mismatch, or an idle write timeout.
    pub async fn accept_and_send(&self, payload: &[u8]) -> Result<PayloadSendReport> {
        let _guard = match &self.gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        let deadline = self.bound_at + self.config.accept_timeout;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TestkitError::Timeout);
        }
        let (tcp, _) = timeout(remaining, self.listener.accept())
            .await
            .map_err(|_| TestkitError::Timeout)??;
        tcp.set_nodelay(true)?;
        let stream = timeout(self.config.idle_timeout, self.acceptor.accept(tcp))
            .await
            .map_err(|_| TestkitError::Timeout)??;
        let mut stream: TlsStream<TcpStream> = stream.into();
        let negotiated = negotiated_name(negotiated_version(&stream)?);
        let peer_certificate_der = peer_certificate(&stream);
        check_peer_certificate(
            self.config.expected_peer_certificate.as_deref(),
            &peer_certificate_der,
        )?;
        if let Some(stall) = self.config.stall_before_write {
            tokio::time::sleep(stall).await;
        }

        let send_len = self.config.truncate_after.map_or(payload.len(), |limit| {
            payload
                .len()
                .min(usize::try_from(limit).unwrap_or(usize::MAX))
        });
        let write_result = match write_payload(
            &mut stream,
            &payload[..send_len],
            self.config.idle_timeout,
        )
        .await
        {
            Ok(()) => {
                write_extra(
                    &mut stream,
                    self.config.extra_bytes,
                    self.config.idle_timeout,
                )
                .await
            }
            Err(error) => Err(error),
        };
        match write_result {
            Ok(()) => {
                timeout(self.config.idle_timeout, stream.shutdown())
                    .await
                    .map_err(|_| TestkitError::Timeout)??;
            }
            Err(error) => {
                let _ = timeout(SHUTDOWN_GRACE, stream.shutdown()).await;
                return Err(error);
            }
        }

        Ok(PayloadSendReport {
            bytes: send_len as u64,
            extra_bytes: self.config.extra_bytes,
            negotiated,
            peer_certificate_der,
        })
    }
}

/// Result of one testkit payload send.
#[derive(Debug, Clone)]
pub struct PayloadSendReport {
    /// Payload bytes written before any extra bytes.
    pub bytes: u64,
    /// Extra zero bytes written after the payload.
    pub extra_bytes: u64,
    /// Negotiated TLS version name.
    pub negotiated: String,
    /// DER certificate the receiver presented.
    pub peer_certificate_der: Vec<u8>,
}

/// Configuration for one testkit payload receiver.
#[derive(Debug, Clone)]
pub struct PayloadReceiverConfig {
    /// Identity used for the peer certificate CN and TLS configuration.
    pub identity: PeerConfig,
    /// TLS versions the receiver offers.
    pub version: TlsVersion,
    /// Certificate family the receiver presents.
    pub certificate: CertificateKind,
    /// Present a different ECDSA certificate with a `wrong-` CN.
    pub wrong_certificate: bool,
    /// Present a client certificate. `false` models a client with none.
    pub present_client_certificate: bool,
    /// Dial and read raw bytes without TLS.
    pub raw_tcp: bool,
    /// Per-read and TLS handshake timeout.
    pub read_timeout: Duration,
    /// Wait this long after the handshake before reading payload bytes.
    pub stall_before_read: Option<Duration>,
    /// Reject a peer certificate other than this DER certificate.
    pub expected_peer_certificate: Option<Vec<u8>>,
}

impl PayloadReceiverConfig {
    /// Build a receiver config with the default knobs and timeouts.
    #[must_use]
    pub fn new(identity: PeerConfig, version: TlsVersion) -> Self {
        Self {
            identity,
            version,
            ..Self::default()
        }
    }
}

impl Default for PayloadReceiverConfig {
    fn default() -> Self {
        Self {
            identity: PeerConfig::server(),
            version: TlsVersion::Both,
            certificate: CertificateKind::Ecdsa,
            wrong_certificate: false,
            present_client_certificate: true,
            raw_tcp: false,
            read_timeout: DEFAULT_RECEIVER_TIMEOUT,
            stall_before_read: None,
            expected_peer_certificate: None,
        }
    }
}

/// A testkit payload receiver. It dials and is the TLS client.
pub struct PayloadReceiver {
    config: PayloadReceiverConfig,
    connector: Option<TlsConnector>,
    cert_der: Option<Vec<u8>>,
}

impl PayloadReceiver {
    /// Build the TLS client configuration.
    ///
    /// # Errors
    ///
    /// Fails when the certificate cannot be built or the TLS configuration is
    /// invalid.
    pub fn new(config: PayloadReceiverConfig) -> Result<Self> {
        if config.raw_tcp {
            return Ok(Self {
                config,
                connector: None,
                cert_der: None,
            });
        }
        let tls = tls_config(
            &config.identity,
            config.version,
            config.certificate,
            config.wrong_certificate,
        )?;
        let client_config = if config.present_client_certificate {
            tls.client_config()?
        } else {
            tls.client_config_without_certificate()?
        };
        let cert_der = if config.present_client_certificate {
            Some(tls.cert_der().to_vec())
        } else {
            None
        };
        Ok(Self {
            config,
            connector: Some(TlsConnector::from(Arc::new(client_config))),
            cert_der,
        })
    }

    /// The DER certificate this receiver presents, when it presents one.
    #[must_use]
    pub fn cert_der(&self) -> Option<&[u8]> {
        self.cert_der.as_deref()
    }

    /// Dial the sender, complete the TLS client handshake, and read raw bytes
    /// until EOF or the maximum size.
    ///
    /// # Errors
    ///
    /// Fails on a connect or handshake timeout, a certificate pin mismatch, a
    /// read timeout, or a payload larger than `max_size`.
    pub async fn receive<W>(
        &self,
        address: SocketAddr,
        max_size: u64,
        sink: &mut W,
    ) -> Result<PayloadReceiveReport>
    where
        W: AsyncWrite + Unpin + Send,
    {
        if let Some(connector) = &self.connector {
            let tcp = connect(address).await?;
            let server_name = ServerName::try_from("localhost")
                .map_err(|error| TestkitError::Protocol(format!("server name: {error}")))?;
            let stream = timeout(
                self.config.read_timeout,
                connector.connect(server_name, tcp),
            )
            .await
            .map_err(|_| TestkitError::Timeout)??;
            let mut stream: TlsStream<TcpStream> = stream.into();
            let negotiated = negotiated_name(negotiated_version(&stream)?);
            let peer_certificate_der = peer_certificate(&stream);
            check_peer_certificate(
                self.config.expected_peer_certificate.as_deref(),
                &peer_certificate_der,
            )?;
            if let Some(stall) = self.config.stall_before_read {
                tokio::time::sleep(stall).await;
            }
            let (bytes, eof) =
                read_to_sink(&mut stream, max_size, self.config.read_timeout, sink).await?;
            Ok(PayloadReceiveReport {
                bytes,
                negotiated,
                eof,
                peer_certificate_der,
            })
        } else {
            let mut tcp = connect(address).await?;
            if let Some(stall) = self.config.stall_before_read {
                tokio::time::sleep(stall).await;
            }
            let (bytes, eof) =
                read_to_sink(&mut tcp, max_size, self.config.read_timeout, sink).await?;
            Ok(PayloadReceiveReport {
                bytes,
                negotiated: "raw".to_string(),
                eof,
                peer_certificate_der: Vec::new(),
            })
        }
    }
}

/// Result of one testkit payload receive.
#[derive(Debug, Clone)]
pub struct PayloadReceiveReport {
    /// Bytes read from the stream.
    pub bytes: u64,
    /// Negotiated TLS version name, or `raw` for a plain TCP receiver.
    pub negotiated: String,
    /// Whether the stream ended with EOF before `max_size`.
    pub eof: bool,
    /// DER certificate the sender presented.
    pub peer_certificate_der: Vec<u8>,
}

/// One payload self-test case.
#[derive(Debug, Clone, Serialize)]
pub struct PayloadSelfTestCase {
    /// TLS version both peers were restricted to.
    pub version: TlsVersion,
    /// Certificate both peers presented.
    pub certificate: CertificateKind,
    /// Payload size in bytes.
    pub size: u64,
    /// Negotiated TLS version.
    pub negotiated: String,
    /// Bytes the sender wrote.
    pub bytes_sent: u64,
    /// Bytes the receiver read.
    pub bytes_received: u64,
    /// Whether the receiver saw EOF.
    pub eof: bool,
}

/// Report from [`run_payload_self_test`].
#[derive(Debug, Clone, Serialize)]
pub struct PayloadSelfTestReport {
    /// One case per TLS version, certificate family and payload size.
    pub cases: Vec<PayloadSelfTestCase>,
}

/// Run loopback payload transfers for 0, 1, 1 MiB and 8 MiB over TLS 1.2 and
/// TLS 1.3 with ECDSA and RSA certificates.
///
/// # Errors
///
/// Fails on the first case that does not transfer the exact bytes and reach
/// EOF, naming the case.
pub async fn run_payload_self_test() -> Result<PayloadSelfTestReport> {
    let mut cases = Vec::new();
    for version in [TlsVersion::Tls12, TlsVersion::Tls13] {
        for certificate in [CertificateKind::Ecdsa, CertificateKind::Rsa] {
            for size in [0usize, 1, 1024 * 1024, 8 * 1024 * 1024] {
                let case = run_payload_case(version, certificate, size)
                    .await
                    .map_err(|error| {
                        TestkitError::Protocol(format!(
                            "{version:?} {certificate:?} {size} bytes: {error}"
                        ))
                    })?;
                cases.push(case);
            }
        }
    }
    Ok(PayloadSelfTestReport { cases })
}

async fn run_payload_case(
    version: TlsVersion,
    certificate: CertificateKind,
    size: usize,
) -> Result<PayloadSelfTestCase> {
    let payload: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    let sender = PayloadSender::new(PayloadSenderConfig {
        version,
        certificate,
        ..PayloadSenderConfig::default()
    })?;
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        version,
        certificate,
        expected_peer_certificate: Some(sender.cert_der().to_vec()),
        ..PayloadReceiverConfig::default()
    })?;
    let receiver_cert = receiver
        .cert_der()
        .ok_or_else(|| TestkitError::Protocol("receiver has no certificate".to_string()))?
        .to_vec();
    let address = SocketAddr::from(([127, 0, 0, 1], sender.port()));
    let mut buffer = Vec::new();
    let (send_result, receive_result) = tokio::join!(
        sender.accept_and_send(&payload),
        receiver.receive(address, size as u64, &mut buffer),
    );
    let send = send_result?;
    let receive = receive_result?;

    if send.bytes != size as u64 {
        return Err(TestkitError::Protocol(format!(
            "sender wrote {} bytes, expected {size}",
            send.bytes
        )));
    }
    if receive.bytes != size as u64 {
        return Err(TestkitError::Protocol(format!(
            "receiver read {} bytes, expected {size}",
            receive.bytes
        )));
    }
    if !receive.eof {
        return Err(TestkitError::Protocol(
            "receiver did not reach EOF".to_string(),
        ));
    }
    if buffer != payload {
        return Err(TestkitError::Protocol(
            "received bytes differ from the sent payload".to_string(),
        ));
    }
    let expected_version = match version {
        TlsVersion::Tls12 => "TLSv1.2",
        TlsVersion::Tls13 => "TLSv1.3",
        TlsVersion::Both => {
            return Err(TestkitError::Protocol(
                "self-test must pin a TLS version".to_string(),
            ));
        }
    };
    if receive.negotiated != expected_version {
        return Err(TestkitError::Protocol(format!(
            "negotiated {}, expected {expected_version}",
            receive.negotiated
        )));
    }
    if send.negotiated != receive.negotiated {
        return Err(TestkitError::Protocol(format!(
            "sender negotiated {}, receiver negotiated {}",
            send.negotiated, receive.negotiated
        )));
    }
    if send.peer_certificate_der != receiver_cert {
        return Err(TestkitError::Protocol(
            "sender saw an unexpected receiver certificate".to_string(),
        ));
    }
    if receive.peer_certificate_der.as_slice() != sender.cert_der() {
        return Err(TestkitError::Protocol(
            "receiver saw an unexpected sender certificate".to_string(),
        ));
    }

    Ok(PayloadSelfTestCase {
        version,
        certificate,
        size: size as u64,
        negotiated: receive.negotiated,
        bytes_sent: send.bytes,
        bytes_received: receive.bytes,
        eof: receive.eof,
    })
}

fn tls_config(
    identity: &PeerConfig,
    version: TlsVersion,
    certificate: CertificateKind,
    wrong_certificate: bool,
) -> Result<TlsPeerConfig> {
    // A wrong certificate is a fresh ECDSA certificate with a `wrong-` CN, so it
    // differs from both the ECDSA identity certificate and the RSA fixture.
    if wrong_certificate {
        let device_id = &identity.device_id;
        let common_name = format!("wrong-{device_id}");
        return TlsPeerConfig::ecdsa_with_common_name(identity.clone(), version, &common_name);
    }
    match certificate {
        CertificateKind::Ecdsa => TlsPeerConfig::ecdsa(identity.clone(), version),
        CertificateKind::Rsa => Ok(TlsPeerConfig::rsa(version)),
    }
}

async fn connect(address: SocketAddr) -> Result<TcpStream> {
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| TestkitError::Timeout)??;
    tcp.set_nodelay(true)?;
    Ok(tcp)
}

async fn write_payload<W: AsyncWrite + Unpin>(
    stream: &mut W,
    payload: &[u8],
    idle_timeout: Duration,
) -> Result<()> {
    for chunk in payload.chunks(BUFFER_SIZE) {
        timeout(idle_timeout, stream.write_all(chunk))
            .await
            .map_err(|_| TestkitError::Timeout)??;
    }
    timeout(idle_timeout, stream.flush())
        .await
        .map_err(|_| TestkitError::Timeout)??;
    Ok(())
}

async fn write_extra<W: AsyncWrite + Unpin>(
    stream: &mut W,
    extra: u64,
    idle_timeout: Duration,
) -> Result<()> {
    let zeros = [0u8; BUFFER_SIZE];
    let mut remaining = extra;
    while remaining > 0 {
        let chunk = remaining.min(BUFFER_SIZE as u64) as usize;
        timeout(idle_timeout, stream.write_all(&zeros[..chunk]))
            .await
            .map_err(|_| TestkitError::Timeout)??;
        remaining -= chunk as u64;
    }
    Ok(())
}

async fn read_to_sink<R, W>(
    reader: &mut R,
    max_size: u64,
    read_timeout: Duration,
    sink: &mut W,
) -> Result<(u64, bool)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut total = 0u64;
    loop {
        let read = timeout(read_timeout, reader.read(&mut buffer))
            .await
            .map_err(|_| TestkitError::Timeout)??;
        if read == 0 {
            sink.flush().await?;
            return Ok((total, true));
        }
        total += read as u64;
        if total > max_size {
            return Err(TestkitError::TooLarge(max_size));
        }
        sink.write_all(&buffer[..read]).await?;
    }
}

fn peer_certificate(stream: &TlsStream<TcpStream>) -> Vec<u8> {
    stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .map(|certificate| certificate.as_ref().to_vec())
        .unwrap_or_default()
}

fn check_peer_certificate(expected: Option<&[u8]>, actual: &[u8]) -> Result<()> {
    match expected {
        Some(expected) if expected != actual => Err(TestkitError::PeerCertificate),
        _ => Ok(()),
    }
}
