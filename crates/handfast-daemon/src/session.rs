//! One plugin host per paired connected device.
//!
//! The session owns the [`DeviceHost`], dispatches inbound packets, forwards
//! plugin output to the link and plugin events to the daemon, runs the inbound
//! and outbound transfer runner, and runs `on_disconnect` before it exits. The
//! daemon event loop owns the session map; the manager never sees a
//! `DeviceHost`.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_desktop::{Desktop, Files, FilesError, IncomingRequest, OutgoingFile};
use handfast_net::{
    LinkHandle, NetError, PayloadError, PayloadPeer, PayloadService, PayloadSource,
    PeerCertificate, PendingPayload, TransferOutcome,
};
use handfast_proto::{
    file_request, DeviceId, Identity, Packet, PayloadSize, ProtoError, ShareFile,
};
use handfast_runtime::{Device, DeviceHost, Dispatch, PluginEvent, Registry, Transfer};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinHandle, JoinSet};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};

/// Inbound queue for one session. A full queue drops packets rather than
/// blocking the daemon event loop.
const SESSION_QUEUE: usize = 64;

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

/// Queue bounds for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// How long a share packet waits for room in the session queue.
    pub delivery_timeout: Duration,
    /// How many eager receives one peer may queue behind the active transfer.
    pub max_queued_receives: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            delivery_timeout: Duration::from_secs(2),
            max_queued_receives: 8,
        }
    }
}

/// Payload inputs for one session with a transfer runner.
pub struct TransferSetup {
    /// Authenticated control link address. The payload dials this IP.
    pub remote_addr: SocketAddr,
    /// Certificate the control link authenticated.
    pub certificate: PeerCertificate,
    /// Shared payload byte mover.
    pub payloads: Arc<PayloadService>,
    /// Session queue bounds.
    pub limits: SessionLimits,
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
        limits: SessionLimits,
    ) -> Self {
        Self::start_inner(peer, link, registry, desktop, events, limits, None)
    }

    /// Start a session with the share transfer runner.
    ///
    /// The runner exists only when the desktop has a Files adapter. Without one
    /// the share plugin is disabled, so no transfer intent can arrive and an
    /// outbound share is unavailable.
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
            max_queued_receives: setup.limits.max_queued_receives,
            #[cfg(test)]
            connects: Arc::new(AtomicUsize::new(0)),
        });
        Self::start_inner(peer, link, registry, desktop, events, setup.limits, deps)
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
            max_queued_receives: setup.limits.max_queued_receives,
            connects: Arc::clone(&connects),
        });
        (
            Self::start_inner(peer, link, registry, desktop, events, setup.limits, deps),
            connects,
        )
    }

    fn start_inner(
        peer: Identity,
        link: LinkHandle,
        registry: Arc<Registry>,
        desktop: Desktop,
        events: mpsc::Sender<DaemonEvent>,
        limits: SessionLimits,
        deps: Option<TransferDeps>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(SESSION_QUEUE);
        let (stop_tx, stop_rx) = watch::channel(None);
        let handle = SessionHandle {
            tx,
            stop_tx,
            delivery_timeout: limits.delivery_timeout,
            drops: Arc::new(AtomicU64::new(0)),
        };
        let task = tokio::spawn(run(
            peer,
            link,
            registry,
            desktop,
            events,
            SessionChannels {
                input: rx,
                stop: stop_rx,
            },
            deps,
        ));
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
    stop_tx: watch::Sender<Option<String>>,
    delivery_timeout: Duration,
    drops: Arc<AtomicU64>,
}

