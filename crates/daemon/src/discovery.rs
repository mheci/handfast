//! UDP discovery — the KDE Connect broadcast protocol.
//!
//! Every few seconds a discoverable device broadcasts its [`Identity`] as a
//! single UTF-8 JSON datagram to the subnet broadcast address on UDP port
//! 1716. A listener that receives such a datagram learns:
//!
//! * the sender's IP (from the datagram source),
//! * the TCP port of its TLS server (`tcpSourcePort`),
//! * its capabilities.
//!
//! and may then open an outgoing TLS connection. This module only transports
//! identities; trust decisions live in [`crate::device`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use handfast_core::error::Result;
use handfast_protocol::{Identity, Packet, TYPE_IDENTITY, UDP_BROADCAST_PORT};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// How often the daemon announces itself while discoverable.
pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5);
/// Maximum accepted discovery datagram size; identities are tiny.
const MAX_DATAGRAM: usize = 4096;

/// Events produced by the discovery loop for the device manager.
#[derive(Debug, Clone)]
pub struct PeerAnnouncement {
    /// Source IP of the announcing peer.
    pub ip: IpAddr,
    /// Identity carried by the datagram (includes `tcpSourcePort`).
    pub identity: Identity,
}

/// Bind the discovery socket on all IPv4 interfaces with broadcast enabled.
///
/// Binding port 1716 exclusively matches upstream; when another Handfast or
/// KDE Connect instance already holds it we surface the error to the caller
/// instead of degrading silently.
pub async fn bind() -> Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, UDP_BROADCAST_PORT)).await?;
    socket.set_broadcast(true)?;
    Ok(socket)
}

/// Send one identity announcement to the IPv4 limited broadcast address.
///
/// Wraps the identity in a `kdeconnect.identity` packet envelope so peers
/// (including the Android KDE Connect app) can parse it as a standard packet.
pub async fn announce(socket: &UdpSocket, identity: &Identity) -> Result<()> {
    let packet = Packet::identity(identity.clone());
    let payload = serde_json::to_vec(&packet)
        .map_err(|err| handfast_core::error::Error::Other(err.to_string()))?;
    let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), UDP_BROADCAST_PORT);
    // Broadcast sends can fail with EACCES/ECONNRESET on odd interfaces; a
    // failed announce must never kill the supervised task, hence the warn.
    if let Err(err) = socket.send_to(&payload, target).await {
        warn!(%err, "broadcast announce failed");
    }
    Ok(())
}

/// Receive loop: yields every valid foreign identity announcement.
///
/// Own announcements (matched by `self_device_id`) and malformed datagrams
/// are skipped silently at debug level — the network is hostile territory.
/// Also emits periodic self-announcements so peers can find us even when
/// they started first.
pub async fn run(
    socket: UdpSocket,
    identity: Identity,
    self_device_id: String,
    tx: tokio::sync::mpsc::Sender<PeerAnnouncement>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(ANNOUNCE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    info!(port = UDP_BROADCAST_PORT, "udp discovery running");
    loop {
        tokio::select! {
            _ = ticker.tick() => announce(&socket, &identity).await?,
            received = recv_one(&socket, &self_device_id) => match received {
                Some(peer) => {
                    if tx.send(peer).await.is_err() {
                        return Ok(()); // manager gone: end supervision cleanly
                    }
                }
                None => continue,
            },
        }
    }
}

async fn recv_one(socket: &UdpSocket, self_device_id: &str) -> Option<PeerAnnouncement> {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        let (len, source) = match socket.recv_from(&mut buf).await {
            Ok(result) => result,
            Err(err) => {
                debug!(%err, "udp recv failed");
                continue;
            }
        };
        if len == 0 || len > MAX_DATAGRAM {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf[..len]) else {
            debug!(%source, "dropping non-utf8 datagram");
            continue;
        };
        let parsed = {
            // Primary: KDE Connect packet envelope {id,type,body}.
            // Fallback: bare Identity (older Handfast instances) for rollout compat.
            let packet = serde_json::from_str::<Packet>(text).ok();
            if let Some(packet) = packet.filter(|p| p.ptype == TYPE_IDENTITY) {
                match serde_json::from_value::<Identity>(packet.body) {
                    Ok(identity) => identity,
                    Err(_) => {
                        debug!(%source, "dropping identity packet with malformed body");
                        continue;
                    }
                }
            } else if let Ok(identity) = serde_json::from_str::<Identity>(text) {
                identity
            } else {
                debug!(%source, "dropping malformed identity datagram");
                continue;
            }
        };
        if parsed.device_id == self_device_id {
            continue;
        }
        if parsed.tcp_source_port == 0 {
            debug!(device = %parsed.device_id, "announcement lacks tcpSourcePort");
            continue;
        }
        return Some(PeerAnnouncement {
            ip: source.ip(),
            identity: parsed,
        });
    }
}
