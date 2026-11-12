//! UDP discovery on port 1716.

use std::net::{Ipv4Addr, SocketAddr};

use handfast_proto::{Identity, Packet};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::error::Result;

/// KDE Connect's well-known discovery port.
pub const DISCOVERY_PORT: u16 = 1716;

/// UDP discovery socket bound to a local port.
pub struct Discovery {
    socket: UdpSocket,
    identity: Identity,
    port: u16,
}

impl Discovery {
    /// Bind a discovery socket.
    ///
    /// The identity's `tcpPort` is set to `tcp_port` when it is missing.
    ///
    /// # Errors
    ///
    /// Fails when the UDP socket cannot be configured or bound.
    pub async fn bind(mut identity: Identity, port: u16, tcp_port: u16) -> Result<Self> {
        if identity.tcp_port.is_none() {
            identity.tcp_port = Some(tcp_port);
        }
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_broadcast(true)?;
        socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
        socket.set_nonblocking(true)?;
        let std_socket: std::net::UdpSocket = socket.into();
        let socket = UdpSocket::from_std(std_socket)?;
        Ok(Self {
            socket,
            identity,
            port,
        })
    }

    /// Local socket address.
    ///
    /// # Errors
    ///
    /// Fails when the socket is no longer bound.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// The identity this socket announces.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Send one discovery announcement to a target.
    ///
    /// # Errors
    ///
    /// Fails when the identity cannot be encoded or the datagram is rejected.
    pub async fn announce(&self, target: SocketAddr) -> Result<()> {
        let packet = self.identity.to_packet()?;
        let bytes = serde_json::to_vec(&packet)?;
        self.socket.send_to(&bytes, target).await?;
        Ok(())
    }

    /// Send an announcement to the IPv4 broadcast address.
    ///
    /// # Errors
    ///
    /// Fails when the datagram cannot be sent.
    pub async fn announce_broadcast(&self) -> Result<()> {
        self.announce(SocketAddr::from((Ipv4Addr::BROADCAST, self.port)))
            .await
    }

    /// Send an announcement to every interface broadcast address, then the
    /// limited broadcast address as a fallback.
    ///
    /// # Errors
    ///
    /// Fails when interfaces cannot be read or a datagram cannot be sent.
    pub async fn announce_interfaces(&self) -> Result<()> {
        for broadcast in interface_broadcasts()? {
            self.announce(SocketAddr::from((broadcast, self.port)))
                .await?;
        }
        self.announce_broadcast().await
    }

    /// Receive the next valid identity announcement.
    ///
    /// Malformed datagrams are ignored. Wrap this call in a timeout to bound
    /// the wait.
    ///
    /// # Errors
    ///
    /// Fails when the socket read fails.
    pub async fn recv(&self) -> Result<(SocketAddr, Identity)> {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let (size, address) = self.socket.recv_from(&mut buffer).await?;
            let packet: Packet = match serde_json::from_slice(&buffer[..size]) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::debug!(%address, %error, "ignoring malformed discovery datagram");
                    continue;
                }
            };
            match Identity::from_packet(&packet) {
                Ok(identity) => return Ok((address, identity)),
                Err(error) => {
                    tracing::debug!(%address, %error, "ignoring non-identity discovery packet");
                }
            }
        }
    }
}

fn interface_broadcasts() -> Result<Vec<Ipv4Addr>> {
    let interfaces =
        if_addrs::get_if_addrs().map_err(|error| NetError::Discovery(error.to_string()))?;
    let mut broadcasts = Vec::new();
    for interface in interfaces {
        if let if_addrs::IfAddr::V4(address) = interface.addr {
            if let Some(broadcast) = address.broadcast {
                broadcasts.push(broadcast);
            }
        }
    }
    Ok(broadcasts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{Capabilities, DeviceId, DeviceName, DeviceType, Identity};
    use std::time::Duration;

    fn identity(name: &str) -> Identity {
        let capabilities =
            Capabilities::new([handfast_proto::types::PING], [handfast_proto::types::PING])
                .unwrap();
        Identity::new(
            DeviceId::generate(),
            DeviceName::new(name),
            DeviceType::Desktop,
            capabilities,
            None,
        )
    }

    #[tokio::test]
    async fn announce_round_trip() {
        let receiver = Discovery::bind(identity("receiver"), 0, 1716)
            .await
            .unwrap();
        let sender = Discovery::bind(identity("sender"), 0, 1716).await.unwrap();
        let port = receiver.local_addr().unwrap().port();
        sender
            .announce(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .await
            .unwrap();
        let (address, received) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(address.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(received.device_name.as_str(), "sender");
        assert_eq!(received.tcp_port, Some(1716));
    }

    #[tokio::test]
    async fn malformed_datagrams_are_skipped() {
        let receiver = Discovery::bind(identity("receiver"), 0, 1716)
            .await
            .unwrap();
        let sender = Discovery::bind(identity("sender"), 0, 1716).await.unwrap();
        let port = receiver.local_addr().unwrap().port();
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

        let raw = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        raw.send_to(b"{not json", target).await.unwrap();
        sender.announce(target).await.unwrap();

        let (_, received) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.device_name.as_str(), "sender");
    }

    #[test]
    fn well_known_port_matches_upstream() {
        assert_eq!(DISCOVERY_PORT, 1716);
    }
}
