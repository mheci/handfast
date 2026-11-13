//! The terminal pairing prompt and the only writer of peer trust.
//!
//! The prompt window is the protocol's 30-second request window. The manager's
//! 60-second unpaired idle timeout starts from the last inbound packet, and the
//! pairing request itself resets it, so a decision always lands before the link
//! can be closed for idleness.

use std::sync::Arc;
use std::time::Duration;

use handfast_net::{LinkHandle, LinkManagerHandle, PeerCertificate, PAIRING_TIMEOUT_SECS};
use handfast_proto::{DeviceId, Identity, PairingRequest};
use handfast_store::Store;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::session::DaemonEvent;

/// How long the terminal prompt waits for an answer.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(PAIRING_TIMEOUT_SECS as u64);

/// Ask the user, then accept or reject the pending link.
pub async fn run(
    store: Arc<Store>,
    links: LinkManagerHandle,
    peer: Identity,
    certificate: PeerCertificate,
    link: LinkHandle,
    token: u64,
    events: mpsc::Sender<DaemonEvent>,
) {
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();
    let decision = prompt(
        &mut writer,
        &mut reader,
        &peer,
        &certificate,
        PROMPT_TIMEOUT,
    )
    .await;

    match decision {
        Decision::Accept => {
            accept(store, links, peer, certificate, link, token, events).await;
        }
        Decision::Reject => reject(link, peer.device_id, token, events).await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Accept,
    Reject,
}

async fn prompt<W, R>(
    writer: &mut W,
    reader: &mut R,
    peer: &Identity,
    certificate: &PeerCertificate,
    timeout: Duration,
) -> Decision
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let name = peer.device_name.as_str();
    let id = peer.device_id.as_str();
    let fingerprint = certificate.fingerprint().to_hex();
    let text = format!(
        "\nPairing request from {name} ({id})\nCertificate fingerprint: {fingerprint}\nAccept? [y/N] "
    );
    if writer.write_all(text.as_bytes()).await.is_err() || writer.flush().await.is_err() {
        return Decision::Reject;
    }

    let mut line = String::new();
    match tokio::time::timeout(timeout, reader.read_line(&mut line)).await {
        Ok(Ok(read)) if read > 0 => {
            if line.trim().eq_ignore_ascii_case("y") {
                Decision::Accept
            } else {
                Decision::Reject
            }
        }
        _ => Decision::Reject,
    }
}

async fn accept(
    store: Arc<Store>,
    links: LinkManagerHandle,
    peer: Identity,
    certificate: PeerCertificate,
    link: LinkHandle,
    token: u64,
    events: mpsc::Sender<DaemonEvent>,
) {
    let device_id = peer.device_id.clone();
    let pin = certificate.fingerprint();

    let id = device_id.as_str().to_string();
    let stored = Arc::clone(&store)
        .run_blocking(move |store| store.set_trusted(&id, Some(*pin.as_bytes())))
        .await;
    match stored {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(%device_id, "cannot pin an unknown device");
            reject(link, device_id, token, events).await;
            return;
        }
        Err(error) => {
            tracing::warn!(%error, %device_id, "failed to persist the peer pin");
            reject(link, device_id, token, events).await;
            return;
        }
    }

    if let Err(error) = links.set_trust(device_id.clone(), Some(pin)).await {
        tracing::warn!(%error, %device_id, "the manager rejected the pin; rolling back");
        let id = device_id.as_str().to_string();
        let _ = Arc::clone(&store)
            .run_blocking(move |store| store.set_trusted(&id, None))
            .await;
        reject(link, device_id, token, events).await;
        return;
    }

    let packet = match PairingRequest::accept().to_packet() {
        Ok(packet) => packet,
        Err(error) => {
            tracing::warn!(%error, %device_id, "failed to build the pairing acceptance");
            reject(link, device_id, token, events).await;
            return;
        }
    };
    if let Err(error) = link.send(packet).await {
        // The user accepted this certificate, so the pin stays. The session is
        // still created; the manager stops it if the link is already gone.
        tracing::warn!(%error, %device_id, "failed to send the pairing acceptance");
    }

    if events
        .send(DaemonEvent::PairingAccepted { device_id, token })
        .await
        .is_err()
    {
        tracing::warn!("daemon stopped before the session was created");
    }
}

async fn reject(
    link: LinkHandle,
    device_id: DeviceId,
    token: u64,
    events: mpsc::Sender<DaemonEvent>,
) {
    if let Ok(packet) = PairingRequest::reject().to_packet() {
        if let Err(error) = link.send(packet).await {
            tracing::debug!(%error, %device_id, "failed to send the pairing rejection");
        }
    }
    let _ = events
        .send(DaemonEvent::PairingRejected { device_id, token })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_net::{LinkLimits, TlsIdentity};
    use handfast_proto::{Capabilities, DeviceName, DeviceType};

    fn peer() -> (Identity, PeerCertificate) {
        let device_id = DeviceId::new("00112233445566778899aabbccddeeff").expect("valid test ID");
        let tls = TlsIdentity::generate(device_id.clone()).expect("generate the test certificate");
        let certificate =
            PeerCertificate::parse(tls.cert_der()).expect("parse the test certificate");
        let identity = Identity::new(
            device_id,
            DeviceName::new("Test peer"),
            DeviceType::Phone,
            Capabilities::default(),
            Some(1716),
        );
        (identity, certificate)
    }

    async fn answer(input: &[u8]) -> Decision {
        let (mut client, server) = tokio::io::duplex(1024);
        let mut reader = tokio::io::BufReader::new(server);
        let mut writer = tokio::io::sink();
        let (identity, certificate) = peer();
        client.write_all(input).await.expect("write the answer");
        drop(client);
        prompt(
            &mut writer,
            &mut reader,
            &identity,
            &certificate,
            PROMPT_TIMEOUT,
        )
        .await
    }

    #[tokio::test]
    async fn y_accepts() {
        assert_eq!(answer(b"y\n").await, Decision::Accept);
    }

    #[tokio::test]
    async fn n_and_empty_input_reject() {
        assert_eq!(answer(b"n\n").await, Decision::Reject);
        assert_eq!(answer(b"\n").await, Decision::Reject);
    }

    #[tokio::test]
    async fn eof_rejects() {
        assert_eq!(answer(b"").await, Decision::Reject);
    }

    #[tokio::test(start_paused = true)]
    async fn silence_times_out_and_rejects() {
        let (_client, server) = tokio::io::duplex(64);
        let mut reader = tokio::io::BufReader::new(server);
        let mut writer = tokio::io::sink();
        let (identity, certificate) = peer();
        let decision = prompt(
            &mut writer,
            &mut reader,
            &identity,
            &certificate,
            PROMPT_TIMEOUT,
        )
        .await;
        assert_eq!(decision, Decision::Reject);
    }

    #[test]
    fn prompt_window_fits_inside_the_unpaired_idle_timeout() {
        assert!(
            PROMPT_TIMEOUT < LinkLimits::default().unpaired_idle_timeout,
            "a prompt must finish before the manager can close an idle unpaired link"
        );
    }
}
