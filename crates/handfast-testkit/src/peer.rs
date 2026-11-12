//! An Android-shaped Rust peer for conformance tests.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::frame::{read, write, Result, TestkitError, WirePacket};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for one mock peer.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// Device identifier.
    pub device_id: String,
    /// Display name.
    pub device_name: String,
    /// Device type.
    pub device_type: String,
    /// Protocol version.
    pub protocol_version: u8,
    /// Advertised capabilities.
    pub capabilities: Vec<String>,
    /// Announced control port.
    pub tcp_port: Option<u16>,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            device_id: "0123456789abcdef0123456789abcdef".to_string(),
            device_name: "Handfast test peer".to_string(),
            device_type: "desktop".to_string(),
            protocol_version: 8,
            capabilities: vec![
                "kdeconnect.identity".to_string(),
                "kdeconnect.pair".to_string(),
                "kdeconnect.ping".to_string(),
            ],
            tcp_port: Some(1716),
        }
    }
}

impl PeerConfig {
    /// Build a test client with a fixed identifier.
    #[must_use]
    pub fn client() -> Self {
        Self::default()
    }

    /// Build a test server with a distinct identifier.
    #[must_use]
    pub fn server() -> Self {
        Self {
            device_id: "fedcba9876543210fedcba9876543210".to_string(),
            device_name: "Handfast test server".to_string(),
            ..Self::default()
        }
    }

    /// Build an identity packet.
    #[must_use]
    pub fn identity_packet(&self) -> WirePacket {
        let mut body = json!({
            "deviceId": self.device_id,
            "deviceName": self.device_name,
            "deviceType": self.device_type,
            "incomingCapabilities": self.capabilities,
            "outgoingCapabilities": self.capabilities,
            "protocolVersion": self.protocol_version,
        });
        if let Some(port) = self.tcp_port {
            body["tcpPort"] = json!(port);
        }
        WirePacket {
            id: 0,
            ptype: "kdeconnect.identity".to_string(),
            body: body.as_object().cloned().unwrap_or_default(),
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    /// Build a pairing packet.
    #[must_use]
    pub fn pair_packet(&self, pair: bool, timestamp: Option<i64>) -> WirePacket {
        let mut body = json!({ "pair": pair });
        if let Some(timestamp) = timestamp {
            body["timestamp"] = json!(timestamp);
        }
        WirePacket {
            id: 0,
            ptype: "kdeconnect.pair".to_string(),
            body: body.as_object().cloned().unwrap_or_default(),
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    /// Build a ping packet.
    #[must_use]
    pub fn ping_packet(&self, message: &str) -> WirePacket {
        WirePacket {
            id: 0,
            ptype: "kdeconnect.ping".to_string(),
            body: json!({ "message": message })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            payload_size: None,
            payload_transfer_info: None,
        }
    }
}

/// Report from the loopback peer self-test.
#[derive(Debug, Clone, Serialize)]
pub struct SelfTestReport {
    /// Whether the pair exchange completed.
    pub paired: bool,
    /// Whether the server received the ping.
    pub ping_received: bool,
    /// Packets the client sent.
    pub client_sent: usize,
    /// Packets the server sent.
    pub server_sent: usize,
}

/// Run a client and server peer over loopback.
///
/// # Errors
///
/// Fails on IO, protocol violation or timeout.
pub async fn run_self_test() -> Result<SelfTestReport> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server_config = PeerConfig::server();
    let server = tokio::spawn(async move { server_side(listener, server_config).await });
    let client_config = PeerConfig::client();
    let (client_sent, paired) = client_side(address, client_config).await?;
    let (server_sent, ping_received) = server
        .await
        .map_err(|error| TestkitError::Protocol(format!("server task failed: {error}")))??;
    Ok(SelfTestReport {
        paired,
        ping_received,
        client_sent,
        server_sent,
    })
}

async fn server_side(listener: TcpListener, config: PeerConfig) -> Result<(usize, bool)> {
    let (mut stream, _) = timeout(listener.accept()).await?;
    let mut sent = 0;

    write(&mut stream, &config.identity_packet()).await?;
    sent += 1;

    let identity = read(&mut stream)
        .await?
        .ok_or_else(|| TestkitError::Protocol("client closed before identity".to_string()))?;
    expect_type(&identity, "kdeconnect.identity")?;

    let pair = read(&mut stream)
        .await?
        .ok_or_else(|| TestkitError::Protocol("client closed before pair".to_string()))?;
    expect_type(&pair, "kdeconnect.pair")?;
    if pair.body.get("pair") != Some(&Value::Bool(true)) {
        return Err(TestkitError::Protocol(
            "client did not request pairing".to_string(),
        ));
    }

    write(&mut stream, &config.pair_packet(true, None)).await?;
    sent += 1;

    let ping = read(&mut stream)
        .await?
        .ok_or_else(|| TestkitError::Protocol("client closed before ping".to_string()))?;
    expect_type(&ping, "kdeconnect.ping")?;
    Ok((sent, true))
}

async fn client_side(address: SocketAddr, config: PeerConfig) -> Result<(usize, bool)> {
    let mut stream = timeout(TcpStream::connect(address)).await?;
    let mut sent = 0;

    let identity = read(&mut stream)
        .await?
        .ok_or_else(|| TestkitError::Protocol("server closed before identity".to_string()))?;
    expect_type(&identity, "kdeconnect.identity")?;

    write(&mut stream, &config.identity_packet()).await?;
    sent += 1;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    write(&mut stream, &config.pair_packet(true, Some(timestamp))).await?;
    sent += 1;

    let accept = read(&mut stream)
        .await?
        .ok_or_else(|| TestkitError::Protocol("server closed before pair response".to_string()))?;
    expect_type(&accept, "kdeconnect.pair")?;
    if accept.body.get("pair") != Some(&Value::Bool(true)) {
        return Err(TestkitError::Protocol(
            "server rejected pairing".to_string(),
        ));
    }

    write(&mut stream, &config.ping_packet("hello from testkit")).await?;
    sent += 1;
    Ok((sent, true))
}

fn expect_type(packet: &WirePacket, expected: &str) -> Result<()> {
    if packet.ptype != expected {
        return Err(TestkitError::Protocol(format!(
            "expected {expected}, got {}",
            packet.ptype
        )));
    }
    Ok(())
}

async fn timeout<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .map_err(|_| TestkitError::Timeout)?
        .map_err(TestkitError::Io)
}

/// Send one UDP discovery announcement.
///
/// # Errors
///
/// Fails when the socket cannot be created or the datagram cannot be sent.
pub async fn announce_once(target: SocketAddr, config: &PeerConfig) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let packet = config.identity_packet();
    let bytes = serde_json::to_vec(&packet)?;
    socket.send_to(&bytes, target).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_self_test_passes() {
        let report = run_self_test().await.unwrap();
        assert!(report.paired);
        assert!(report.ping_received);
        assert_eq!(report.client_sent, 3);
        assert_eq!(report.server_sent, 2);
    }

    #[test]
    fn identities_differ_between_roles() {
        assert_ne!(
            PeerConfig::client().device_id,
            PeerConfig::server().device_id
        );
    }
}
