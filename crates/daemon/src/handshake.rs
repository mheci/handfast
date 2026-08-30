//! KDE Connect control-plane handshake (upstream-compatible).
//!
//! Upstream's rule — "if I'm the TCP server I will be the SSL client and
//! vice-versa" (android `LanLinkProvider` / kdeconnect-kde
//! `LanLinkProvider`) — plus the two-stage identity exchange:
//!
//! 1. **Plaintext identity.** The TCP *dialer* writes one
//!    `kdeconnect.identity` packet (JSON + `\n`) to the raw socket; the TCP
//!    *acceptor* reads exactly that line. This happens *before* any TLS.
//! 2. **TLS.** The dialer upgrades as the TLS **server**, the acceptor as the
//!    TLS **client** (note the inverted roles vs. a normal client/server).
//! 3. **Secure identity.** Immediately after the TLS handshake both sides
//!    send their identity again *inside* the encrypted channel (protocol ≥ 8)
//!    and read the peer's, write-then-read on both sides so neither blocks.
//!    Each side validates that the secure identity's `deviceId` and
//!    `protocolVersion` match the plaintext one (anti-downgrade / anti-spoof).
//!
//! [`accept_control`] implements stages 1–3 for the accepting side and
//! [`dial_control`] for the dialing side; both return the verified secure
//! identity plus the established [`Transport`].

use std::sync::Arc;
use std::time::Duration;

use handfast_core::error::{Error, Result};
use handfast_protocol::tls::CertPair;
use handfast_protocol::{Identity, Packet, MAX_PACKET_LEN, TYPE_IDENTITY};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::debug;

use crate::tls::Transport;

/// Generous bound on the post-handshake exchange; a peer that cannot
/// introduce itself within this window is not worth keeping.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Read one newline-delimited identity packet from a raw TCP stream **without
/// over-reading**, byte by byte, mirroring upstream's handshake read
/// (android `LanLinkProvider#tcpPacketReceived` deliberately reads the OS
/// buffer byte by byte so no TLS handshake bytes are ever swallowed).
///
/// The line must be a `kdeconnect.identity` packet whose body deserializes
/// into an [`Identity`]; anything else is a protocol violation. Bounded by
/// [`MAX_PACKET_LEN`] (512 KiB — the same cap as upstream's
/// `MAX_IDENTITY_PACKET_SIZE`). A first byte that is not `{` fails fast: a
/// peer that skips the plaintext step and sends TLS bytes instead is rejected
/// immediately rather than stalling the read until the timeout.
pub(crate) async fn read_plaintext_identity(tcp: &mut TcpStream) -> Result<Identity> {
    let mut line = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    let mut first = true;
    loop {
        let n = timeout(HANDSHAKE_TIMEOUT, tcp.read(&mut byte))
            .await
            .map_err(|_| Error::Other("peer never finished its identity line".into()))??;
        if n == 0 {
            return Err(Error::Other(
                "connection closed before the plaintext identity line".into(),
            ));
        }
        if first {
            if byte[0] != b'{' {
                return Err(Error::Other(
                    "peer did not start with a plaintext identity packet".into(),
                ));
            }
            first = false;
        }
        if line.len() >= MAX_PACKET_LEN {
            return Err(Error::Other(format!(
                "identity line exceeds MAX_PACKET_LEN ({MAX_PACKET_LEN})"
            )));
        }
        if byte[0] == b'\n' {
            let packet: Packet = serde_json::from_slice(&line).map_err(|err| {
                Error::Other(format!("malformed plaintext identity packet: {err}"))
            })?;
            if packet.ptype != TYPE_IDENTITY {
                return Err(Error::Other(format!(
                    "expected {TYPE_IDENTITY} as first packet, got {}",
                    packet.ptype
                )));
            }
            return serde_json::from_value(packet.body)
                .map_err(|err| Error::Other(format!("malformed identity body: {err}")));
        }
        line.push(byte[0]);
    }
}

/// Post-TLS secure identity exchange: write mine, read theirs (both sides
/// write first, so the ordering can never deadlock).
pub(crate) async fn exchange(
    mut transport: Transport,
    mine: &Identity,
) -> Result<(Identity, Transport)> {
    let hello = Packet::identity(mine.clone());
    transport.write_packet(&hello).await?;

    let packet = timeout(HANDSHAKE_TIMEOUT, transport.read_packet())
        .await
        .map_err(|_| Error::Other("identity exchange timed out".into()))??;

    if packet.ptype != TYPE_IDENTITY {
        return Err(Error::Other(format!(
            "expected {TYPE_IDENTITY} as first packet, got {}",
            packet.ptype
        )));
    }
    let theirs: Identity = serde_json::from_value(packet.body)
        .map_err(|err| Error::Other(format!("malformed identity body: {err}")))?;
    if theirs.device_id.is_empty() {
        return Err(Error::Other("peer identity lacks deviceId".into()));
    }
    debug!(device = %theirs.device_id, "secure identity exchanged");
    Ok((theirs, transport))
}

/// Anti-downgrade / anti-spoof check, mirroring android
/// `LanLinkProvider#identityPacketReceived`: the identity re-transmitted
/// inside TLS must agree with the one read in plaintext.
fn cross_check(plain: &Identity, secure: &Identity) -> Result<()> {
    if plain.device_id != secure.device_id {
        return Err(Error::Other(format!(
            "device id changed during handshake: {} -> {}",
            plain.device_id, secure.device_id
        )));
    }
    if plain.protocol_version != secure.protocol_version {
        return Err(Error::Other(format!(
            "protocol version changed during handshake: {} -> {}",
            plain.protocol_version, secure.protocol_version
        )));
    }
    Ok(())
}

/// Accept an inbound control connection end-to-end (stages 1–3): read the
/// peer's plaintext identity, upgrade as TLS client, exchange the secure
/// identity, and cross-check. Returns the verified secure identity and the
/// transport.
pub async fn accept_control(
    tcp: TcpStream,
    pair: Arc<CertPair>,
    mine: &Identity,
) -> Result<(Identity, Transport)> {
    let mut tcp = tcp;
    let plain = read_plaintext_identity(&mut tcp).await?;
    let transport = Transport::accept_control(tcp, pair).await?;
    let (secure, transport) = exchange(transport, mine).await?;
    cross_check(&plain, &secure)?;
    debug!(device = %secure.device_id, "inbound control handshake complete");
    Ok((secure, transport))
}

/// Dial a control connection end-to-end (stages 1–3): write our plaintext
/// identity, upgrade as TLS server, exchange the secure identity. Returns
/// the peer's secure identity and the transport.
pub async fn dial_control(
    addr: impl tokio::net::ToSocketAddrs,
    pair: Arc<CertPair>,
    mine: &Identity,
) -> Result<(Identity, Transport)> {
    let transport = Transport::connect_control(addr, pair, mine).await?;
    let (secure, transport) = exchange(transport, mine).await?;
    debug!(device = %secure.device_id, "outbound control handshake complete");
    Ok((secure, transport))
}
