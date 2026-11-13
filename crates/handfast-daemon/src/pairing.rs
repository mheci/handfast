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
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::session::DaemonEvent;

/// How long the terminal prompt waits for an answer.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(PAIRING_TIMEOUT_SECS as u64);

/// Pending prompts. Prompts are serialized so one stdin reader serves them all.
const PROMPT_QUEUE: usize = 8;

/// Lines read while no prompt is open are dropped.
const ANSWER_QUEUE: usize = 8;

/// One queued prompt. The daemon cancels it when the link closes.
pub struct PromptRequest {
    /// Authenticated peer.
    pub peer: Identity,
    /// Peer certificate shown in the prompt.
    pub certificate: PeerCertificate,
    /// Link the answer applies to.
    pub link: LinkHandle,
    /// Prompt generation, so a stale outcome cannot clear a newer prompt.
    pub token: u64,
    /// The daemon signals this when the link closes or is unpaired.
    pub cancel: oneshot::Receiver<()>,
}

/// Start the single prompt worker and its stdin reader.
pub fn spawn(
    store: Arc<Store>,
    links: LinkManagerHandle,
    events: mpsc::Sender<DaemonEvent>,
) -> (mpsc::Sender<PromptRequest>, tokio::task::JoinHandle<()>) {
    let (answers, _) = broadcast::channel(ANSWER_QUEUE);
    tokio::spawn(read_answers(answers.clone()));
    let (tx, rx) = mpsc::channel(PROMPT_QUEUE);
    let task = tokio::spawn(run(store, links, events, answers, rx));
    (tx, task)
}

async fn read_answers(answers: broadcast::Sender<String>) {
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let _ = answers.send(line);
            }
        }
    }
}

