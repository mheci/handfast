//! The newline-delimited TLS link.
//!
//! KDE Connect reverses the usual TLS roles. The dialer sends a plaintext
//! identity hello, then acts as the TLS server. The listener reads the hello,
//! then acts as the TLS client. Both sides parse the peer certificate, check
//! its common name and pin, and correlate the encrypted identity with the
//! plaintext one before any packet is read or written. Every packet on the
//! wire, the hello included, is one JSON line terminated by `\n`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use handfast_proto::{
    DeviceId, Identity, IdentityHello, Packet, ProtoError, MAX_TCP_PORT, MIN_TCP_PORT,
};
use rustls::pki_types::ServerName;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    ReadHalf, WriteHalf,
};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::error::{NetError, Result, TlsError};
use crate::identity::{LocalEndpoint, PeerCertificate, PeerTrust, TlsIdentity, TrustTable};
use crate::policy::{AdmissionPermit, LinkPolicy};
use crate::tls::{client_config, server_config};

/// Timeouts and admission limits for one link.
///
/// The manager owns these values in production. The link unit uses the
/// connection and handshake timeouts; the admission fields are carried here so
/// the manager and policy unit can read one struct.
#[derive(Debug, Clone)]
pub struct LinkLimits {
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Plaintext hello and TLS handshake timeout.
    pub handshake_timeout: Duration,
    /// Encrypted identity read and write timeout.
    pub identity_timeout: Duration,
    /// One packet line write timeout.
    pub write_timeout: Duration,
    /// Maximum concurrent inbound handshakes.
    pub max_pending: usize,
    /// Maximum tracked discovery peers.
    pub max_known_peers: usize,
    /// Discovery peer expiry.
    pub peer_ttl: Duration,
    /// Maximum established unpaired links.
    pub max_unpaired_links: usize,
    /// Idle timeout for an unpaired link.
    pub unpaired_idle_timeout: Duration,
    /// Minimum interval between inbound connections from one IP.
    pub per_ip_interval: Duration,
    /// Maximum tracked inbound rate-limit entries.
    pub rate_limit_entries: usize,
    /// TCP keepalive idle time.
    pub keepalive_idle: Duration,
    /// TCP keepalive probe interval.
    pub keepalive_interval: Duration,
    /// TCP keepalive probe count.
    pub keepalive_retries: u32,
    /// `TCP_USER_TIMEOUT` where the platform exposes it.
    pub tcp_user_timeout: Duration,
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            identity_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(10),
            max_pending: 64,
            max_known_peers: 256,
            peer_ttl: Duration::from_secs(10 * 60),
            max_unpaired_links: 42,
            unpaired_idle_timeout: Duration::from_secs(60),
            per_ip_interval: Duration::from_millis(500),
            rate_limit_entries: 1024,
            keepalive_idle: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(10),
            keepalive_retries: 3,
            tcp_user_timeout: Duration::from_secs(60),
        }
    }
}

/// A bound TCP listener in the KDE Connect port range.
pub struct LinkListener {
    listener: TcpListener,
    tls: Arc<TlsIdentity>,
}

