//! Post-TLS identity exchange.
//!
//! Immediately after the TLS handshake both sides send a
//! `kdeconnect.identity` packet carrying their [`Identity`], then read the
//! peer's. Both helpers follow the same write-then-read order so neither
//! side can deadlock waiting for the other.

use std::time::Duration;

use handfast_core::error::{Error, Result};
use handfast_protocol::{Identity, Packet, TYPE_IDENTITY};
use tokio::time::timeout;
use tracing::debug;

/// Generous bound on the post-handshake exchange; a peer that cannot
/// introduce itself within this window is not worth keeping.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

async fn exchange(
    mut transport: crate::tls::Transport,
    mine: &Identity,
) -> Result<(String, crate::tls::Transport)> {
    let hello = Packet::identity(mine.clone());
    transport.write_packet(&hello).await?;

    let packet = timeout(HANDSHAKE_TIMEOUT, transport.read_packet())
        .await
        .map_err(|_| Error::Other("identity exchange timed out".into()))??;

    if packet.ptype != TYPE_IDENTITY {
        return Err(Error::Other(format!(
            "expected {} as first packet, got {}",
            TYPE_IDENTITY, packet.ptype
        )));
    }
    let theirs: Identity = serde_json::from_value(packet.body.clone())
        .map_err(|err| Error::Other(format!("malformed identity body: {err}")))?;
    if theirs.device_id.is_empty() {
        return Err(Error::Other("peer identity lacks deviceId".into()));
    }
    debug!(device = %theirs.device_id, "identity exchanged");
    Ok((theirs.device_id, transport))
}

/// Client-side exchange: dialer sends first, then reads.
pub async fn complete_outbound(
    transport: crate::tls::Transport,
    mine: &Identity,
) -> Result<(String, crate::tls::Transport)> {
    exchange(transport, mine).await
}

/// Server-side exchange: acceptor follows the identical sequence.
pub async fn complete_inbound(
    transport: crate::tls::Transport,
    mine: &Identity,
) -> Result<(String, crate::tls::Transport)> {
    exchange(transport, mine).await
}