impl SessionHandle {
    /// Deliver one packet without waiting. A full queue drops the packet.
    pub fn deliver(&self, packet: Packet) {
        if let Err(error) = self.tx.try_send(SessionInput::Packet(packet)) {
            let full = matches!(&error, mpsc::error::TrySendError::Full(_));
            // `try_send` hands the input back; only packets use this path.
            if let SessionInput::Packet(packet) = error.into_inner() {
                if full {
                    let mut count = self.drops.load(Ordering::Relaxed);
                    let dropped = loop {
                        let next = count.saturating_add(1);
                        match self.drops.compare_exchange_weak(
                            count,
                            next,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break next,
                            Err(current) => count = current,
                        }
                    };
                    tracing::warn!(
                        packet_type = %packet.ptype,
                        dropped,
                        "dropping a packet for a full session queue"
                    );
                } else {
                    tracing::debug!("dropping a packet for a closed session");
                }
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

    /// Queue one outbound file and wait for the payload to finish.
    ///
    /// Waits up to the session delivery timeout for room in the session queue,
    /// then waits for the session to run the transfer. A zero-byte file sends a
    /// metadata-only packet and opens no payload listener.
    ///
    /// # Errors
    ///
    /// Fails with [`TransferError::Session`] when the queue stays full for the
    /// delivery timeout or the session stopped before it replied. Otherwise
    /// returns the file, packet or payload failure.
    pub async fn share_file(&self, path: PathBuf) -> Result<TransferOutcome, TransferError> {
        let (reply, reply_rx) = oneshot::channel();
        match tokio::time::timeout(
            self.delivery_timeout,
            self.tx.send(SessionInput::ShareFile { path, reply }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(SessionError::Closed.into()),
            Err(_) => return Err(SessionError::Full.into()),
        }
        reply_rx
            .await
            .map_err(|_| TransferError::Session(SessionError::Closed))?
    }

    /// Stop the session. `on_disconnect` runs before the task exits.
    pub fn stop(&self, reason: String) {
        self.stop_tx.send_replace(Some(reason));
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

/// Why an outbound share failed.
#[derive(Debug)]
pub enum TransferError {
    /// The source file could not be opened.
    Files(FilesError),
    /// The payload listener or stream failed.
    Payload(PayloadError),
    /// The share packet could not be built.
    Proto(ProtoError),
    /// The share packet could not be sent on the control link.
    Net(NetError),
    /// The session queue rejected the request or the session stopped.
    Session(SessionError),
    /// The session has no files adapter.
    Unavailable(&'static str),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Files(error) => write!(formatter, "files: {error}"),
            Self::Payload(error) => write!(formatter, "payload: {error}"),
            Self::Proto(error) => write!(formatter, "protocol: {error}"),
            Self::Net(error) => write!(formatter, "link: {error}"),
            Self::Session(error) => write!(formatter, "session: {error}"),
            Self::Unavailable(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for TransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Files(error) => Some(error),
            Self::Payload(error) => Some(error),
            Self::Proto(error) => Some(error),
            Self::Net(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::Unavailable(_) => None,
        }
    }
}

impl From<FilesError> for TransferError {
    fn from(error: FilesError) -> Self {
        Self::Files(error)
    }
}

impl From<PayloadError> for TransferError {
    fn from(error: PayloadError) -> Self {
        Self::Payload(error)
    }
}

impl From<ProtoError> for TransferError {
    fn from(error: ProtoError) -> Self {
        Self::Proto(error)
    }
}

impl From<NetError> for TransferError {
    fn from(error: NetError) -> Self {
        Self::Net(error)
    }
}

impl From<SessionError> for TransferError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

/// Input to a session task.
pub enum SessionInput {
    /// One inbound packet on the best-effort path.
    Packet(Packet),
    /// One share packet on the reliable path.
    Share(Packet),
    /// One outbound file share. The session replies exactly once.
    ShareFile {
        /// Local source file.
        path: PathBuf,
        /// Transfer result.
        reply: oneshot::Sender<Result<TransferOutcome, TransferError>>,
    },
}

/// Payload and file adapters for the inbound transfer runner.
struct TransferDeps {
    peer: PayloadPeer,
    files: Arc<dyn Files>,
    payloads: Arc<PayloadService>,
    max_queued_receives: usize,
    #[cfg(test)]
    connects: Arc<AtomicUsize>,
}

/// The result of one eager connect task.
type ConnectResult = (u64, Result<PendingPayload, PayloadError>);

/// The join outcome of one eager connect task.
type ConnectJoin = Result<ConnectResult, JoinError>;

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
struct ActiveReceive;

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
    connects: JoinSet<ConnectResult>,
    sends: JoinSet<()>,
    next_id: u64,
    max_queued_receives: usize,
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
            max_queued_receives,
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
            sends: JoinSet::new(),
            next_id: 1,
            max_queued_receives,
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
        if self.slots.len() >= self.max_queued_receives {
            tracing::warn!(
                device_id = %self.peer.device_id,
                max = self.max_queued_receives,
                "receive queue full"
            );
            emit(
                &self.events,
                &self.peer.device_id,
                PluginEvent::ShareFailed {
                    transfer_id: 0,
                    reason: format!("receive queue full (max {})", self.max_queued_receives),
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
        self.active = Some(ActiveReceive);
        let files = Arc::clone(&self.files);
        let payloads = Arc::clone(&self.payloads);
        let events = self.events.clone();
        let device_id = self.peer.device_id.clone();
        self.receives.spawn(async move {
            receive_transfer(id, intent, pending, files, payloads, events, device_id).await;
        });
    }

    fn receive_finished(&mut self, joined: Option<Result<(), JoinError>>) {
        if let Some(Err(error)) = joined {
            tracing::warn!(
                device_id = %self.peer.device_id,
                %error,
                "receive task ended abnormally"
            );
        }
        self.active = None;
    }

    async fn connect_finished(&mut self, joined: Option<ConnectJoin>) {
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

    /// Spawn one outbound share. The reply resolves when the transfer ends.
    fn share_file(
        &mut self,
        path: PathBuf,
        link: &LinkHandle,
        reply: oneshot::Sender<Result<TransferOutcome, TransferError>>,
    ) {
        let files = Arc::clone(&self.files);
        let payloads = Arc::clone(&self.payloads);
        let peer = self.peer.clone();
        let link = LinkHandle::clone(link);
        self.sends.spawn(async move {
            let result = outbound_share(files, payloads, peer, path, link).await;
            if reply.send(result).is_err() {
                tracing::debug!("outbound share reply dropped");
            }
        });
    }

    fn send_finished(&mut self, joined: Option<Result<(), JoinError>>) {
        if let Some(Err(error)) = joined {
            tracing::warn!(
                device_id = %self.peer.device_id,
                %error,
                "share task ended abnormally"
            );
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
        self.sends.abort_all();
        while self.sends.join_next().await.is_some() {}
        self.slots.clear();
        self.active = None;
    }
}

/// One step of the session loop.
enum SessionStep {
    Input(Option<SessionInput>),
    Stop(String),
    Receive(Option<Result<(), JoinError>>),
    Connect(Option<ConnectJoin>),
    Send(Option<Result<(), JoinError>>),
}

async fn next_stop(stop: &mut watch::Receiver<Option<String>>) -> String {
    loop {
        if let Some(reason) = stop.borrow_and_update().clone() {
            return reason;
        }
        if stop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

struct SessionChannels {
    input: mpsc::Receiver<SessionInput>,
    stop: watch::Receiver<Option<String>>,
}

async fn run(
    peer: Identity,
    link: LinkHandle,
    registry: Arc<Registry>,
    desktop: Desktop,
    events: mpsc::Sender<DaemonEvent>,
    channels: SessionChannels,
    deps: Option<TransferDeps>,
) {
    let SessionChannels {
        mut input,
        mut stop,
    } = channels;
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

        let (receives_open, connects_open, sends_open) = match runner.as_ref() {
            Some(runner) => (
                !runner.receives.is_empty(),
                !runner.connects.is_empty(),
                !runner.sends.is_empty(),
            ),
            None => (false, false, false),
        };
        let step = match runner.as_mut() {
            Some(runner) => {
                tokio::select! {
                    biased;
                    reason = next_stop(&mut stop) => SessionStep::Stop(reason),
                    input = input.recv() => SessionStep::Input(input),
                    joined = runner.receives.join_next(), if receives_open => {
                        SessionStep::Receive(joined)
                    }
                    joined = runner.connects.join_next(), if connects_open => {
                        SessionStep::Connect(joined)
                    }
                    joined = runner.sends.join_next(), if sends_open => {
                        SessionStep::Send(joined)
                    }
                }
            }
            None => {
                tokio::select! {
                    biased;
                    reason = next_stop(&mut stop) => SessionStep::Stop(reason),
                    input = input.recv() => SessionStep::Input(input),
                }
            }
        };

        match step {
            SessionStep::Stop(reason) => break reason,
            SessionStep::Input(None) => break "session channel closed".to_string(),
            SessionStep::Input(Some(input)) => {
                if let Some(reason) = stop.borrow().clone() {
                    break reason;
                }
                handle_input(input, &mut host, &mut runner, &link, &events, &device_id).await;
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
            SessionStep::Send(joined) => {
                if let Some(runner) = runner.as_mut() {
                    runner.send_finished(joined);
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
) {
    match input {
        SessionInput::Packet(packet) | SessionInput::Share(packet) => {
            let dispatch = host.dispatch(&packet).await;
            let transfers = send_dispatch(link, events, device_id, dispatch).await;
            if let Some(runner) = runner.as_mut() {
                runner.enqueue_all(transfers).await;
            }
        }
        SessionInput::ShareFile { path, reply } => match runner.as_mut() {
            Some(runner) => runner.share_file(path, link, reply),
            None => {
                let _ = reply.send(Err(TransferError::Unavailable(
                    "session has no files adapter",
                )));
            }
        },
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

/// Run one outbound share to completion and return its outcome.
async fn outbound_share(
    files: Arc<dyn Files>,
    payloads: Arc<PayloadService>,
    peer: PayloadPeer,
    path: PathBuf,
    link: LinkHandle,
) -> Result<TransferOutcome, TransferError> {
    let outgoing = files.open_outgoing(&path).await?;
    let OutgoingFile {
        filename,
        size,
        created_ms,
        modified_ms,
        reader,
    } = outgoing;
    let file = ShareFile {
        filename: Some(filename),
        size: PayloadSize::Known(size),
        created_ms,
        modified_ms,
        open: false,
        batch: None,
    };
    tracing::debug!(device_id = %peer.device_id, size, "outbound share starting");

    if size == 0 {
        let packet = file_request(&file, None)?;
        link.send(packet).await?;
        return Ok(TransferOutcome {
            bytes: 0,
            extra_bytes: 0,
        });
    }

    let prepared = payloads
        .prepare_send(
            peer.clone(),
            PayloadSource {
                size: Some(size),
                reader: Box::new(reader),
            },
        )
        .await?;
    let packet = file_request(&file, Some(prepared.port()))?;
    if let Err(error) = link.send(packet).await {
        // Dropping the prepared send closes the listener and releases the
        // active permit, so a failed packet send leaves no port open.
        drop(prepared);
        return Err(TransferError::Net(error));
    }
    let outcome = prepared.start(None).await?;
    tracing::debug!(
        device_id = %peer.device_id,
        bytes = outcome.bytes,
        "outbound share finished"
    );
    Ok(outcome)
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
    let mut incoming = match files
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use handfast_desktop::{Desktop, FilesError, MockDesktop};
    use handfast_net::{NetError, PayloadLimits, TlsIdentity};
    use handfast_plugins::registry;
    use handfast_proto::{
        file_request, text_request, types, Capabilities, DeviceName, DeviceType, ShareFile,
        ShareRequest, PAYLOAD_PORT_MAX, PAYLOAD_PORT_MIN,
    };
    use handfast_runtime::{
        DesktopRequirement, LogLevel, Plugin, PluginContext, PluginError, PluginFactory,
        PluginMeta, PluginOutput, PluginStatus, Registry,
    };
    use handfast_testkit::payload_peer::{
        PayloadReceiver, PayloadReceiverConfig, PayloadSender, PayloadSenderConfig,
    };
    use handfast_testkit::peer::PeerConfig;
    use handfast_testkit::tls_peer::TlsVersion;
    use tokio::sync::{mpsc, Notify};

    static RECORDING: PluginMeta = PluginMeta {
        name: "recording",
        title: "Recording",
        status: PluginStatus::Implemented,
        incoming: &[],
        outgoing: &[],
        desktop: DesktopRequirement::None,
    };

    static GATED: PluginMeta = PluginMeta {
        name: "gated",
        title: "Gated",
        status: PluginStatus::Implemented,
        incoming: &[types::PING],
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

    fn one_port_payloads(port: u16) -> Arc<PayloadService> {
        let tls = Arc::new(TlsIdentity::generate(DeviceId::generate()).expect("generate TLS"));
        Arc::new(PayloadService::new(
            tls,
            PayloadLimits {
                connect_timeout: Duration::from_secs(2),
                handshake_timeout: Duration::from_secs(2),
                accept_timeout: Duration::from_secs(2),
                io_timeout: Duration::from_secs(2),
                port_min: port,
                port_max: port,
                ..PayloadLimits::default()
            },
        ))
    }

    fn long_accept_payloads() -> Arc<PayloadService> {
        let tls = Arc::new(TlsIdentity::generate(DeviceId::generate()).expect("generate TLS"));
        Arc::new(PayloadService::new(
            tls,
            PayloadLimits {
                connect_timeout: Duration::from_secs(2),
                handshake_timeout: Duration::from_secs(2),
                accept_timeout: Duration::from_secs(30),
                io_timeout: Duration::from_secs(2),
                ..PayloadLimits::default()
            },
        ))
    }

    fn free_payload_port() -> u16 {
        for port in PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX {
            if std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() {
                return port;
            }
        }
        panic!("no free payload port in 1739-1764");
    }

    fn occupied_payload_port() -> (std::net::TcpListener, u16) {
        for port in PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX {
            if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
                return (listener, port);
            }
        }
        panic!("no free payload port in 1739-1764");
    }

    fn payload_receiver() -> PayloadReceiver {
        PayloadReceiver::new(PayloadReceiverConfig::new(
            PeerConfig::server(),
            TlsVersion::Both,
        ))
        .expect("build the payload receiver")
    }

    fn source_file(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let path = dir.path().join("report.pdf");
        std::fs::write(&path, bytes).expect("write the source file");
        (dir, path)
    }

    /// Drain the link channel so the session loop never blocks on it, and
    /// forward the share packets it saw.
    fn share_packets(
        mut link_rx: mpsc::Receiver<Packet>,
    ) -> (mpsc::Receiver<Packet>, JoinHandle<()>) {
        let (share_tx, share_rx) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            while let Some(packet) = link_rx.recv().await {
                if packet.ptype != types::SHARE_REQUEST {
                    continue;
                }
                if share_tx.send(packet).await.is_err() {
                    break;
                }
            }
        });
        (share_rx, task)
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
                limits: SessionLimits::default(),
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
                limits: SessionLimits::default(),
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

    struct GatedPlugin {
        packets: Arc<AtomicUsize>,
        entered: Arc<Notify>,
        gate: Arc<Notify>,
        disconnects: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Plugin for GatedPlugin {
        fn meta(&self) -> &'static PluginMeta {
            &GATED
        }

        async fn on_packet(
            &mut self,
            _ctx: &mut PluginContext<'_>,
            _packet: &Packet,
        ) -> Result<PluginOutput, PluginError> {
            self.packets.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.gate.notified().await;
            Ok(PluginOutput::new())
        }

        async fn on_disconnect(
            &mut self,
            _ctx: &mut PluginContext<'_>,
        ) -> Result<PluginOutput, PluginError> {
            self.disconnects.fetch_add(1, Ordering::SeqCst);
            Ok(PluginOutput::new())
        }
    }

    struct GatedFactory {
        packets: Arc<AtomicUsize>,
        entered: Arc<Notify>,
        gate: Arc<Notify>,
        disconnects: Arc<AtomicUsize>,
    }

    impl PluginFactory for GatedFactory {
        fn meta(&self) -> &'static PluginMeta {
            &GATED
        }

        fn create(&self) -> Box<dyn Plugin> {
            Box::new(GatedPlugin {
                packets: Arc::clone(&self.packets),
                entered: Arc::clone(&self.entered),
                gate: Arc::clone(&self.gate),
                disconnects: Arc::clone(&self.disconnects),
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
        let session = DeviceSession::start(
            peer,
            link,
            registry,
            Desktop::default(),
            events_tx,
            SessionLimits::default(),
        );

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
    async fn stop_preempts_a_full_queue_and_disconnects_once() {
        let packets = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let disconnects = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(Registry::new(vec![Box::new(GatedFactory {
            packets: Arc::clone(&packets),
            entered: Arc::clone(&entered),
            gate: Arc::clone(&gate),
            disconnects: Arc::clone(&disconnects),
        })]));
        let (link_tx, mut link_rx) = mpsc::channel(8);
        let drain = tokio::spawn(async move { while link_rx.recv().await.is_some() {} });
        let (events, _events_rx) = mpsc::channel(8);
        let session = DeviceSession::start(
            peer_identity(),
            LinkHandle::from_channel(peer_id(), link_tx),
            registry,
            Desktop::default(),
            events,
            SessionLimits::default(),
        );

        session
            .handle()
            .deliver(Packet::empty(types::PING).expect("a valid empty ping"));
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("the first packet reaches the plugin");

        for _ in 0..SESSION_QUEUE {
            session
                .handle()
                .deliver(Packet::empty(types::PING).expect("a valid empty ping"));
        }
        assert_eq!(
            packets.load(Ordering::SeqCst),
            1,
            "one packet is in flight and the queue is full"
        );

        session.handle().stop("test stop".to_string());
        let dispatched = packets.load(Ordering::SeqCst);
        gate.notify_one();

        tokio::time::timeout(Duration::from_secs(5), session.into_task())
            .await
            .expect("the session exits promptly")
            .expect("the session task joins");

        assert_eq!(
            packets.load(Ordering::SeqCst),
            dispatched,
            "no queued packet is dispatched after the stop"
        );
        assert_eq!(dispatched, 1, "only the in-flight packet ran");
        assert_eq!(
            disconnects.load(Ordering::SeqCst),
            1,
            "on_disconnect runs once"
        );
        drain.abort();
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
        for index in 0..SessionLimits::default().max_queued_receives {
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
            stop_tx: watch::channel(None).0,
            delivery_timeout: SessionLimits::default().delivery_timeout,
            drops: Arc::new(AtomicU64::new(0)),
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

    #[tokio::test]
    async fn full_session_queue_counts_the_drop_and_keeps_queued_packets() {
        let (tx, mut rx) = mpsc::channel(SESSION_QUEUE);
        let handle = SessionHandle {
            tx,
            stop_tx: watch::channel(None).0,
            delivery_timeout: SessionLimits::default().delivery_timeout,
            drops: Arc::new(AtomicU64::new(0)),
        };
        for _ in 0..SESSION_QUEUE {
            handle.deliver(Packet::empty(types::PING).expect("a valid empty ping"));
        }
        assert_eq!(handle.drops.load(Ordering::Relaxed), 0);

        handle.deliver(Packet::empty(types::SHARE_REQUEST).expect("a valid share packet"));

        assert_eq!(
            handle.drops.load(Ordering::Relaxed),
            1,
            "the full queue counts the drop"
        );
        assert_eq!(rx.len(), SESSION_QUEUE, "the queued packets remain");
        let queued = rx.try_recv().expect("a queued packet remains");
        assert!(
            matches!(queued, SessionInput::Packet(packet) if packet.ptype == types::PING),
            "the queued packet is untouched"
        );
    }

    #[tokio::test]
    async fn outbound_share_sends_packet_and_streams_bytes() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let receiver = payload_receiver();
        let certificate = receiver
            .cert_der()
            .expect("the receiver presents a certificate")
            .to_vec();
        let (session, _events, link_rx) = start_session(&desktop, payloads(), &certificate);
        let (mut shares, _drain) = share_packets(link_rx);
        let bytes: Vec<u8> = (0..256 * 1024).map(|index| (index % 251) as u8).collect();
        let len = bytes.len() as u64;
        let (_source, path) = source_file(&bytes);
        let expected = bytes;
        let handle = session.handle();
        let share = tokio::spawn(async move { handle.share_file(path).await });
        let receive = tokio::spawn(async move {
            let packet = tokio::time::timeout(Duration::from_secs(10), shares.recv())
                .await
                .expect("the share packet arrives")
                .expect("the drain forwards the share packet");
            let port = packet
                .payload_transfer_info
                .as_ref()
                .expect("a payload port")
                .port;
            let mut buffer = Vec::new();
            let report = receiver
                .receive(SocketAddr::from(([127, 0, 0, 1], port)), len, &mut buffer)
                .await
                .expect("the receiver reads the payload");
            (packet, buffer, report)
        });

        let outcome = share
            .await
            .expect("the share task joins")
            .expect("the outbound share succeeds");
        let (packet, buffer, report) = receive.await.expect("the receive task joins");

        match ShareRequest::from_packet(&packet).expect("a share request") {
            ShareRequest::File(file) => {
                assert_eq!(file.filename.as_deref(), Some("report.pdf"));
                assert_eq!(file.size, PayloadSize::Known(len));
            }
            other => panic!("expected a file request, got {other:?}"),
        }
        let offer = packet
            .payload_offer()
            .expect("a valid payload offer")
            .expect("a payload");
        assert!((PAYLOAD_PORT_MIN..=PAYLOAD_PORT_MAX).contains(&offer.port));
        assert_eq!(buffer, expected);
        assert_eq!(report.bytes, len);
        assert!(report.eof, "the receiver sees EOF");
        assert_eq!(
            outcome,
            TransferOutcome {
                bytes: len,
                extra_bytes: 0,
            }
        );
        assert_eq!(temp_files(&desktop), 0, "no temp file is written");
        stop_session(session).await;
    }

    #[tokio::test]
    async fn outbound_large_file_completes() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let receiver = payload_receiver();
        let certificate = receiver
            .cert_der()
            .expect("the receiver presents a certificate")
            .to_vec();
        let (session, _events, link_rx) = start_session(&desktop, payloads(), &certificate);
        let (mut shares, _drain) = share_packets(link_rx);
        let size = 8 * 1024 * 1024;
        let bytes: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
        let expected = bytes.clone();
        let (_source, path) = source_file(&bytes);
        let handle = session.handle();
        let share = tokio::spawn(async move { handle.share_file(path).await });
        let receive = tokio::spawn(async move {
            let packet = tokio::time::timeout(Duration::from_secs(10), shares.recv())
                .await
                .expect("the share packet arrives")
                .expect("the drain forwards the share packet");
            let port = packet
                .payload_transfer_info
                .as_ref()
                .expect("a payload port")
                .port;
            let mut buffer = Vec::new();
            let report = receiver
                .receive(
                    SocketAddr::from(([127, 0, 0, 1], port)),
                    size as u64,
                    &mut buffer,
                )
                .await
                .expect("the receiver reads the payload");
            (buffer, report)
        });

        let outcome = share
            .await
            .expect("the share task joins")
            .expect("the outbound share succeeds");
        let (buffer, report) = receive.await.expect("the receive task joins");

        assert_eq!(report.bytes, size as u64);
        assert!(report.eof, "the receiver sees EOF");
        assert_eq!(buffer, expected);
        assert_eq!(outcome.bytes, size as u64);
        assert_eq!(outcome.extra_bytes, 0);
        assert_eq!(temp_files(&desktop), 0, "no temp file is written");
        stop_session(session).await;
    }

    #[tokio::test]
    async fn outbound_transfer_does_not_stall_the_session_loop() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let receiver = payload_receiver();
        let certificate = receiver
            .cert_der()
            .expect("the receiver presents a certificate")
            .to_vec();
        let (session, mut events, link_rx) =
            start_session(&desktop, long_accept_payloads(), &certificate);
        let (mut shares, _drain) = share_packets(link_rx);
        let (_source, path) = source_file(b"payload");
        let handle = session.handle();
        let share = tokio::spawn(async move { handle.share_file(path).await });

        let _packet = tokio::time::timeout(Duration::from_secs(10), shares.recv())
            .await
            .expect("the share packet arrives")
            .expect("the drain forwards the share packet");
        session
            .handle()
            .deliver_share(text_request("hello").expect("build the text share"))
            .await
            .expect("the text share is queued");
        let event = wait_for_event(&mut events, |event| {
            matches!(event, PluginEvent::ShareText { .. })
        })
        .await;
        assert!(
            matches!(event, PluginEvent::ShareText { text } if text == "hello"),
            "the session handles a share while the transfer waits for accept"
        );
        assert!(
            !share.is_finished(),
            "the outbound transfer is still waiting for the receiver"
        );

        stop_session(session).await;
        let error = share
            .await
            .expect("the share task joins")
            .expect_err("the stopped session drops the reply");
        assert!(
            matches!(error, TransferError::Session(SessionError::Closed)),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn outbound_empty_file_sends_metadata_only() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let (occupied, port) = occupied_payload_port();
        occupied
            .set_nonblocking(true)
            .expect("the occupied listener is nonblocking");
        let certificate = TlsIdentity::generate(peer_id())
            .expect("generate the peer certificate")
            .cert_der()
            .to_vec();
        let (session, _events, link_rx) =
            start_session(&desktop, one_port_payloads(port), &certificate);
        let (mut shares, _drain) = share_packets(link_rx);
        let (_source, path) = source_file(b"");
        let handle = session.handle();
        let share = tokio::spawn(async move { handle.share_file(path).await });

        let packet = tokio::time::timeout(Duration::from_secs(10), shares.recv())
            .await
            .expect("the share packet arrives")
            .expect("the drain forwards the share packet");
        let outcome = share
            .await
            .expect("the share task joins")
            .expect("the empty file shares");

        assert_eq!(
            outcome,
            TransferOutcome {
                bytes: 0,
                extra_bytes: 0,
            }
        );
        assert_eq!(packet.payload_size, None);
        assert_eq!(packet.payload_transfer_info, None);
        assert_eq!(packet.payload_offer().expect("a valid packet"), None);
        assert_eq!(
            occupied
                .accept()
                .expect_err("no payload connection is dialed")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        stop_session(session).await;
    }

    #[tokio::test]
    async fn outbound_no_free_payload_port_fails_before_packet() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let (occupied, port) = occupied_payload_port();
        let certificate = TlsIdentity::generate(peer_id())
            .expect("generate the peer certificate")
            .cert_der()
            .to_vec();
        let (session, _events, link_rx) =
            start_session(&desktop, one_port_payloads(port), &certificate);
        let (mut shares, _drain) = share_packets(link_rx);
        let (_source, path) = source_file(b"payload");

        let error = session
            .handle()
            .share_file(path)
            .await
            .expect_err("the occupied port fails the share");
        assert!(
            matches!(error, TransferError::Payload(PayloadError::NoPort)),
            "{error:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), shares.recv())
                .await
                .is_err(),
            "no share packet is sent"
        );
        drop(occupied);
        stop_session(session).await;
    }

    #[tokio::test]
    async fn outbound_packet_send_failure_closes_the_listener() {
        let _ports = ports().await;
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let port = free_payload_port();
        let certificate = TlsIdentity::generate(peer_id())
            .expect("generate the peer certificate")
            .cert_der()
            .to_vec();
        let (session, _events, link_rx) =
            start_session(&desktop, one_port_payloads(port), &certificate);
        drop(link_rx);
        let (_source, path) = source_file(b"payload");

        let error = session
            .handle()
            .share_file(path)
            .await
            .expect_err("the closed link fails the share");
        assert!(
            matches!(error, TransferError::Net(NetError::LinkClosed)),
            "{error:?}"
        );
        let rebind =
            std::net::TcpListener::bind(("0.0.0.0", port)).expect("the payload listener is closed");
        drop(rebind);
        stop_session(session).await;
    }

    #[tokio::test]
    async fn outbound_missing_or_directory_path_fails_before_packet() {
        let desktop = MockDesktop::with_files().expect("build the mock desktop");
        let certificate = TlsIdentity::generate(peer_id())
            .expect("generate the peer certificate")
            .cert_der()
            .to_vec();
        let (session, _events, link_rx) = start_session(&desktop, payloads(), &certificate);
        let (mut shares, _drain) = share_packets(link_rx);
        let dir = tempfile::TempDir::new().expect("a temp dir");

        let error = session
            .handle()
            .share_file(dir.path().join("missing.bin"))
            .await
            .expect_err("a missing path fails");
        assert!(matches!(error, TransferError::Files(_)), "{error:?}");
        let error = session
            .handle()
            .share_file(dir.path().to_path_buf())
            .await
            .expect_err("a directory fails");
        assert!(
            matches!(error, TransferError::Files(FilesError::NotRegularFile(_))),
            "{error:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), shares.recv())
                .await
                .is_err(),
            "no share packet is sent"
        );
        stop_session(session).await;
    }

    #[tokio::test]
    async fn share_file_full_queue_returns_within_two_seconds() {
        let (tx, _rx) = mpsc::channel(SESSION_QUEUE);
        let handle = SessionHandle {
            tx,
            stop_tx: watch::channel(None).0,
            delivery_timeout: SessionLimits::default().delivery_timeout,
            drops: Arc::new(AtomicU64::new(0)),
        };
        for _ in 0..SESSION_QUEUE {
            handle.deliver(Packet::empty(types::PING).expect("a valid empty ping"));
        }

        let start = Instant::now();
        let error = handle
            .share_file(PathBuf::from("unused"))
            .await
            .expect_err("the full queue rejects the share");
        let elapsed = start.elapsed();
        assert!(
            matches!(error, TransferError::Session(SessionError::Full)),
            "{error:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(1900),
            "waits for the delivery timeout: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "returns within the two-second contract: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn share_file_without_a_files_adapter_is_unavailable() {
        let (events, _events_rx) = mpsc::channel(8);
        let (link_tx, mut link_rx) = mpsc::channel(8);
        let drain = tokio::spawn(async move { while link_rx.recv().await.is_some() {} });
        let session = DeviceSession::start(
            peer_identity(),
            LinkHandle::from_channel(peer_id(), link_tx),
            Arc::new(registry()),
            Desktop::default(),
            events,
            SessionLimits::default(),
        );

        let error = session
            .handle()
            .share_file(PathBuf::from("unused"))
            .await
            .expect_err("the session has no files adapter");
        assert!(matches!(error, TransferError::Unavailable(_)), "{error:?}");
        stop_session(session).await;
        drain.await.expect("the drain task joins");
    }
}
