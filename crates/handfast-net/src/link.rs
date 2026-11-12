//! Plain TCP link with protocol framing.

use std::net::SocketAddr;

use handfast_proto::{frame, Packet};
use tokio::net::{TcpListener, TcpStream};

use crate::error::Result;

/// A framed TCP link to one peer.
pub struct Link {
    stream: TcpStream,
    peer: SocketAddr,
}

impl Link {
    /// Dial a peer.
    ///
    /// # Errors
    ///
    /// Fails when the TCP connection cannot be established.
    pub async fn connect(address: SocketAddr) -> Result<Self> {
        let stream = TcpStream::connect(address).await?;
        let peer = stream.peer_addr()?;
        Ok(Self { stream, peer })
    }

    /// Accept one peer from a listener.
    ///
    /// # Errors
    ///
    /// Fails when the listener cannot accept a connection.
    pub async fn accept(listener: &TcpListener) -> Result<Self> {
        let (stream, peer) = listener.accept().await?;
        Ok(Self { stream, peer })
    }

    /// Remote address.
    #[must_use]
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Send one packet.
    ///
    /// # Errors
    ///
    /// Fails when the frame cannot be encoded or written.
    pub async fn send(&mut self, packet: &Packet) -> Result<()> {
        frame::write_frame(&mut self.stream, packet).await?;
        Ok(())
    }

    /// Receive one packet, or `None` on clean EOF.
    ///
    /// # Errors
    ///
    /// Fails on a truncated or malformed frame.
    pub async fn recv(&mut self) -> Result<Option<Packet>> {
        Ok(frame::read_frame(&mut self.stream).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::types;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn link_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut link = Link::accept(&listener).await.unwrap();
            let packet = link.recv().await.unwrap().unwrap();
            assert_eq!(packet.ptype, types::PING);
            link.send(&Packet::empty(types::PING).unwrap())
                .await
                .unwrap();
        });

        let mut client = Link::connect(address).await.unwrap();
        client
            .send(&Packet::empty(types::PING).unwrap())
            .await
            .unwrap();
        let reply = client.recv().await.unwrap().unwrap();
        assert_eq!(reply.ptype, types::PING);
        server.await.unwrap();
    }
}
