//! The payload byte mover.
//!
//! A share payload is a separate TLS session with the same long-term device
//! certificate as the control link. The sender listens on the first free port
//! from 1739 through 1764 and is the TLS server; the receiver dials the
//! control link's peer address at the offered port and is the TLS client. Both
//! sides compare the peer certificate DER exactly with the certificate the
//! control link authenticated. There is no application framing: the sender
//! writes raw bytes and closes, and the receiver reads until EOF.
//!
//! [`PayloadService::prepare_send`] reserves a port and holds the source before
//! the caller sends the packet, so the advertised port is the bound port.
//! [`PayloadService::connect`] holds a raw TCP connection for a queued file and
//! [`PayloadService::receive_pending`] starts the TLS handshake when the
//! previous file finishes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_proto::{DeviceId, PayloadOffer, PayloadSize};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::error::TlsError;
use crate::identity::{PeerCertificate, TlsIdentity};
use crate::link::{bind_tcp_in_range, peer_certificate_der};
use crate::tls::{client_config, server_config};

/// The authenticated peer for one payload transfer.
///
/// The certificate is the one the control link authenticated. The payload TLS
/// session must present exactly this DER certificate.
#[derive(Debug, Clone)]
pub struct PayloadPeer {
    /// Authenticated device ID, used for logging.
    pub device_id: DeviceId,
    /// Authenticated control link address. The payload dials this IP at the
    /// offered port; the address is routing, not authentication.
    pub address: SocketAddr,
    /// Certificate the control link authenticated for this peer.
    pub certificate: PeerCertificate,
}

/// Timeouts, caps and pool sizes for the payload service.
#[derive(Debug, Clone)]
pub struct PayloadLimits {
    /// TCP connect deadline for a payload dial.
    pub connect_timeout: Duration,
    /// TLS handshake deadline. Inside Android's 10 s payload timeout.
    pub handshake_timeout: Duration,
    /// Accept deadline for a sender, measured from bind.
    pub accept_timeout: Duration,
    /// Per-read and per-write deadline. Inside Android's 10 s payload timeout.
    pub io_timeout: Duration,
    /// Maximum accepted connections per sender before a failed handshake is
    /// reported.
    pub max_attempts: usize,
    /// Absolute byte cap, including accepted extra bytes.
    pub max_size: u64,
    /// Extra bytes accepted beyond the declared size.
    pub max_extra: u64,
    /// Stream buffer size.
    pub buffer_size: usize,
    /// Maximum concurrent active transfers.
    pub max_active: usize,
    /// Maximum eager raw TCP connections held for queued files.
    pub max_pending_connects: usize,
    /// Lowest payload port a sender binds and a receiver accepts.
    pub port_min: u16,
    /// Highest payload port a sender binds and a receiver accepts.
    pub port_max: u16,
}

impl Default for PayloadLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(8),
            accept_timeout: Duration::from_secs(30),
            io_timeout: Duration::from_secs(8),
            max_attempts: 3,
            max_size: 2 * 1024 * 1024 * 1024,
            max_extra: 64 * 1024 * 1024,
            buffer_size: 64 * 1024,
            max_active: 8,
            max_pending_connects: 8,
            port_min: handfast_proto::PAYLOAD_PORT_MIN,
            port_max: handfast_proto::PAYLOAD_PORT_MAX,
        }
    }
}

/// The bytes one sender writes, or one receiver reads.
pub struct PayloadSource {
    /// Declared size, or `None` when the length is unknown.
    pub size: Option<u64>,
    /// Byte source. A known size is checked against the bytes read.
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
}

/// The bytes one transfer moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferOutcome {
    /// Total bytes written to the sink, including extra bytes.
    pub bytes: u64,
    /// Bytes received beyond the declared size. Always 0 for a sender.
    pub extra_bytes: u64,
}