impl LinkListener {
    /// Bind the first free port from `start` through 1764.
    ///
    /// # Errors
    ///
    /// Fails when `start` is outside 1716-1764 or every port in the range is
    /// taken.
    pub async fn bind_in_range(start: u16, tls: Arc<TlsIdentity>) -> Result<Self> {
        if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&start) {
            return Err(NetError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("TCP port {start} is outside {MIN_TCP_PORT}-{MAX_TCP_PORT}"),
            )));
        }

        let listener = bind_tcp_in_range(start, MAX_TCP_PORT).map_err(NetError::Io)?;
        Ok(Self { listener, tls })
    }

    /// The bound address.
    ///
    /// # Errors
    ///
    /// Fails when the listener no longer has a local address.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// The listener's TLS material, for handshake tasks that run outside the
    /// accept loop.
    #[must_use]
    pub(crate) fn tls(&self) -> &Arc<TlsIdentity> {
        &self.tls
    }

    /// Accept one TCP connection without running the handshake.
    ///
    /// The manager owns the pending cap and runs the handshake in its own task.
    pub(crate) async fn accept_tcp(&self) -> Result<TcpStream> {
        let (tcp, _) = self.listener.accept().await?;
        Ok(tcp)
    }

    /// Accept one dialer and complete the handshake.
    ///
    /// The dialer's plaintext hello selects the expected device ID. Its
    /// certificate is checked against that ID and the pin in `trust`. The
    /// encrypted identity must then agree with both. The admission policy is
    /// asked for a permit after the hello and before TLS.
    ///
    /// # Errors
    ///
    /// Fails on IO, a malformed hello, a hello that targets another device, an
    /// admission rejection, a TLS failure, a certificate or pin mismatch, an
    /// identity mismatch or a timeout.
    pub async fn accept(
        &self,
        local: &LocalEndpoint,
        trust: &TrustTable,
        limits: &LinkLimits,
        policy: &LinkPolicy,
    ) -> Result<Link> {
        let tcp = self.accept_tcp().await?;
        accept_handshake(tcp, &self.tls, local, trust, limits, policy).await
    }
}

pub(crate) async fn accept_handshake(
    mut tcp: TcpStream,
    tls: &Arc<TlsIdentity>,
    local: &LocalEndpoint,
    trust_table: &TrustTable,
    limits: &LinkLimits,
    policy: &LinkPolicy,
) -> Result<Link> {
    if tls.fingerprint() != local.tls().fingerprint() {
        return Err(NetError::Tls(TlsError::Certificate(
            "listener certificate does not match the local endpoint".to_string(),
        )));
    }

    configure_socket(&tcp, limits)?;
    let remote_addr = tcp.peer_addr()?;

    let hello = read_hello(&mut tcp, limits).await?;
    if &hello.target_device_id != local.device_id() {
        return Err(NetError::Proto(handfast_proto::ProtoError::Identity(
            format!(
                "hello targets {}, expected {}",
                hello.target_device_id,
                local.device_id()
            ),
        )));
    }
    let peer_id = hello.identity.device_id;
    let trust = trust_table.trust(&peer_id);
    let permit = policy.reserve_incoming(remote_addr.ip(), &peer_id, trust)?;

    let connector = TlsConnector::from(client_config(local.tls())?);
    let server_name = ServerName::IpAddress(remote_addr.ip().into());
    let stream = timeout(
        limits.handshake_timeout,
        connector.connect(server_name, tcp),
    )
    .await
    .map_err(|_| NetError::Timeout("tls handshake"))??;
    let mut stream: TlsStream<TcpStream> = stream.into();
    tracing::debug!(
        %remote_addr,
        version = ?stream.get_ref().1.protocol_version(),
        "TLS handshake complete"
    );

    let certificate = peer_certificate(&stream)?;
    certificate.check(&peer_id, trust)?;
    let identity = exchange_identity(
        &mut stream,
        local.identity(),
        &certificate,
        &peer_id,
        limits,
    )
    .await?;

    let (reader, writer) = tokio::io::split(stream);
    Ok(Link {
        peer: LinkedPeer {
            identity,
            certificate,
            remote_addr,
        },
        reader: LinkReader {
            reader: BufReader::new(reader),
        },
        writer: LinkWriter {
            writer,
            write_timeout: limits.write_timeout,
            line: Vec::new(),
        },
        permit,
    })
}

/// Bind the first free IPv4 port from `min` through `max`.
///
/// The link listener and the payload sender share this helper so both use the
/// same `SO_REUSEADDR`, backlog and last-error handling.
pub(crate) fn bind_tcp_in_range(min: u16, max: u16) -> std::io::Result<TcpListener> {
    let mut last_error = None;
    for port in min..=max {
        let address = SocketAddr::from(([0, 0, 0, 0], port));
        match bind_one(address) {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no free TCP port")
    }))
}

