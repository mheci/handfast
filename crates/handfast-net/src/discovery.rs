//! UDP discovery on port 1716.

use std::net::{Ipv4Addr, SocketAddr};

use handfast_proto::{DiscoveryAnnouncement, Identity, Packet};
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

    /// Receive the next valid discovery announcement.
    ///
    /// Accepts the desktop minimal packet and the Android full identity
    /// packet. Malformed datagrams and ports outside the KDE Connect range are
    /// ignored. Wrap this call in a timeout to bound the wait.
    ///
    /// # Errors
    ///
    /// Fails when the socket read fails.
    pub async fn recv(&self) -> Result<(SocketAddr, DiscoveryAnnouncement)> {
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
            match DiscoveryAnnouncement::from_packet(&packet) {
                Ok(announcement) => return Ok((address, announcement)),
                Err(error) => {
                    tracing::debug!(%address, %error, "ignoring invalid discovery announcement");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{types, Capabilities, DeviceId, DeviceName, DeviceType, Identity};
    use std::time::Duration;

    fn identity(name: &str) -> Identity {
        let capabilities = Capabilities::new([types::PING], [types::PING]).unwrap();
        Identity::new(
            DeviceId::generate(),
            DeviceName::new(name),
            DeviceType::Desktop,
            capabilities,
            None,
        )
    }

    async fn bind_receiver() -> (Discovery, SocketAddr) {
        let receiver = Discovery::bind(identity("receiver"), 0, 1716)
            .await
            .unwrap();
        let port = receiver.local_addr().unwrap().port();
        (receiver, SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    }

    fn desktop_datagram(port: u16) -> String {
        format!(
            r#"{{"id":0,"type":"kdeconnect.identity","body":{{"deviceId":"0f1e2d3c4b5a69788796a5b4c3d2e1f0","deviceName":"kde-desktop","protocolVersion":8,"tcpPort":{port}}}}}"#
        )
    }

    #[tokio::test]
    async fn full_identity_announcement_round_trip() {
        let (receiver, target) = bind_receiver().await;
        let sender = Discovery::bind(identity("sender"), 0, 1716).await.unwrap();
        sender.announce(target).await.unwrap();

        let (address, received) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(address.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(received.device_name.as_str(), "sender");
        assert_eq!(received.device_type, DeviceType::Desktop);
        assert_eq!(received.tcp_port, 1716);
    }

    #[tokio::test]
    async fn desktop_four_field_datagram_is_accepted() {
        let (receiver, target) = bind_receiver().await;
        let raw = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        raw.send_to(desktop_datagram(1716).as_bytes(), target)
            .await
            .unwrap();

        let (_, received) = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            received.device_id.as_str(),
            "0f1e2d3c4b5a69788796a5b4c3d2e1f0"
        );
        assert_eq!(received.device_name.as_str(), "kde-desktop");
        assert_eq!(received.device_type, DeviceType::Unknown);
        assert_eq!(received.tcp_port, 1716);
    }

    #[tokio::test]
    async fn malformed_datagrams_are_ignored() {
        let (receiver, target) = bind_receiver().await;
        let raw = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        for datagram in [
            "{not json".as_bytes(),
            br#"{"type":"kdeconnect.ping","body":{}}"#.as_slice(),
            br#"{"type":"kdeconnect.identity","body":{"deviceId":"short","protocolVersion":8,"tcpPort":1716}}"#.as_slice(),
        ] {
            raw.send_to(datagram, target).await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), receiver.recv())
                .await
                .is_err(),
            "a malformed datagram was accepted"
        );
    }

    #[tokio::test]
    async fn out_of_range_ports_are_ignored() {
        let (receiver, target) = bind_receiver().await;
        let raw = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        for port in [0, 1715, 1765, u16::MAX] {
            raw.send_to(desktop_datagram(port).as_bytes(), target)
                .await
                .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), receiver.recv())
                .await
                .is_err(),
            "an out-of-range port was accepted"
        );
    }

    #[test]
    fn well_known_port_matches_upstream() {
        assert_eq!(DISCOVERY_PORT, 1716);
    }
}