/// Errors from the payload service.
#[derive(Debug, thiserror::Error)]
pub enum PayloadError {
    /// No payload port in the configured range is free.
    #[error("no payload port available")]
    NoPort,
    /// The offered port is outside the configured payload range.
    #[error("payload port {0} is outside 1739-1764")]
    PortOutOfRange(u16),
    /// The payload certificate is not the certificate the control link
    /// authenticated.
    #[error("payload peer certificate mismatch")]
    PeerCertificate,
    /// Fewer bytes arrived than the declared size.
    #[error("payload size mismatch: expected {expected}, got {got}")]
    SizeMismatch {
        /// Declared size.
        expected: u64,
        /// Bytes actually read.
        got: u64,
    },
    /// The payload exceeds the byte cap.
    #[error("payload exceeds the {0} byte cap")]
    TooLarge(u64),
    /// A phase did not finish in time.
    #[error("payload timeout: {0}")]
    Timeout(&'static str),
    /// TLS configuration or certificate extraction failed.
    #[error("payload tls: {0}")]
    Tls(#[from] TlsError),
    /// Socket or stream IO failed.
    #[error("payload io: {0}")]
    Io(#[from] std::io::Error),
}

/// The payload byte mover.
///
/// One service is shared by every session. It holds the daemon's long-term TLS
/// identity, the limits, and the active-transfer and pending-connect pools.
pub struct PayloadService {
    tls: Arc<TlsIdentity>,
    limits: PayloadLimits,
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
}

impl PayloadService {
    /// Build a service over the daemon's TLS identity.
    ///
    /// The identity is the control link's identity; the payload never generates
    /// a second certificate.
    #[must_use]
    pub fn new(tls: Arc<TlsIdentity>, limits: PayloadLimits) -> Self {
        Self {
            active: Arc::new(Semaphore::new(limits.max_active)),
            pending: Arc::new(Semaphore::new(limits.max_pending_connects)),
            tls,
            limits,
        }
    }

    /// Reserve a payload port and hold the source.
    ///
    /// The caller puts [`PreparedSend::port`] in the packet, sends the packet,
    /// then calls [`PreparedSend::start`]. The accept deadline is measured from
    /// bind, not from `start`. Dropping the prepared send closes the listener
    /// and releases the active-transfer permit.
    ///
    /// # Errors
    ///
    /// Fails when the active-transfer pool stays full for the accept deadline,
    /// or when every port in the configured range is taken.
    pub async fn prepare_send(
        &self,
        peer: PayloadPeer,
        source: PayloadSource,
    ) -> Result<PreparedSend, PayloadError> {
        let permit = acquire_permit(
            &self.active,
            self.limits.accept_timeout,
            "payload active pool",
        )
        .await?;
        let listener = bind_tcp_in_range(self.limits.port_min, self.limits.port_max)
            .map_err(|_| PayloadError::NoPort)?;
        let port = listener.local_addr()?.port();
        tracing::debug!(peer = %peer.device_id, port, "payload listener bound");
        Ok(PreparedSend {
            listener,
            source,
            peer,
            tls: Arc::clone(&self.tls),
            limits: self.limits.clone(),
            port,
            bound_at: Instant::now(),
            _permit: permit,
        })
    }

    /// Dial the peer's payload port and hold the raw TCP connection.
    ///
    /// The TLS handshake is deferred until [`Self::receive_pending`], so a
    /// queued file can connect before the previous file finishes. The
    /// pending-connect permit is held by the returned value.
    ///
    /// # Errors
    ///
    /// Fails when the offered port is outside the configured payload range,
    /// when the pending-connect pool stays full for the connect deadline, or
    /// when the dial fails or times out.
    pub async fn connect(
        &self,
        peer: PayloadPeer,
        offer: PayloadOffer,
    ) -> Result<PendingPayload, PayloadError> {
        if !(self.limits.port_min..=self.limits.port_max).contains(&offer.port) {
            return Err(PayloadError::PortOutOfRange(offer.port));
        }
        let permit = acquire_permit(
            &self.pending,
            self.limits.connect_timeout,
            "payload pending pool",
        )
        .await?;
        let address = SocketAddr::new(peer.address.ip(), offer.port);
        tracing::debug!(peer = %peer.device_id, %address, "payload dial");
        let tcp = timeout(self.limits.connect_timeout, TcpStream::connect(address))
            .await
            .map_err(|_| PayloadError::Timeout("payload connect"))??;
        tcp.set_nodelay(true)?;
        Ok(PendingPayload {
            tcp,
            peer,
            offer,
            connected_at: Instant::now(),
            _permit: permit,
        })
    }

    /// Run the TLS client handshake on a held connection and stream into
    /// `sink`.
    ///
    /// The pending-connect permit is released before the active-transfer
    /// permit is acquired. Progress is best effort: a full channel drops the
    /// update, not the transfer.
    ///
    /// # Errors
    ///
    /// Fails on a certificate mismatch, a handshake or read timeout, a sink
    /// write error, a short payload, or a payload beyond the byte cap.
    pub async fn receive_pending(
        &self,
        pending: PendingPayload,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError> {
        let PendingPayload {
            tcp,
            peer,
            offer,
            connected_at,
            _permit,
        } = pending;
        drop(_permit);
        tracing::debug!(
            held = ?connected_at.elapsed(),
            "payload tls handshake starts"
        );
        let _active = acquire_permit(
            &self.active,
            self.limits.handshake_timeout,
            "payload active pool",
        )
        .await?;
        let connector = TlsConnector::from(client_config(&self.tls)?);
        let server_name = ServerName::IpAddress(peer.address.ip().into());
        let stream = timeout(
            self.limits.handshake_timeout,
            connector.connect(server_name, tcp),
        )
        .await
        .map_err(|_| PayloadError::Timeout("payload handshake"))??;
        let mut stream: TlsStream<TcpStream> = stream.into();
        let certificate = peer_certificate_der(&stream)?;
        if certificate != peer.certificate.der() {
            return Err(PayloadError::PeerCertificate);
        }
        let outcome =
            read_payload(&mut stream, sink, offer, &self.limits, progress.as_ref()).await?;
        tracing::debug!(
            peer = %peer.device_id,
            bytes = outcome.bytes,
            extra = outcome.extra_bytes,
            "payload received"
        );
        Ok(outcome)
    }

    /// Connect and receive in one call.
    ///
    /// # Errors
    ///
    /// Fails with every error of [`Self::connect`] and
    /// [`Self::receive_pending`].
    pub async fn receive(
        &self,
        peer: PayloadPeer,
        offer: PayloadOffer,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError> {
        let pending = self.connect(peer, offer).await?;
        self.receive_pending(pending, sink, progress).await
    }
}

/// A bound payload listener and the source it will send.
///
/// Dropping this value before [`Self::start`] closes the listener and releases
/// the active-transfer permit.
pub struct PreparedSend {
    listener: TcpListener,
    source: PayloadSource,
    peer: PayloadPeer,
    tls: Arc<TlsIdentity>,
    limits: PayloadLimits,
    port: u16,
    bound_at: Instant,
    _permit: OwnedSemaphorePermit,
}

impl PreparedSend {
    /// The bound port to put in `payloadTransferInfo.port`.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Accept one authenticated receiver and stream the source.
    ///
    /// The accept deadline is the bind time plus
    /// [`PayloadLimits::accept_timeout`]. A failed handshake is discarded and
    /// retried until the deadline or the attempt limit. A source whose byte
    /// count does not match its declared size fails with
    /// [`PayloadError::SizeMismatch`].
    ///
    /// # Errors
    ///
    /// Fails when the bind-time deadline has expired, no receiver connects in
    /// time, every handshake attempt fails, the peer certificate does not
    /// match, or the source cannot be read or written.
    pub async fn start(
        mut self,
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError> {
        let deadline = self.bound_at + self.limits.accept_timeout;
        let acceptor = TlsAcceptor::from(server_config(&self.tls)?);
        let mut last_error = None;
        for _ in 0..self.limits.max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PayloadError::Timeout("payload accept"));
            }
            let accepted = timeout(remaining, self.listener.accept()).await;
            let (tcp, _) = match accepted {
                Err(_) => return Err(PayloadError::Timeout("payload accept")),
                Ok(Err(error)) => return Err(PayloadError::Io(error)),
                Ok(Ok(accepted)) => accepted,
            };
            match self.handshake(tcp, &acceptor).await {
                Ok(stream) => return self.send_source(stream, progress).await,
                Err(error) => {
                    tracing::debug!(%error, "payload attempt failed");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or(PayloadError::Timeout("payload accept")))
    }

    async fn handshake(
        &self,
        tcp: TcpStream,
        acceptor: &TlsAcceptor,
    ) -> Result<TlsStream<TcpStream>, PayloadError> {
        tcp.set_nodelay(true)?;
        let stream = timeout(self.limits.handshake_timeout, acceptor.accept(tcp))
            .await
            .map_err(|_| PayloadError::Timeout("payload handshake"))??;
        let stream: TlsStream<TcpStream> = stream.into();
        let certificate = peer_certificate_der(&stream)?;
        if certificate != self.peer.certificate.der() {
            return Err(PayloadError::PeerCertificate);
        }
        Ok(stream)
    }

    async fn send_source(
        &mut self,
        mut stream: TlsStream<TcpStream>,
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError> {
        let expected = self.source.size;
        let outcome = write_source(
            &mut stream,
            &mut *self.source.reader,
            &self.limits,
            progress.as_ref(),
        )
        .await?;
        if let Some(expected) = expected {
            if outcome.bytes != expected {
                return Err(PayloadError::SizeMismatch {
                    expected,
                    got: outcome.bytes,
                });
            }
        }
        tracing::debug!(bytes = outcome.bytes, "payload sent");
        Ok(outcome)
    }
}

/// A raw TCP payload connection held for a queued file.
///
/// Dropping this value closes the connection and releases the pending-connect
/// permit. No TLS state and no destination file exist until
/// [`PayloadService::receive_pending`] runs.
pub struct PendingPayload {
    tcp: TcpStream,
    peer: PayloadPeer,
    offer: PayloadOffer,
    connected_at: Instant,
    _permit: OwnedSemaphorePermit,
}

async fn acquire_permit(
    pool: &Arc<Semaphore>,
    wait: Duration,
    name: &'static str,
) -> Result<OwnedSemaphorePermit, PayloadError> {
    let permit = timeout(wait, Arc::clone(pool).acquire_owned())
        .await
        .map_err(|_| PayloadError::Timeout(name))?
        .map_err(|_| PayloadError::Io(std::io::Error::other("payload pool closed")))?;
    Ok(permit)
}

async fn write_source(
    stream: &mut TlsStream<TcpStream>,
    reader: &mut (dyn AsyncRead + Send + Unpin),
    limits: &PayloadLimits,
    progress: Option<&mpsc::Sender<u64>>,
) -> Result<TransferOutcome, PayloadError> {
    let mut buffer = vec![0u8; limits.buffer_size];
    let mut written = 0u64;
    loop {
        let read = timeout(limits.io_timeout, reader.read(&mut buffer))
            .await
            .map_err(|_| PayloadError::Timeout("payload source read"))??;
        if read == 0 {
            break;
        }
        timeout(limits.io_timeout, stream.write_all(&buffer[..read]))
            .await
            .map_err(|_| PayloadError::Timeout("payload write"))??;
        written += read as u64;
        if let Some(progress) = progress {
            let _ = progress.try_send(written);
        }
    }
    timeout(limits.io_timeout, stream.shutdown())
        .await
        .map_err(|_| PayloadError::Timeout("payload close"))??;
    Ok(TransferOutcome {
        bytes: written,
        extra_bytes: 0,
    })
}

async fn read_payload(
    stream: &mut TlsStream<TcpStream>,
    sink: &mut (dyn AsyncWrite + Unpin + Send),
    offer: PayloadOffer,
    limits: &PayloadLimits,
    progress: Option<&mpsc::Sender<u64>>,
) -> Result<TransferOutcome, PayloadError> {
    let cap = match offer.size {
        PayloadSize::Known(size) => size.saturating_add(limits.max_extra).min(limits.max_size),
        PayloadSize::Unknown => limits.max_size,
    };
    let mut buffer = vec![0u8; limits.buffer_size];
    let mut total = 0u64;
    loop {
        let read = timeout(limits.io_timeout, stream.read(&mut buffer))
            .await
            .map_err(|_| PayloadError::Timeout("payload read"))??;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > cap {
            return Err(PayloadError::TooLarge(cap));
        }
        sink.write_all(&buffer[..read]).await?;
        if let Some(progress) = progress {
            let _ = progress.try_send(total);
        }
    }
    sink.flush().await?;
    match offer.size {
        PayloadSize::Known(expected) if total < expected => Err(PayloadError::SizeMismatch {
            expected,
            got: total,
        }),
        PayloadSize::Known(expected) => Ok(TransferOutcome {
            bytes: total,
            extra_bytes: total - expected,
        }),
        PayloadSize::Unknown => Ok(TransferOutcome {
            bytes: total,
            extra_bytes: 0,
        }),
    }
}
