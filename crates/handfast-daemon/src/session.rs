//! One plugin host per paired connected device.
//!
//! The session owns the [`DeviceHost`], dispatches inbound packets, forwards
//! plugin output to the link and plugin events to the daemon, runs the inbound
//! transfer runner, and runs `on_disconnect` before it exits. The daemon event
//! loop owns the session map; the manager never sees a `DeviceHost`.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_desktop::{Desktop, Files, IncomingRequest};
use handfast_net::{
    LinkHandle, PayloadError, PayloadPeer, PayloadService, PeerCertificate, PendingPayload,
};
use handfast_proto::{DeviceId, Identity, Packet, PayloadSize};
use handfast_runtime::{Device, DeviceHost, Dispatch, PluginEvent, Registry, Transfer};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle, JoinSet};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Inbound queue for one session. A full queue drops packets rather than
/// blocking the daemon event loop.
const SESSION_QUEUE: usize = 64;

/// How long a share packet waits for room in the session queue.
const SHARE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// How many eager receives one peer may queue behind the active transfer.
const MAX_QUEUED_RECEIVES: usize = 8;

/// Progress updates buffered between the payload service and the event sender.
const PROGRESS_QUEUE: usize = 8;

/// Minimum interval between progress events for one transfer.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Byte step that also releases a progress event.
const PROGRESS_BYTES: u64 = 4 * 1024 * 1024;

/// An event from a session or a pairing task back to the daemon.
#[derive(Debug)]
pub enum DaemonEvent {
    /// Plugin events from one device.
    PluginEvents {
        /// Device that produced the events.
        device_id: DeviceId,
        /// Typed events.
        events: Vec<PluginEvent>,
    },
    /// The user accepted a pairing request and the link is pinned.
    PairingAccepted {
        /// Device whose link is ready for a session.
        device_id: DeviceId,
        /// Prompt generation, so a stale outcome cannot clear a newer prompt.
        token: u64,
    },
    /// The user rejected a pairing request or the prompt timed out.
    PairingRejected {
        /// Device whose request was rejected.
        device_id: DeviceId,
        /// Prompt generation, so a stale outcome cannot clear a newer prompt.
        token: u64,
    },
}

/// Payload inputs for one session with an inbound transfer runner.
pub struct TransferSetup {
    /// Authenticated control link address. The payload dials this IP.
    pub remote_addr: SocketAddr,
    /// Certificate the control link authenticated.
    pub certificate: PeerCertificate,
    /// Shared payload byte mover.
    pub payloads: Arc<PayloadService>,
}

/// A running session and its handle.
pub struct DeviceSession {
    task: JoinHandle<()>,
    handle: SessionHandle,
}

impl DeviceSession {
    /// Start a session for an authenticated, paired link without transfers.
    pub fn start(
        peer: Identity,
        link: LinkHandle,
        registry: Arc<Registry>,
        desktop: Desktop,
        events: mpsc::Sender<DaemonEvent>,
    ) -> Self {
        Self::start_inner(peer, link, registry, desktop, events, None)
    }

    /// Start a session with the inbound share transfer runner.
    ///
    /// The runner exists only when the desktop has a Files adapter. Without one
    /// the share plugin is disabled, so no transfer intent can arrive.
    pub fn start_with_transfers(
        peer: Identity,
        link: LinkHandle,
        registry: Arc<Registry>,
        desktop: Desktop,
        events: mpsc::Sender<DaemonEvent>,
        setup: TransferSetup,
    ) -> Self {
        let deps = desktop.files.clone().map(|files| TransferDeps {
            peer: PayloadPeer {
                device_id: peer.device_id.clone(),
                address: setup.remote_addr,
                certificate: setup.certificate,
            },
            files,
            payloads: setup.payloads,
            #[cfg(test)]
            connects: Arc::new(AtomicUsize::new(0)),
        });
        Self::start_inner(peer, link, registry, desktop, events, deps)
    }

    /// Start a session and return the connect-completion probe. Test only.
    #[cfg(test)]
    pub fn start_with_probe(
        peer: Identity,
        link: LinkHandle,
        registry: Arc<Registry>,
        desktop: Desktop,
        events: mpsc::Sender<DaemonEvent>,
        setup: TransferSetup,
    ) -> (Self, Arc<AtomicUsize>) {
        let connects = Arc::new(AtomicUsize::new(0));
        let deps = desktop.files.clone().map(|files| TransferDeps {
            peer: PayloadPeer {
                device_id: peer.device_id.clone(),
                address: setup.remote_addr,
                certificate: setup.certificate,
            },
            files,
            payloads: setup.payloads,
            connects: Arc::clone(&connects),
        });
        (
            Self::start_inner(peer, link, registry, desktop, events, deps),
            connects,
        )
    }

    fn start_inner(
        peer: Identity,
        link: LinkHandle,
        registry: Arc<Registry>,
        desktop: Desktop,
        events: mpsc::Sender<DaemonEvent>,
        deps: Option<TransferDeps>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(SESSION_QUEUE);
        let handle = SessionHandle {
            tx,
            delivery_timeout: SHARE_DELIVERY_TIMEOUT,
        };
        let task = tokio::spawn(run(peer, link, registry, desktop, events, rx, deps));
        Self { task, handle }
    }