fn bind_one(address: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    // SO_REUSEADDR lets a restart rebind a port whose old connections are in
    // TIME_WAIT. It is Unix only; on Windows it would allow socket hijacking.
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    socket.bind(address)?;
    socket.listen(1024)
}

/// Dial a peer and complete the handshake.
///
/// The dialer sends the plaintext identity hello and acts as the TLS server.
/// The listener's certificate is checked against `expected` and `trust`, then
/// the encrypted identity must agree with both. The caller reserves `permit`
/// before dialing; it is held by the link and released on close.
///
/// # Errors
///
/// Fails when the target is this device, the TCP connection or TLS handshake
/// fails, the certificate or pin does not match, the encrypted identity does
/// not match, or a phase times out.
pub async fn connect(
    address: SocketAddr,
    local: &LocalEndpoint,
    expected: &DeviceId,
    trust: PeerTrust,
    limits: &LinkLimits,
    permit: AdmissionPermit,
) -> Result<Link> {
    if expected == local.device_id() {
        return Err(NetError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to dial this device",
        )));
    }

    let mut tcp = timeout(limits.connect_timeout, TcpStream::connect(address))
        .await
        .map_err(|_| NetError::Timeout("connect"))??;
    let remote_addr = tcp.peer_addr()?;
    configure_socket(&tcp, limits)?;

    let hello = IdentityHello::new(local.identity().clone(), expected.clone()).to_packet()?;
    timeout(limits.handshake_timeout, write_hello(&mut tcp, &hello))
        .await
        .map_err(|_| NetError::Timeout("plaintext hello"))??;

    let acceptor = TlsAcceptor::from(server_config(local.tls())?);
    let stream = timeout(limits.handshake_timeout, acceptor.accept(tcp))
        .await
        .map_err(|_| NetError::Timeout("tls handshake"))??;
    let mut stream: TlsStream<TcpStream> = stream.into();
    tracing::debug!(
        %remote_addr,
        version = ?stream.get_ref().1.protocol_version(),
        "TLS handshake complete"
    );

    let certificate = peer_certificate(&stream)?;
    certificate.check(expected, trust)?;
    let identity = exchange_identity(
        &mut stream,
        local.identity(),
        &certificate,
        expected,
        limits,
    )
    .await?;

    let (reader, writer) = tokio::io::split(stream);
    Ok(Link {
        peer: LinkedPeer {
            identity,
            certificate,
            remote_addr,
        },
        reader: LinkReader {
            reader: BufReader::new(reader),
        },
        writer: LinkWriter {
            writer,
            write_timeout: limits.write_timeout,
            line: Vec::new(),
        },
        permit,
    })
}

/// An established, authenticated link.
pub struct Link {
    peer: LinkedPeer,
    reader: LinkReader,
    writer: LinkWriter,
    permit: AdmissionPermit,
}

impl Link {
    /// The authenticated peer.
    #[must_use]
    pub fn peer(&self) -> &LinkedPeer {
        &self.peer
    }

    /// Split the link into its peer, reader, writer and admission permit.
    ///
    /// The permit keeps the unpaired slot reserved while the caller owns it.
    #[must_use]
    pub fn into_parts(self) -> (LinkedPeer, LinkReader, LinkWriter, AdmissionPermit) {
        (self.peer, self.reader, self.writer, self.permit)
    }
}

/// The authenticated identity and certificate of the peer.
#[derive(Debug)]
pub struct LinkedPeer {
    /// Full identity from the encrypted exchange.
    pub identity: Identity,
    /// Certificate whose common name matched the identity.
    pub certificate: PeerCertificate,
    /// Remote socket address.
    pub remote_addr: SocketAddr,
}

impl LinkedPeer {
    /// The authenticated device ID.
    #[must_use]
    pub fn device_id(&self) -> &DeviceId {
        &self.identity.device_id
    }
}

/// The read half of an established link.
pub struct LinkReader {
    reader: BufReader<ReadHalf<TlsStream<TcpStream>>>,
}

