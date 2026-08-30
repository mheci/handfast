//! Payload (file-transfer) data connections.
//!
//! Wire-compatible with upstream KDE Connect. A share header announces
//! `payloadTransferInfo: {"port": N}`; the *sender* listens on `N` and the
//! *receiver* dials it, then both sides upgrade to TLS (sender = TLS server,
//! receiver = TLS client) using the same self-signed device certificates and
//! fingerprint trust as the control connection. The file bytes then stream
//! raw — no chunk headers, no base64 — until exactly `payloadSize` bytes
//! (or EOF for unknown sizes) have moved.
//!
//! Port choice mirrors upstream: try [`PAYLOAD_PORT_MIN`]..=[`PAYLOAD_PORT_MAX`]
//! first, then fall back to an ephemeral port so transfers still work when
//! the preferred range is occupied.

use std::net::SocketAddr;
use std::sync::Arc;

use handfast_core::error::{Error, Result};
use handfast_protocol::tls::CertPair;
use handfast_protocol::transfer::{PAYLOAD_ACCEPT_TIMEOUT, PAYLOAD_PORT_MAX, PAYLOAD_PORT_MIN};
use tokio::net::TcpListener;
use tracing::debug;

use crate::tls::Transport;

/// Sender side of a payload transfer: a listening socket on the announced
/// port plus the certificate pair used to accept the data connection.
pub struct PayloadListener {
    listener: TcpListener,
    pair: Arc<CertPair>,
}

impl PayloadListener {
    /// Bind a data-connection listener, preferring the upstream port range.
    ///
    /// Returns the chosen port (for the `payloadTransferInfo` header) and the
    /// listener. The socket binds `0.0.0.0` so the receiver — which dials the
    /// address it sees us at on the control connection — can reach us
    /// regardless of which interface that address is on.
    pub async fn bind(pair: Arc<CertPair>) -> Result<(u16, Self)> {
        for port in PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX {
            match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(listener) => {
                    debug!(port, "payload listener bound in upstream range");
                    return Ok((port, Self { listener, pair }));
                }
                Err(_) => continue,
            }
        }
        let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
        let port = listener.local_addr()?.port();
        debug!(port, "payload listener bound on ephemeral port");
        Ok((port, Self { listener, pair }))
    }

    /// Wait for the receiver to dial the data connection.
    ///
    /// Bounded like upstream's accept window ([`PAYLOAD_ACCEPT_TIMEOUT`]) so a
    /// peer that never fetches the payload cannot pin a file handle forever.
    pub async fn accept(self) -> Result<Transport> {
        let (tcp, peer) = tokio::time::timeout(PAYLOAD_ACCEPT_TIMEOUT, self.listener.accept())
            .await
            .map_err(|_| {
                Error::Other(
                    "timed out waiting for the receiver to dial the data connection".into(),
                )
            })?
            .map_err(Error::from)?;
        debug!(%peer, "payload connection accepted");
        Transport::accept(tcp, self.pair).await
    }
}

/// Receiver side: dial the sender's announced data port and upgrade to TLS.
///
/// `control_peer` is the address the control connection came from — the
/// payload connection always targets that same host (upstream uses
/// `m_socket->peerAddress()`).
pub async fn connect_payload(
    control_peer: SocketAddr,
    port: u16,
    pair: Arc<CertPair>,
) -> Result<Transport> {
    let addr = SocketAddr::new(control_peer.ip(), port);
    debug!(%addr, "dialing payload data connection");
    Transport::connect(addr, pair).await
}