    /// Deliver one packet to the session.
    pub fn deliver(&self, packet: Packet) {
        self.handle.deliver(packet);
    }

    /// The session's handle.
    #[must_use]
    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }

    /// Consume the session and return its task, for shutdown.
    #[must_use]
    pub fn into_task(self) -> JoinHandle<()> {
        self.task
    }
}

/// A bounded handle for one session.
#[derive(Clone)]
pub struct SessionHandle {
    tx: mpsc::Sender<SessionInput>,
    delivery_timeout: Duration,
}

impl SessionHandle {
    /// Deliver one packet without waiting. A full queue drops the packet.
    pub fn deliver(&self, packet: Packet) {
        match self.tx.try_send(SessionInput::Packet(packet)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("dropping a packet for a full session queue");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("dropping a packet for a closed session");
            }
        }
    }

    /// Queue one share packet on the reliable path.
    ///
    /// Waits up to the session delivery timeout for room. A timeout or a closed
    /// queue returns an error so the caller can close the link and emit
    /// `ShareFailed`.
    ///
    /// # Errors
    ///
    /// Fails with [`SessionError::Full`] when the queue stays full for the
    /// delivery timeout, and [`SessionError::Closed`] when the session stopped.
    pub async fn deliver_share(&self, packet: Packet) -> Result<(), SessionError> {
        match tokio::time::timeout(
            self.delivery_timeout,
            self.tx.send(SessionInput::Share(packet)),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(SessionError::Closed),
            Err(_) => Err(SessionError::Full),
        }
    }

    /// Stop the session. `on_disconnect` runs before the task exits.
    pub fn stop(&self, reason: String) {
        match self.tx.try_send(SessionInput::Disconnected(reason)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(input)) => {
                // The queue is backed up; deliver the stop after the backlog
                // rather than dropping it.
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(input).await;
                });
            }
        }
    }
}