impl LinkReader {
    /// Receive one newline-delimited packet, or `None` on clean EOF.
    ///
    /// # Errors
    ///
    /// Fails on a truncated or malformed line or an oversized line.
    pub async fn recv(&mut self) -> Result<Option<Packet>> {
        read_buffered_line(&mut self.reader, MAX_LINK_LINE).await
    }
}

/// The write half of an established link.
pub struct LinkWriter {
    writer: WriteHalf<TlsStream<TcpStream>>,
    write_timeout: Duration,
    line: Vec<u8>,
}

impl LinkWriter {
    /// Send one newline-delimited packet.
    ///
    /// # Errors
    ///
    /// Fails when the line cannot be encoded, the write times out or the
    /// stream rejects the write.
    pub async fn send(&mut self, packet: &Packet) -> Result<()> {
        timeout(
            self.write_timeout,
            send_line(&mut self.writer, &mut self.line, packet, MAX_LINK_LINE),
        )
        .await
        .map_err(|_| NetError::Timeout("line write"))??;
        Ok(())
    }

    /// Send TLS close-notify and close the link.
    pub async fn close(mut self) {
        let _ = self.writer.shutdown().await;
    }
}

/// Maximum plaintext hello line, matching Android's `readLineBounded` cap.
pub const MAX_HELLO_LINE: usize = 512 * 1024;

/// Maximum secure identity line, matching Android's `readLineBounded` cap.
pub const MAX_IDENTITY_LINE: usize = 512 * 1024;

/// Maximum packet line accepted after the hello.
pub const MAX_LINK_LINE: usize = 1024 * 1024;

/// Write one compact JSON line.
///
/// Android's `NetworkPacket.serialize()` appends `\n`. The hello is written by
/// `LanLinkProvider` and every later packet by `LanLink.sendPacket`, both with
/// no length prefix.
async fn send_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    line: &mut Vec<u8>,
    packet: &Packet,
    max: usize,
) -> Result<()> {
    let capacity = line.capacity();
    line.clear();
    packet.validate()?;
    if let Err(error) = serde_json::to_writer(&mut *line, packet) {
        line.clear();
        return Err(error.into());
    }
    let size = line.len();
    if size > max {
        *line = Vec::with_capacity(capacity);
        return Err(line_too_large(size, max));
    }
    line.push(b'\n');
    writer.write_all(line).await?;
    writer.flush().await?;
    Ok(())
}

/// Write the plaintext hello as one compact JSON line.
async fn write_hello<W: AsyncWrite + Unpin>(writer: &mut W, packet: &Packet) -> Result<()> {
    let mut line = Vec::new();
    send_line(writer, &mut line, packet, MAX_HELLO_LINE).await
}

async fn read_hello<R: AsyncRead + Unpin>(
    reader: &mut R,
    limits: &LinkLimits,
) -> Result<IdentityHello> {
    let packet = timeout(
        limits.handshake_timeout,
        read_line_unbuffered(reader, MAX_HELLO_LINE),
    )
    .await
    .map_err(|_| NetError::Timeout("plaintext hello"))??;
    Ok(IdentityHello::from_packet(&packet)?)
}

/// Read one newline-terminated JSON line, one byte at a time.
///
/// The hello leaves the raw socket to rustls, and the identity leaves the TLS
/// stream to the buffered link reader, so the read must stop exactly at the
/// newline. The loop also mirrors Android's `readLineBounded`.
async fn read_line_unbuffered<R: AsyncRead + Unpin>(reader: &mut R, max: usize) -> Result<Packet> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte).await? == 0 {
            return Err(NetError::LinkClosed);
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= max {
            return Err(line_too_large(line.len() + 1, max));
        }
        line.push(byte[0]);
    }
    decode_line(&line)
}

/// Read one newline-terminated JSON line through a buffered reader.
///
/// Returns `Ok(None)` on a clean EOF before the first byte.
async fn read_buffered_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Packet>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(NetError::LinkClosed)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if line.len() + take > max {
            return Err(line_too_large(line.len() + take, max));
        }
        line.extend_from_slice(&available[..take]);
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return decode_line(&line).map(Some);
        }
    }
}