async fn run(
    store: Arc<Store>,
    links: LinkManagerHandle,
    events: mpsc::Sender<DaemonEvent>,
    answers: broadcast::Sender<String>,
    mut requests: mpsc::Receiver<PromptRequest>,
) {
    let mut writer = tokio::io::stdout();
    while let Some(mut request) = requests.recv().await {
        let mut answers = answers.subscribe();
        // A line that arrived before this prompt opened is not an answer to it.
        while answers.try_recv().is_ok() {}
        let decision = prompt(
            &mut writer,
            &mut answers,
            &request.peer,
            &request.certificate,
            &mut request.cancel,
            PROMPT_TIMEOUT,
        )
        .await;

        match decision {
            Decision::Accept => {
                accept(
                    Arc::clone(&store),
                    links.clone(),
                    request.peer,
                    request.certificate,
                    request.link,
                    request.token,
                    events.clone(),
                )
                .await;
            }
            Decision::Reject => {
                reject(
                    request.link,
                    request.peer.device_id,
                    request.token,
                    events.clone(),
                )
                .await;
            }
            Decision::Cancelled => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Accept,
    Reject,
    Cancelled,
}

async fn prompt<W>(
    writer: &mut W,
    answers: &mut broadcast::Receiver<String>,
    peer: &Identity,
    certificate: &PeerCertificate,
    cancel: &mut oneshot::Receiver<()>,
    timeout: Duration,
) -> Decision
where
    W: AsyncWrite + Unpin,
{
    if matches!(
        cancel.try_recv(),
        Ok(()) | Err(oneshot::error::TryRecvError::Closed)
    ) {
        return Decision::Cancelled;
    }

    let name = peer.device_name.as_str();
    let id = peer.device_id.as_str();
    let fingerprint = certificate.fingerprint().to_hex();
    let text = format!(
        "\nPairing request from {name} ({id})\nCertificate fingerprint: {fingerprint}\nAccept? [y/N] "
    );
    if writer.write_all(text.as_bytes()).await.is_err() || writer.flush().await.is_err() {
        return Decision::Reject;
    }

    tokio::select! {
        biased;
        _ = &mut *cancel => Decision::Cancelled,
        answer = tokio::time::timeout(timeout, answers.recv()) => match answer {
            Ok(Ok(line)) if line.trim().eq_ignore_ascii_case("y") => Decision::Accept,
            _ => Decision::Reject,
        },
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
    tracing::info!(
        %device_id,
        fingerprint = %pin.to_hex(),
        "pairing pin written"
    );

    let packet = match PairingRequest::accept().to_packet() {
        Ok(packet) => packet,
        Err(error) => {
            tracing::warn!(%error, %device_id, "failed to build the pairing acceptance");
            rollback(&store, &links, &device_id).await;
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

/// Undo both trust writes when acceptance cannot be completed.
async fn rollback(store: &Arc<Store>, links: &LinkManagerHandle, device_id: &DeviceId) {
    let id = device_id.as_str().to_string();
    if let Err(error) = Arc::clone(store)
        .run_blocking(move |store| store.set_trusted(&id, None))
        .await
    {
        tracing::warn!(%error, %device_id, "failed to roll back the stored pin");
    }
    if let Err(error) = links.set_trust(device_id.clone(), None).await {
        tracing::warn!(%error, %device_id, "failed to roll back the manager pin");
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

    async fn answer(input: &str) -> Decision {
        let (lines, _) = broadcast::channel(ANSWER_QUEUE);
        let mut answers = lines.subscribe();
        lines
            .send(input.to_string())
            .expect("the answer channel is open");
        let (_cancel, mut cancel) = oneshot::channel();
        let (identity, certificate) = peer();
        prompt(
            &mut tokio::io::sink(),
            &mut answers,
            &identity,
            &certificate,
            &mut cancel,
            PROMPT_TIMEOUT,
        )
        .await
    }

    #[tokio::test]
    async fn y_accepts() {
        assert_eq!(answer("y\n").await, Decision::Accept);
    }

    #[tokio::test]
    async fn n_and_empty_input_reject() {
        assert_eq!(answer("n\n").await, Decision::Reject);
        assert_eq!(answer("\n").await, Decision::Reject);
    }

    #[tokio::test(start_paused = true)]
    async fn silence_times_out_and_rejects() {
        let (lines, _) = broadcast::channel(ANSWER_QUEUE);
        let mut answers = lines.subscribe();
        let (_cancel, mut cancel) = oneshot::channel();
        let (identity, certificate) = peer();
        let decision = prompt(
            &mut tokio::io::sink(),
            &mut answers,
            &identity,
            &certificate,
            &mut cancel,
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(decision, Decision::Reject);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_prompt_does_not_consume_a_later_answer() {
        let (lines, _) = broadcast::channel(ANSWER_QUEUE);
        let (identity, certificate) = peer();

        let (_cancel, mut cancel) = oneshot::channel();
        let mut first = lines.subscribe();
        let first_decision = prompt(
            &mut tokio::io::sink(),
            &mut first,
            &identity,
            &certificate,
            &mut cancel,
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(first_decision, Decision::Reject);

        lines
            .send("y\n".to_string())
            .expect("the answer channel is open");
        let (_cancel, mut cancel) = oneshot::channel();
        let mut second = lines.subscribe();
        let second_decision = prompt(
            &mut tokio::io::sink(),
            &mut second,
            &identity,
            &certificate,
            &mut cancel,
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(
            second_decision,
            Decision::Reject,
            "the stale answer must not satisfy the next prompt"
        );
    }

    #[tokio::test]
    async fn a_cancelled_prompt_does_not_wait_for_an_answer() {
        let (lines, _) = broadcast::channel(ANSWER_QUEUE);
        let mut answers = lines.subscribe();
        let (cancel, mut cancel_rx) = oneshot::channel();
        cancel.send(()).expect("the cancel receiver is open");
        let (identity, certificate) = peer();
        let decision = prompt(
            &mut tokio::io::sink(),
            &mut answers,
            &identity,
            &certificate,
            &mut cancel_rx,
            PROMPT_TIMEOUT,
        )
        .await;
        assert_eq!(decision, Decision::Cancelled);
    }

    #[test]
    fn prompt_window_fits_inside_the_unpaired_idle_timeout() {
        assert!(
            PROMPT_TIMEOUT < LinkLimits::default().unpaired_idle_timeout,
            "a prompt must finish before the manager can close an idle unpaired link"
        );
    }
}