/// Why a share packet could not enter the session queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// The session queue stayed full for the delivery timeout.
    Full,
    /// The session task has stopped.
    Closed,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => formatter.write_str("session share queue full"),
            Self::Closed => formatter.write_str("session closed"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Input to a session task.
pub enum SessionInput {
    /// One inbound packet on the best-effort path.
    Packet(Packet),
    /// One share packet on the reliable path.
    Share(Packet),
    /// The link closed or the daemon is shutting down.
    Disconnected(String),
}

/// Payload and file adapters for the inbound transfer runner.
struct TransferDeps {
    peer: PayloadPeer,
    files: Arc<dyn Files>,
    payloads: Arc<PayloadService>,
    #[cfg(test)]
    connects: Arc<AtomicUsize>,
}

/// One queued receive. Only the front slot may start.
enum Slot {
    /// An empty file, or a file whose eager connect is not spawned yet.
    Queued { id: u64, intent: Transfer },
    /// A raw TCP connection is being dialed.
    Connecting { id: u64, intent: Transfer },
    /// The raw TCP connection is held until the front slot starts.
    Ready {
        id: u64,
        intent: Transfer,
        pending: Box<PendingPayload>,
    },
}

impl Slot {
    fn id(&self) -> u64 {
        match self {
            Self::Queued { id, .. } | Self::Connecting { id, .. } | Self::Ready { id, .. } => *id,
        }
    }
}

/// The one active receive for a peer.
struct ActiveReceive {
    id: u64,
}

/// Inbound share transfer state for one peer.
///
/// Slots are ordered by transfer id. Every non-empty slot dials raw TCP at
/// enqueue; the TLS handshake and the temp file wait for the front slot.
struct TransferRunner {
    peer: PayloadPeer,
    files: Arc<dyn Files>,
    payloads: Arc<PayloadService>,
    events: mpsc::Sender<DaemonEvent>,
    active: Option<ActiveReceive>,
    slots: VecDeque<Slot>,
    receives: JoinSet<()>,
    connects: JoinSet<(u64, Result<PendingPayload, PayloadError>)>,
    next_id: u64,
    #[cfg(test)]
    connects_completed: Arc<AtomicUsize>,
}

impl TransferRunner {
    fn new(deps: TransferDeps, events: mpsc::Sender<DaemonEvent>) -> Self {
        #[cfg(test)]
        let connects_completed = Arc::clone(&deps.connects);
        let TransferDeps {
            peer,
            files,
            payloads,
            ..
        } = deps;
        Self {
            peer,
            files,
            payloads,
            events,
            active: None,
            slots: VecDeque::new(),
            receives: JoinSet::new(),
            connects: JoinSet::new(),
            next_id: 1,
            #[cfg(test)]
            connects_completed,
        }
    }

    async fn enqueue_all(&mut self, transfers: Vec<Transfer>) {
        for transfer in transfers {
            self.enqueue(transfer).await;
        }
    }

    async fn enqueue(&mut self, transfer: Transfer) {
        if self.slots.len() >= MAX_QUEUED_RECEIVES {
            tracing::warn!(
                device_id = %self.peer.device_id,
                max = MAX_QUEUED_RECEIVES,
                "receive queue full"
            );
            emit(
                &self.events,
                &self.peer.device_id,
                PluginEvent::ShareFailed {
                    transfer_id: 0,
                    reason: format!("receive queue full (max {MAX_QUEUED_RECEIVES})"),
                },
            )
            .await;
            return;
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let Transfer::ReceiveFile {
            filename,
            size,
            offer,
        } = transfer;
        let intent = Transfer::ReceiveFile {
            filename,
            size,
            offer,
        };
        match offer {
            Some(offer) => {
                self.slots.push_back(Slot::Connecting { id, intent });
                let peer = self.peer.clone();
                let payloads = Arc::clone(&self.payloads);
                self.connects.spawn(async move {
                    let result = payloads.connect(peer, offer).await;
                    (id, result)
                });
            }
            None => self.slots.push_back(Slot::Queued { id, intent }),
        }
    }

    /// Start the front slot when no receive is active.
    fn start_front(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(front) = self.slots.front() else {
            return;
        };
        if matches!(front, Slot::Connecting { .. }) {
            return;
        }
        let Some(slot) = self.slots.pop_front() else {
            return;
        };
        match slot {
            Slot::Queued { id, intent } => self.spawn_receive(id, intent, None),
            Slot::Ready {
                id,
                intent,
                pending,
            } => self.spawn_receive(id, intent, Some(*pending)),
            Slot::Connecting { .. } => {}
        }
    }

    fn spawn_receive(&mut self, id: u64, intent: Transfer, pending: Option<PendingPayload>) {
        self.active = Some(ActiveReceive { id });
        let files = Arc::clone(&self.files);
        let payloads = Arc::clone(&self.payloads);
        let events = self.events.clone();
        let device_id = self.peer.device_id.clone();
        self.receives.spawn(async move {
            receive_transfer(id, intent, pending, files, payloads, events, device_id).await;
        });
    }

    fn receive_finished(&mut self, joined: Option<Result<(), JoinError>>) {
        match joined {
            Some(Err(error)) => {
                tracing::warn!(
                    device_id = %self.peer.device_id,
                    %error,
                    "receive task ended abnormally"
                );
            }
            Some(Ok(())) | None => {}
        }
        self.active = None;
    }

    async fn connect_finished(
        &mut self,
        joined: Option<Result<(u64, Result<PendingPayload, PayloadError>), JoinError>>,
    ) {
        let Some(Ok((id, result))) = joined else {
            return;
        };
        let Some(index) = self.slots.iter().position(|slot| slot.id() == id) else {
            return;
        };
        match result {
            Ok(pending) => {
                if let Some(Slot::Connecting { id, intent }) = self.slots.remove(index) {
                    self.slots.insert(
                        index,
                        Slot::Ready {
                            id,
                            intent,
                            pending: Box::new(pending),
                        },
                    );
                    #[cfg(test)]
                    self.connects_completed.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(error) => {
                let _ = self.slots.remove(index);
                emit(
                    &self.events,
                    &self.peer.device_id,
                    PluginEvent::ShareFailed {
                        transfer_id: id,
                        reason: error.to_string(),
                    },
                )
                .await;
            }
        }
    }

    /// Stop every transfer, close held sockets and remove temp files.
    async fn shutdown(mut self, reason: &str) {
        tracing::debug!(
            device_id = %self.peer.device_id,
            %reason,
            "stopping the transfer runner"
        );
        self.receives.abort_all();
        while self.receives.join_next().await.is_some() {}
        self.connects.abort_all();
        while self.connects.join_next().await.is_some() {}
        self.slots.clear();
        self.active = None;
    }
}

/// One step of the session loop.
enum SessionStep {
    Input(Option<SessionInput>),
    Receive(Option<Result<(), JoinError>>),
    Connect(Option<Result<(u64, Result<PendingPayload, PayloadError>), JoinError>>),
}

async fn run(
    peer: Identity,
    link: LinkHandle,
    registry: Arc<Registry>,
    desktop: Desktop,
    events: mpsc::Sender<DaemonEvent>,
    mut input: mpsc::Receiver<SessionInput>,
    deps: Option<TransferDeps>,
) {
    let device_id = peer.device_id.clone();
    let device = Device {
        id: peer.device_id,
        name: peer.device_name,
        device_type: peer.device_type,
        paired: true,
        connected: true,
    };
    let mut host = DeviceHost::new(device, registry.as_ref(), desktop);
    let mut runner = deps.map(|deps| TransferRunner::new(deps, events.clone()));

    let dispatch = host.on_connect().await;
    let transfers = send_dispatch(&link, &events, &device_id, dispatch).await;
    if let Some(runner) = runner.as_mut() {
        runner.enqueue_all(transfers).await;
    }

    let reason = loop {
        if let Some(runner) = runner.as_mut() {
            runner.start_front();
        }

        let (receives_open, connects_open) = match runner.as_ref() {
            Some(runner) => (!runner.receives.is_empty(), !runner.connects.is_empty()),
            None => (false, false),
        };
        let step = match runner.as_mut() {
            Some(runner) => {
                tokio::select! {
                    input = input.recv() => SessionStep::Input(input),
                    joined = runner.receives.join_next(), if receives_open => {
                        SessionStep::Receive(joined)
                    }
                    joined = runner.connects.join_next(), if connects_open => {
                        SessionStep::Connect(joined)
                    }
                }
            }
            None => SessionStep::Input(input.recv().await),
        };

        match step {
            SessionStep::Input(None) => break "session channel closed".to_string(),
            SessionStep::Input(Some(input)) => {
                if let Some(reason) =
                    handle_input(input, &mut host, &mut runner, &link, &events, &device_id).await
                {
                    break reason;
                }
            }
            SessionStep::Receive(joined) => {
                if let Some(runner) = runner.as_mut() {
                    runner.receive_finished(joined);
                }
            }
            SessionStep::Connect(joined) => {
                if let Some(runner) = runner.as_mut() {
                    runner.connect_finished(joined).await;
                }
            }
        }
    };

    tracing::debug!(%device_id, %reason, "device session closing");
    if let Some(runner) = runner.take() {
        runner.shutdown(&reason).await;
    }
    let dispatch = host.on_disconnect().await;
    let _ = send_dispatch(&link, &events, &device_id, dispatch).await;
}

async fn handle_input(
    input: SessionInput,
    host: &mut DeviceHost,
    runner: &mut Option<TransferRunner>,
    link: &LinkHandle,
    events: &mpsc::Sender<DaemonEvent>,
    device_id: &DeviceId,
) -> Option<String> {
    match input {
        SessionInput::Packet(packet) | SessionInput::Share(packet) => {
            let dispatch = host.dispatch(&packet).await;
            let transfers = send_dispatch(link, events, device_id, dispatch).await;
            if let Some(runner) = runner.as_mut() {
                runner.enqueue_all(transfers).await;
            }
            None
        }
        SessionInput::Disconnected(reason) => Some(reason),
    }
}

async fn send_dispatch(
    link: &LinkHandle,
    events: &mpsc::Sender<DaemonEvent>,
    device_id: &DeviceId,
    dispatch: Dispatch,
) -> Vec<Transfer> {
    for packet in dispatch.packets {
        if let Err(error) = link.send(packet).await {
            tracing::warn!(%error, %device_id, "failed to send a plugin packet");
        }
    }
    for error in dispatch.errors {
        tracing::warn!(%error, %device_id, "plugin call failed");
    }
    if !dispatch.events.is_empty() {
        let event = DaemonEvent::PluginEvents {
            device_id: device_id.clone(),
            events: dispatch.events,
        };
        if let Err(error) = events.send(event).await {
            tracing::warn!(%error, %device_id, "failed to forward plugin events");
        }
    }
    dispatch.transfers
}

async fn emit(events: &mpsc::Sender<DaemonEvent>, device_id: &DeviceId, event: PluginEvent) {
    let message = DaemonEvent::PluginEvents {
        device_id: device_id.clone(),
        events: vec![event],
    };
    if events.send(message).await.is_err() {
        tracing::warn!(%device_id, "daemon event channel closed; transfer event dropped");
    }
}

fn emit_progress(events: &mpsc::Sender<DaemonEvent>, device_id: &DeviceId, event: PluginEvent) {
    let message = DaemonEvent::PluginEvents {
        device_id: device_id.clone(),
        events: vec![event],
    };
    if events.try_send(message).is_err() {
        tracing::debug!(%device_id, "dropping a transfer progress event for a full queue");
    }
}

async fn receive_transfer(
    id: u64,
    intent: Transfer,
    pending: Option<PendingPayload>,
    files: Arc<dyn Files>,
    payloads: Arc<PayloadService>,
    events: mpsc::Sender<DaemonEvent>,
    device_id: DeviceId,
) {
    let Transfer::ReceiveFile {
        filename,
        size,
        offer,
    } = intent;
    let declared = match size {
        PayloadSize::Known(bytes) => Some(bytes),
        PayloadSize::Unknown => None,
    };
    let incoming = match files
        .open_incoming(IncomingRequest {
            filename,
            size: declared,
            open: false,
        })
        .await
    {
        Ok(incoming) => incoming,
        Err(error) => {
            emit(
                &events,
                &device_id,
                PluginEvent::ShareFailed {
                    transfer_id: id,
                    reason: error.to_string(),
                },
            )
            .await;
            return;
        }
    };

    let Some(pending) = pending else {
        match incoming.commit().await {
            Ok(path) => {
                emit(
                    &events,
                    &device_id,
                    PluginEvent::ShareCompleted {
                        transfer_id: id,
                        path,
                        bytes: 0,
                    },
                )
                .await;
            }
            Err(error) => {
                emit(
                    &events,
                    &device_id,
                    PluginEvent::ShareFailed {
                        transfer_id: id,
                        reason: error.to_string(),
                    },
                )
                .await;
            }
        }
        return;
    };

    let total = offer.and_then(|offer| match offer.size {
        PayloadSize::Known(bytes) => Some(bytes),
        PayloadSize::Unknown => None,
    });

    let (progress_tx, mut progress_rx) = mpsc::channel(PROGRESS_QUEUE);
    let result = {
        let writer = incoming.writer();
        let receive = payloads.receive_pending(pending, writer, Some(progress_tx));
        tokio::pin!(receive);
        let mut last_emit = Instant::now();
        let mut last_bytes = 0u64;
        let mut progress_open = true;
        loop {
            tokio::select! {
                result = &mut receive => break result,
                update = progress_rx.recv(), if progress_open => match update {
                    Some(bytes) => {
                        if last_emit.elapsed() >= PROGRESS_INTERVAL
                            || bytes.saturating_sub(last_bytes) >= PROGRESS_BYTES
                        {
                            emit_progress(
                                &events,
                                &device_id,
                                PluginEvent::ShareProgress {
                                    transfer_id: id,
                                    bytes,
                                    total,
                                },
                            );
                            last_emit = Instant::now();
                            last_bytes = bytes;
                        }
                    }
                    None => progress_open = false,
                },
            }
        }
    };

    match result {
        Ok(outcome) => match incoming.commit().await {
            Ok(path) => {
                emit(
                    &events,
                    &device_id,
                    PluginEvent::ShareCompleted {
                        transfer_id: id,
                        path,
                        bytes: outcome.bytes,
                    },
                )
                .await;
            }
            Err(error) => {
                emit(
                    &events,
                    &device_id,
                    PluginEvent::ShareFailed {
                        transfer_id: id,
                        reason: error.to_string(),
                    },
                )
                .await;
            }
        },
        Err(error) => {
            incoming.abort().await;
            emit(
                &events,
                &device_id,
                PluginEvent::ShareFailed {
                    transfer_id: id,
                    reason: error.to_string(),
                },
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use handfast_desktop::{Desktop, MockDesktop};
    use handfast_net::{PayloadLimits, TlsIdentity};
    use handfast_plugins::registry;
    use handfast_proto::{file_request, types, Capabilities, DeviceName, DeviceType, ShareFile};
    use handfast_runtime::{
        DesktopRequirement, LogLevel, Plugin, PluginContext, PluginError, PluginFactory,
        PluginMeta, PluginOutput, PluginStatus, Registry,
    };
    use handfast_testkit::payload_peer::{PayloadSender, PayloadSenderConfig};
    use handfast_testkit::peer::PeerConfig;
    use handfast_testkit::tls_peer::TlsVersion;
    use tokio::sync::mpsc;

    static RECORDING: PluginMeta = PluginMeta {
        name: "recording",
        title: "Recording",
        status: PluginStatus::Implemented,
        incoming: &[],
        outgoing: &[],
        desktop: DesktopRequirement::None,
    };

    static PORTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn ports() -> tokio::sync::MutexGuard<'static, ()> {
        PORTS.lock().await
    }

    fn peer_id() -> DeviceId {
        DeviceId::new("0123456789abcdef0123456789abcdef").expect("a valid test ID")
    }

    fn peer_identity() -> Identity {
        Identity::new(
            peer_id(),
            DeviceName::new("Test peer"),
            DeviceType::Phone,
            Capabilities::default(),
            Some(1716),
        )
    }

    fn payloads() -> Arc<PayloadService> {
        let tls = Arc::new(TlsIdentity::generate(DeviceId::generate()).expect("generate TLS"));
        Arc::new(PayloadService::new(
            tls,
            PayloadLimits {
                connect_timeout: Duration::from_secs(2),
                handshake_timeout: Duration::from_secs(2),
                accept_timeout: Duration::from_secs(2),
                io_timeout: Duration::from_secs(2),
                ..PayloadLimits::default()
            },
        ))
    }

    fn payload_sender(mut config: PayloadSenderConfig) -> PayloadSender {
        for port in handfast_proto::PAYLOAD_PORT_MIN..=handfast_proto::PAYLOAD_PORT_MAX {
            config.bind_port = Some(port);
            if let Ok(sender) = PayloadSender::new(config.clone()) {
                return sender;
            }
        }
        panic!("no free payload port in 1739-1764");
    }

    fn sender_config() -> PayloadSenderConfig {
        PayloadSenderConfig::new(PeerConfig::client(), TlsVersion::Both)
    }

    fn file_packet(filename: &str, size: u64, port: u16) -> Packet {
        let file = ShareFile {
            filename: Some(filename.to_string()),
            size: PayloadSize::Known(size),
            created_ms: None,
            modified_ms: None,
            open: false,
            batch: None,
        };
        file_request(&file, Some(port)).expect("build the file request")
    }

    fn empty_file_packet(filename: &str) -> Packet {
        let file = ShareFile {
            filename: Some(filename.to_string()),
            size: PayloadSize::Known(0),
            created_ms: None,
            modified_ms: None,
            open: false,
            batch: None,
        };
        file_request(&file, None).expect("build the empty file request")
    }

    fn start_session(
        desktop: &MockDesktop,
        payloads: Arc<PayloadService>,
        certificate: &[u8],
    ) -> (
        DeviceSession,
        mpsc::Receiver<DaemonEvent>,
        mpsc::Receiver<Packet>,
    ) {
        let (events, events_rx) = mpsc::channel(64);
        let (link_tx, link_rx) = mpsc::channel(8);
        let session = DeviceSession::start_with_transfers(
            peer_identity(),
            LinkHandle::from_channel(peer_id(), link_tx),
            Arc::new(registry()),
            desktop.desktop(),
            events,
            TransferSetup {
                remote_addr: SocketAddr::from(([127, 0, 0, 1], 1716)),
                certificate: PeerCertificate::parse(certificate).expect("parse the certificate"),
                payloads,
            },
        );
        (session, events_rx, link_rx)
    }

    #[allow(clippy::type_complexity)]
    fn start_session_with_probe(
        desktop: &MockDesktop,
        payloads: Arc<PayloadService>,
        certificate: &[u8],
    ) -> (
        DeviceSession,
        mpsc::Receiver<DaemonEvent>,
        mpsc::Receiver<Packet>,
        Arc<AtomicUsize>,
    ) {
        let (events, events_rx) = mpsc::channel(64);
        let (link_tx, link_rx) = mpsc::channel(8);
        let (session, probe) = DeviceSession::start_with_probe(
            peer_identity(),
            LinkHandle::from_channel(peer_id(), link_tx),
            Arc::new(registry()),
            desktop.desktop(),
            events,
            TransferSetup {
                remote_addr: SocketAddr::from(([127, 0, 0, 1], 1716)),
                certificate: PeerCertificate::parse(certificate).expect("parse the certificate"),
                payloads,
            },
        );
        (session, events_rx, link_rx, probe)
    }

    async fn wait_for_event(
        events: &mut mpsc::Receiver<DaemonEvent>,
        predicate: impl Fn(&PluginEvent) -> bool,
    ) -> PluginEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = tokio::time::timeout(remaining, events.recv())
                .await
                .expect("an event arrives before the timeout")
                .expect("the event channel is open");
            if let DaemonEvent::PluginEvents { events, .. } = event {
                for event in events {
                    if predicate(&event) {
                        return event;
                    }
                }
            }
        }
    }

    async fn wait_for_temp(desktop: &MockDesktop) {
        let temp = desktop
            .incoming_dir()
            .expect("the mock has a files adapter")
            .join(".handfast-tmp");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let has_temp = std::fs::read_dir(&temp)
                    .map(|mut entries| entries.next().is_some())
                    .unwrap_or(false);
                if has_temp {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a temp file appears");
    }

    fn temp_files(desktop: &MockDesktop) -> usize {
        let temp = desktop
            .incoming_dir()
            .expect("the mock has a files adapter")
            .join(".handfast-tmp");
        std::fs::read_dir(temp)
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    async fn stop_session(session: DeviceSession) {
        session.handle().stop("test done".to_string());
        let task = session.into_task();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the session exits")
            .expect("the session task joins");
    }

    struct RecordingPlugin {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Plugin for RecordingPlugin {
        fn meta(&self) -> &'static PluginMeta {
            &RECORDING
        }

        async fn on_connect(
            &mut self,
            _ctx: &mut PluginContext<'_>,
        ) -> Result<PluginOutput, PluginError> {
            self.calls.lock().unwrap().push("connect");
            Ok(PluginOutput::new())
        }

        async fn on_packet(
            &mut self,
            _ctx: &mut PluginContext<'_>,
            _packet: &Packet,
        ) -> Result<PluginOutput, PluginError> {
            Ok(PluginOutput::new())
        }

        async fn on_disconnect(
            &mut self,
            _ctx: &mut PluginContext<'_>,
        ) -> Result<PluginOutput, PluginError> {
            self.calls.lock().unwrap().push("disconnect");
            Ok(PluginOutput::new()
                .with_packet(Packet::empty(types::PING).expect("a valid empty ping"))
                .with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: "disconnect".to_string(),
                }))
        }
    }

    struct RecordingFactory {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl PluginFactory for RecordingFactory {
        fn meta(&self) -> &'static PluginMeta {
            &RECORDING
        }

        fn create(&self) -> Box<dyn Plugin> {
            Box::new(RecordingPlugin {
                calls: Arc::clone(&self.calls),
            })
        }
    }

    #[tokio::test]
    async fn disconnect_runs_on_disconnect_and_forwards_its_output() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = Arc::new(Registry::new(vec![Box::new(RecordingFactory {
            calls: Arc::clone(&calls),
        })]));
        let peer = peer_identity();
        let device_id = peer.device_id.clone();
        let (link_tx, mut link_rx) = mpsc::channel(8);
        let link = LinkHandle::from_channel(device_id.clone(), link_tx);
        let (events_tx, mut events_rx) = mpsc::channel(8);
        let session = DeviceSession::start(peer, link, registry, Desktop::default(), events_tx);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if calls.lock().unwrap().contains(&"connect") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("on_connect runs");

        session.handle().stop("test disconnect".to_string());
        tokio::time::timeout(Duration::from_secs(5), session.into_task())
            .await
            .expect("the session exits")
            .expect("the session task joins");

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["connect", "disconnect"],
            "on_disconnect runs before the task exits"
        );
        let packet = link_rx
            .recv()
            .await
            .expect("on_disconnect output reaches the link");
        assert_eq!(packet.ptype, types::PING);

        match events_rx.recv().await.expect("the event is forwarded") {
            DaemonEvent::PluginEvents {
                device_id: event_id,
                events,
            } => {
                assert_eq!(event_id, device_id);
                assert!(matches!(
                    events.first(),
                    Some(PluginEvent::Log { message, .. }) if message.as_str() == "disconnect"
                ));
            }
            _ => panic!("expected plugin events"),
        }
    }

    #[tokio::test]
    async fn testkit_sender_file_appears_with_progress_and_completion() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let sender = payload_sender(sender_config());
        let port = sender.port();
        let certificate = sender.cert_der().to_vec();
        let (session, mut events, _link) = start_session(&desktop, payloads(), &certificate);
        let bytes: Vec<u8> = (0..6 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let expected = bytes.clone();
        let send = tokio::spawn(async move { sender.accept_and_send(&bytes).await });

        session
            .handle()
            .deliver_share(file_packet("photo.jpg", expected.len() as u64, port))
            .await
            .expect("queue the share");

        let progress = wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareProgress { .. })
        })
        .await;
        let completed = wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareCompleted { .. })
        })
        .await;

        let report = send
            .await
            .expect("the sender task joins")
            .expect("the sender writes the payload");
        assert_eq!(report.bytes, expected.len() as u64);
        let path = desktop
            .incoming_dir()
            .expect("the mock has a files adapter")
            .join("photo.jpg");
        assert_eq!(std::fs::read(&path).expect("read the saved file"), expected);
        assert!(matches!(
            progress,
            PluginEvent::ShareProgress { bytes, .. } if bytes > 0
        ));
        assert!(matches!(
            completed,
            PluginEvent::ShareCompleted { path: saved, bytes, .. }
                if saved == path && bytes == expected.len() as u64
        ));
        assert_eq!(temp_files(&desktop), 0, "the temp file is gone");
        stop_session(session).await;
    }

    #[tokio::test]
    async fn empty_file_commits_without_a_payload_connection() {
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let certificate = TlsIdentity::generate(peer_id())
            .expect("generate the peer certificate")
            .cert_der()
            .to_vec();
        let (session, mut events, _link) = start_session(&desktop, payloads(), &certificate);

        session
            .handle()
            .deliver_share(empty_file_packet("empty.txt"))
            .await
            .expect("queue the empty file");
        let completed = wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareCompleted { .. })
        })
        .await;

        let path = desktop
            .incoming_dir()
            .expect("the mock has a files adapter")
            .join("empty.txt");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("the empty file exists")
                .len(),
            0
        );
        assert!(matches!(
            completed,
            PluginEvent::ShareCompleted { path: saved, bytes: 0, .. } if saved == path
        ));
        assert_eq!(temp_files(&desktop), 0, "no temp file remains");
        stop_session(session).await;
    }

    #[tokio::test]
    async fn truncated_payload_leaves_no_file_and_emits_share_failed() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let sender = payload_sender(PayloadSenderConfig {
            truncate_after: Some(4),
            ..sender_config()
        });
        let port = sender.port();
        let certificate = sender.cert_der().to_vec();
        let (session, mut events, _link) = start_session(&desktop, payloads(), &certificate);
        let send = tokio::spawn(async move { sender.accept_and_send(b"12345678").await });

        session
            .handle()
            .deliver_share(file_packet("short.bin", 8, port))
            .await
            .expect("queue the share");
        let failed = wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareFailed { .. })
        })
        .await;

        assert!(matches!(
            failed,
            PluginEvent::ShareFailed { reason, .. } if reason.contains("size mismatch")
        ));
        assert!(!desktop
            .incoming_dir()
            .expect("the mock has a files adapter")
            .join("short.bin")
            .exists());
        assert_eq!(temp_files(&desktop), 0, "the temp file is gone");
        let _ = send.await;
        stop_session(session).await;
    }

    #[tokio::test]
    async fn duplicate_name_gets_a_suffix() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let sender = payload_sender(sender_config());
        let port = sender.port();
        let certificate = sender.cert_der().to_vec();
        let (session, mut events, _link) = start_session(&desktop, payloads(), &certificate);
        let handle = session.handle();

        let first_send = sender.accept_and_send(b"one");
        let first_deliver = handle.deliver_share(file_packet("report.txt", 3, port));
        let (first_send, first_deliver) = tokio::join!(first_send, first_deliver);
        first_send.expect("the first payload is sent");
        first_deliver.expect("the first share is queued");
        wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareCompleted { .. })
        })
        .await;

        let second_send = sender.accept_and_send(b"two");
        let second_deliver = handle.deliver_share(file_packet("report.txt", 3, port));
        let (second_send, second_deliver) = tokio::join!(second_send, second_deliver);
        second_send.expect("the second payload is sent");
        second_deliver.expect("the second share is queued");
        wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareCompleted { .. })
        })
        .await;

        let root = desktop
            .incoming_dir()
            .expect("the mock has a files adapter");
        assert_eq!(std::fs::read(root.join("report.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(root.join("report (1).txt")).unwrap(), b"two");
        stop_session(session).await;
    }

    #[tokio::test]
    async fn second_file_connects_before_the_first_finishes() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let sender = payload_sender(PayloadSenderConfig {
            pause_second_until_first_completes: true,
            stall_before_write: Some(Duration::from_millis(800)),
            ..sender_config()
        });
        let port = sender.port();
        let certificate = sender.cert_der().to_vec();
        let (session, mut events, _link, probe) =
            start_session_with_probe(&desktop, payloads(), &certificate);

        session
            .handle()
            .deliver_share(file_packet("first.bin", 4, port))
            .await
            .expect("queue the first file");
        session
            .handle()
            .deliver_share(file_packet("second.bin", 4, port))
            .await
            .expect("queue the second file");

        tokio::time::timeout(Duration::from_secs(2), async {
            while probe.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both eager connects complete");
        let early = tokio::time::timeout(Duration::from_millis(100), events.recv()).await;
        assert!(
            early.is_err(),
            "the first file is still active when the second connects"
        );

        let first = sender.accept_and_send(b"aaaa");
        let second = sender.accept_and_send(b"bbbb");
        let (first, second) = tokio::join!(first, second);
        first.expect("the first payload is sent");
        second.expect("the second payload is sent");

        wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareCompleted { path, .. } if path.ends_with("first.bin"))
        })
        .await;
        wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareCompleted { path, .. } if path.ends_with("second.bin"))
        })
        .await;
        stop_session(session).await;
    }

    #[tokio::test]
    async fn disconnect_mid_transfer_removes_the_temp() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let sender = payload_sender(PayloadSenderConfig {
            stall_before_write: Some(Duration::from_secs(5)),
            ..sender_config()
        });
        let port = sender.port();
        let certificate = sender.cert_der().to_vec();
        let (session, _events, _link) = start_session(&desktop, payloads(), &certificate);
        let send = tokio::spawn(async move { sender.accept_and_send(b"payload").await });

        session
            .handle()
            .deliver_share(file_packet("partial.bin", 7, port))
            .await
            .expect("queue the share");
        wait_for_temp(&desktop).await;
        stop_session(session).await;

        assert!(!desktop
            .incoming_dir()
            .expect("the mock has a files adapter")
            .join("partial.bin")
            .exists());
        assert_eq!(temp_files(&desktop), 0, "the temp file is gone");
        send.abort();
    }

    #[tokio::test]
    async fn session_exit_leaves_no_part_file() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let sender = payload_sender(PayloadSenderConfig {
            stall_before_write: Some(Duration::from_secs(5)),
            ..sender_config()
        });
        let port = sender.port();
        let certificate = sender.cert_der().to_vec();
        let (session, _events, _link) = start_session(&desktop, payloads(), &certificate);
        let send = tokio::spawn(async move { sender.accept_and_send(b"payload").await });

        session
            .handle()
            .deliver_share(file_packet("partial.bin", 7, port))
            .await
            .expect("queue the share");
        wait_for_temp(&desktop).await;
        let task = session.into_task();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the session exits")
            .expect("the session task joins");

        assert_eq!(temp_files(&desktop), 0, "the temp file is gone");
        send.abort();
    }

    #[tokio::test]
    async fn queued_receive_over_the_cap_fails_with_an_event() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let active_sender = payload_sender(PayloadSenderConfig {
            stall_before_write: Some(Duration::from_secs(5)),
            ..sender_config()
        });
        let active_port = active_sender.port();
        let queued_sender = payload_sender(sender_config());
        let queued_port = queued_sender.port();
        let certificate = active_sender.cert_der().to_vec();
        let (session, mut events, _link) = start_session(&desktop, payloads(), &certificate);
        let active_send = tokio::spawn(async move { active_sender.accept_and_send(b"x").await });

        session
            .handle()
            .deliver_share(file_packet("active.bin", 1, active_port))
            .await
            .expect("queue the active file");
        wait_for_temp(&desktop).await;
        for index in 0..MAX_QUEUED_RECEIVES {
            session
                .handle()
                .deliver_share(file_packet(&format!("queued-{index}.bin"), 1, queued_port))
                .await
                .expect("queue a receive under the cap");
        }
        session
            .handle()
            .deliver_share(file_packet("overflow.bin", 1, queued_port))
            .await
            .expect("the share channel accepts the packet");

        let failed = wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareFailed { transfer_id: 0, .. })
        })
        .await;
        assert!(matches!(
            failed,
            PluginEvent::ShareFailed { reason, .. } if reason.contains("queue full")
        ));
        stop_session(session).await;
        active_send.abort();
        drop(queued_sender);
    }

    #[tokio::test]
    async fn full_session_share_queue_returns_an_error_within_two_seconds() {
        let (tx, _rx) = mpsc::channel(SESSION_QUEUE);
        let handle = SessionHandle {
            tx,
            delivery_timeout: SHARE_DELIVERY_TIMEOUT,
        };
        for _ in 0..SESSION_QUEUE {
            handle.deliver(Packet::empty(types::PING).expect("a valid empty ping"));
        }

        let start = Instant::now();
        let error = handle
            .deliver_share(Packet::empty(types::SHARE_REQUEST).expect("a valid share packet"))
            .await
            .expect_err("the full queue rejects the share");
        let elapsed = start.elapsed();
        assert_eq!(error, SessionError::Full);
        assert!(
            elapsed >= Duration::from_millis(1900),
            "waits for the delivery timeout: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "returns within the two-second contract: {elapsed:?}"
        );
    }
}