fn decode_line(line: &[u8]) -> Result<Packet> {
    let packet: Packet = serde_json::from_slice(line).map_err(ProtoError::from)?;
    packet.validate()?;
    Ok(packet)
}

fn line_too_large(size: usize, max: usize) -> NetError {
    NetError::Proto(ProtoError::LineTooLarge { size, max })
}

async fn exchange_identity(
    stream: &mut TlsStream<TcpStream>,
    local: &Identity,
    certificate: &PeerCertificate,
    hello: &DeviceId,
    limits: &LinkLimits,
) -> Result<Identity> {
    let packet = local.to_packet()?;
    let mut line = Vec::new();
    timeout(
        limits.identity_timeout,
        send_line(stream, &mut line, &packet, MAX_IDENTITY_LINE),
    )
    .await
    .map_err(|_| NetError::Timeout("identity write"))??;
    let packet = timeout(
        limits.identity_timeout,
        read_line_unbuffered(stream, MAX_IDENTITY_LINE),
    )
    .await
    .map_err(|_| NetError::Timeout("identity read"))??;
    let identity = Identity::from_packet(&packet)?;
    certificate.check_identity(hello, &identity)?;
    Ok(identity)
}

pub(crate) fn peer_certificate_der(
    stream: &TlsStream<TcpStream>,
) -> std::result::Result<Vec<u8>, TlsError> {
    let certificates = stream
        .get_ref()
        .1
        .peer_certificates()
        .ok_or_else(|| TlsError::Certificate("peer presented no certificate".to_string()))?;
    if certificates.len() != 1 {
        return Err(TlsError::Certificate(format!(
            "expected one peer certificate, got {}",
            certificates.len()
        )));
    }
    Ok(certificates[0].as_ref().to_vec())
}

fn peer_certificate(stream: &TlsStream<TcpStream>) -> Result<PeerCertificate> {
    let der = peer_certificate_der(stream)?;
    PeerCertificate::parse(&der).map_err(NetError::Tls)
}

fn configure_socket(stream: &TcpStream, limits: &LinkLimits) -> Result<()> {
    stream.set_nodelay(true)?;
    let socket = SockRef::from(stream);
    let keepalive = TcpKeepalive::new()
        .with_time(limits.keepalive_idle)
        .with_interval(limits.keepalive_interval);
    // shortcut: keepalive retries are applied on Linux and Windows only; other
    // platforms keep the time and interval.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let keepalive = keepalive.with_retries(limits.keepalive_retries);
    socket.set_tcp_keepalive(&keepalive)?;
    #[cfg(any(
        target_os = "android",
        target_os = "fuchsia",
        target_os = "linux",
        target_os = "cygwin"
    ))]
    socket.set_tcp_user_timeout(Some(limits.tcp_user_timeout))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{types, ProtoError};
    use serde_json::json;
    use tokio::io::AsyncReadExt;

    /// Shaped like the phone's plaintext hello line.
    const ANDROID_HELLO_LINE: &str = concat!(
        r#"{"id":0,"type":"kdeconnect.identity","body":{"deviceId":"fedcba9876543210fedcba9876543210","#,
        r#""deviceName":"Pixel","deviceType":"phone","incomingCapabilities":["kdeconnect.ping"],"#,
        r#""outgoingCapabilities":["kdeconnect.ping"],"protocolVersion":8,"tcpPort":1716,"#,
        r#""targetDeviceId":"00112233445566778899aabbccddeeff","targetProtocolVersion":"8"}}"#,
    );

    /// Shaped like the phone's secure identity line.
    const ANDROID_IDENTITY_LINE: &str = concat!(
        r#"{"id":0,"type":"kdeconnect.identity","body":{"deviceId":"fedcba9876543210fedcba9876543210","#,
        r#""deviceName":"Pixel","deviceType":"phone","incomingCapabilities":["kdeconnect.ping"],"#,
        r#""outgoingCapabilities":["kdeconnect.ping"],"protocolVersion":8,"tcpPort":1716}}"#,
    );

    /// Shaped like the desktop connection packet.
    const DESKTOP_HELLO_LINE: &str = concat!(
        r#"{"id":0,"type":"kdeconnect.identity","body":{"deviceId":"fedcba9876543210fedcba9876543210","#,
        r#""deviceName":"Desktop","protocolVersion":8,"#,
        r#""targetDeviceId":"00112233445566778899aabbccddeeff","targetProtocolVersion":8}}"#,
    );

    fn ping_packet() -> Packet {
        Packet::from_body(types::PING, &json!({ "message": "hello" })).expect("build a ping")
    }

    #[test]
    fn line_caps_are_pinned() {
        assert_eq!(MAX_HELLO_LINE, 512 * 1024);
        assert_eq!(MAX_IDENTITY_LINE, 512 * 1024);
        assert_eq!(MAX_LINK_LINE, 1024 * 1024);
    }

    #[test]
    fn limits_default_to_the_desktop_values() {
        let limits = LinkLimits::default();
        assert_eq!(limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(limits.handshake_timeout, Duration::from_secs(10));
        assert_eq!(limits.identity_timeout, Duration::from_secs(5));
        assert_eq!(limits.write_timeout, Duration::from_secs(10));
        assert_eq!(limits.max_pending, 64);
        assert_eq!(limits.max_known_peers, 256);
        assert_eq!(limits.peer_ttl, Duration::from_secs(600));
        assert_eq!(limits.max_unpaired_links, 42);
        assert_eq!(limits.unpaired_idle_timeout, Duration::from_secs(60));
        assert_eq!(limits.per_ip_interval, Duration::from_millis(500));
        assert_eq!(limits.rate_limit_entries, 1024);
        assert_eq!(limits.keepalive_idle, Duration::from_secs(30));
        assert_eq!(limits.keepalive_interval, Duration::from_secs(10));
        assert_eq!(limits.keepalive_retries, 3);
        assert_eq!(limits.tcp_user_timeout, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn read_hello_accepts_a_raw_android_line() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(format!("{ANDROID_HELLO_LINE}\n").as_bytes())
            .await
            .expect("write the phone's hello line");

        let hello = read_hello(&mut server, &LinkLimits::default())
            .await
            .expect("the listener reads the phone's newline-terminated hello");

        assert_eq!(
            hello.identity.device_id.as_str(),
            "fedcba9876543210fedcba9876543210"
        );
        assert_eq!(
            hello.target_device_id.as_str(),
            "00112233445566778899aabbccddeeff"
        );
        assert_eq!(hello.identity.device_type.as_str(), "phone");
        assert_eq!(hello.target_protocol_version, 8);
    }

    #[tokio::test]
    async fn read_line_accepts_a_raw_android_secure_identity_line() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(format!("{ANDROID_IDENTITY_LINE}\n").as_bytes())
            .await
            .expect("write the phone's identity line");

        let packet = read_line_unbuffered(&mut server, MAX_IDENTITY_LINE)
            .await
            .expect("the listener reads the phone's identity line");
        let identity = Identity::from_packet(&packet).expect("the line is a secure identity");

        assert_eq!(
            identity.device_id.as_str(),
            "fedcba9876543210fedcba9876543210"
        );
        assert_eq!(identity.device_type.as_str(), "phone");
    }

    #[tokio::test]
    async fn read_hello_accepts_a_raw_desktop_line() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(format!("{DESKTOP_HELLO_LINE}\n").as_bytes())
            .await
            .expect("write the desktop hello line");

        let hello = read_hello(&mut server, &LinkLimits::default())
            .await
            .expect("the listener reads the desktop newline-terminated hello");

        assert_eq!(hello.identity.device_type.as_str(), "unknown");
        assert!(hello.identity.capabilities.incoming().is_empty());
        assert_eq!(hello.target_protocol_version, 8);
    }

    #[tokio::test]
    async fn read_hello_rejects_a_line_without_a_newline() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(DESKTOP_HELLO_LINE.as_bytes())
            .await
            .expect("write the desktop hello without its newline");
        drop(client);

        let error = read_hello(&mut server, &LinkLimits::default())
            .await
            .expect_err("a hello without a newline is rejected");
        assert!(matches!(error, NetError::LinkClosed), "{error}");
    }

    #[tokio::test]
    async fn read_hello_rejects_a_line_over_the_cap() {
        let (mut client, mut server) = tokio::io::duplex(MAX_HELLO_LINE + 2);
        client
            .write_all(&vec![b'x'; MAX_HELLO_LINE + 1])
            .await
            .expect("write an over-cap hello line");

        let error = read_hello(&mut server, &LinkLimits::default())
            .await
            .expect_err("an over-cap hello line is rejected");
        match error {
            NetError::Proto(ProtoError::LineTooLarge { size, max }) => {
                assert_eq!(size, MAX_HELLO_LINE + 1);
                assert_eq!(max, MAX_HELLO_LINE);
            }
            other => panic!("{other}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn keepalive_matches_the_desktop_values() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let address = listener.local_addr().expect("read the bound address");
        let connect = TcpStream::connect(address);
        let accept = listener.accept();
        let (client, accepted) = tokio::join!(connect, accept);
        let client = client.expect("connect the socket pair");
        let (server, _) = accepted.expect("accept the socket pair");

        configure_socket(&client, &LinkLimits::default()).expect("apply the socket options");
        let socket = SockRef::from(&client);
        assert!(socket.keepalive().expect("read SO_KEEPALIVE"));
        assert_eq!(
            socket.tcp_keepalive_time().expect("read TCP_KEEPIDLE"),
            Duration::from_secs(30)
        );
        assert_eq!(
            socket.tcp_keepalive_interval().expect("read TCP_KEEPINTVL"),
            Duration::from_secs(10)
        );
        assert_eq!(socket.tcp_keepalive_retries().expect("read TCP_KEEPCNT"), 3);
        assert_eq!(
            socket.tcp_user_timeout().expect("read TCP_USER_TIMEOUT"),
            Some(Duration::from_secs(60))
        );
        assert!(client.nodelay().expect("read TCP_NODELAY"));
        drop(server);
    }

    // socket2 applies Windows keepalive through SIO_KEEPALIVE_VALS and does
    // not expose getters for the time, interval or retry count. The flag and
    // TCP_NODELAY are the values Windows lets this test read back.
    #[cfg(windows)]
    #[tokio::test]
    async fn keepalive_is_enabled_on_windows() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let address = listener.local_addr().expect("read the bound address");
        let connect = TcpStream::connect(address);
        let accept = listener.accept();
        let (client, accepted) = tokio::join!(connect, accept);
        let client = client.expect("connect the socket pair");
        let (server, _) = accepted.expect("accept the socket pair");

        configure_socket(&client, &LinkLimits::default()).expect("apply the socket options");
        let socket = SockRef::from(&client);
        assert!(socket.keepalive().expect("read SO_KEEPALIVE"));
        assert!(client.nodelay().expect("read TCP_NODELAY"));
        drop(server);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reuse_address_rebinds_a_released_port() {
        use tokio::io::AsyncReadExt;

        let device_id =
            DeviceId::new("00112233445566778899aabbccddeeff").expect("the test device ID is valid");
        let tls = Arc::new(TlsIdentity::generate(device_id).expect("generate the local identity"));
        let first = LinkListener::bind_in_range(1716, Arc::clone(&tls))
            .await
            .expect("bind the first listener");
        let port = first
            .local_addr()
            .expect("read the first bound address")
            .port();
        let address = SocketAddr::from(([127, 0, 0, 1], port));

        let client = TcpStream::connect(address)
            .await
            .expect("connect to the first listener");
        let (mut server, _) = first.listener.accept().await.expect("accept the client");
        server
            .shutdown()
            .await
            .expect("close the server side first");
        drop(server);
        drop(first);

        let mut client = client;
        let mut buffer = [0u8; 1];
        assert_eq!(
            client
                .read(&mut buffer)
                .await
                .expect("read the server close"),
            0,
            "the client sees the server close"
        );
        drop(client);

        let second = LinkListener::bind_in_range(port, tls)
            .await
            .expect("rebind the released port");
        assert_eq!(
            second
                .local_addr()
                .expect("read the second bound address")
                .port(),
            port,
            "the port is rebound while the old connection is closing"
        );
    }

    #[tokio::test]
    async fn send_line_writes_a_newline_terminated_packet() {
        let packet = ping_packet();
        let mut expected = serde_json::to_vec(&packet).expect("serialize the expected line");
        expected.push(b'\n');
        let (mut client, mut server) = tokio::io::duplex(4096);
        let mut line = Vec::new();

        send_line(&mut client, &mut line, &packet, MAX_LINK_LINE)
            .await
            .expect("write the line");

        let mut written = vec![0u8; expected.len()];
        server
            .read_exact(&mut written)
            .await
            .expect("read the line");
        assert_eq!(written, expected, "the line is compact JSON plus a newline");
        assert_eq!(
            written.first(),
            Some(&b'{'),
            "the line has no length prefix"
        );
    }

    #[tokio::test]
    async fn send_line_reuses_the_buffer_without_leaking_between_packets() {
        let large = Packet::from_body(types::PING, &json!({ "blob": "x".repeat(64 * 1024) }))
            .expect("build a large ping");
        let small = Packet::empty(types::PING).expect("build a small ping");
        let mut large_line = serde_json::to_vec(&large).expect("serialize the large line");
        large_line.push(b'\n');
        let mut small_line = serde_json::to_vec(&small).expect("serialize the small line");
        small_line.push(b'\n');
        let (mut client, mut server) = tokio::io::duplex(256 * 1024);
        let mut line = Vec::new();

        send_line(&mut client, &mut line, &large, MAX_LINK_LINE)
            .await
            .expect("write the large line");
        let capacity = line.capacity();
        let pointer = line.as_ptr();
        for _ in 0..100 {
            send_line(&mut client, &mut line, &small, MAX_LINK_LINE)
                .await
                .expect("write a small line");
            assert_eq!(line.as_ptr(), pointer, "the line buffer is reused");
            assert_eq!(line.capacity(), capacity, "the line buffer does not grow");
        }
        assert_eq!(line, small_line, "the last line carries no old bytes");

        let mut written = vec![0u8; large_line.len() + 100 * small_line.len()];
        server
            .read_exact(&mut written)
            .await
            .expect("read every line");
        assert_eq!(&written[..large_line.len()], &large_line[..]);
        for chunk in written[large_line.len()..].chunks(small_line.len()) {
            assert_eq!(chunk, &small_line[..], "each small line is byte-identical");
        }
    }

    #[tokio::test]
    async fn send_line_rejects_an_oversized_packet_before_writing() {
        let packet = Packet::new(types::PING, json!({ "blob": "x".repeat(MAX_LINK_LINE) }))
            .expect("build an oversized ping");
        let (mut client, mut server) = tokio::io::duplex(64);
        let mut line = Vec::new();

        let error = send_line(&mut client, &mut line, &packet, MAX_LINK_LINE)
            .await
            .expect_err("the line limit rejects the packet");
        match error {
            NetError::Proto(ProtoError::LineTooLarge { size, max }) => {
                assert!(size > MAX_LINK_LINE, "{size}");
                assert_eq!(max, MAX_LINK_LINE);
            }
            other => panic!("{other}"),
        }
        assert!(line.is_empty(), "no partial line stays in the buffer");

        drop(client);
        let mut written = Vec::new();
        server
            .read_to_end(&mut written)
            .await
            .expect("read the peer side");
        assert!(written.is_empty(), "no bytes reach the peer");
    }
}
