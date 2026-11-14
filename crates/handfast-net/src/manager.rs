//! The link manager actor.
//!
//! One task owns the listener, the trust snapshot, the admission policy, the
//! peer table and the active links. Discovery sightings and trust updates are
//! commands. Handshake tasks report back, link tasks forward frames, and the
//! manager alone decides which frames pass the gate. Connection and pairing
//! events are sent with backpressure; packet events use `try_send`. A dropped
//! packet is counted and logged with its device and type. Share requests travel
//! on a dedicated reliable channel; a full or closed share channel closes the
//! link instead of dropping the request.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_proto::{types, DeviceId, Identity, Packet, PairingRequest};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tracing::{debug, info, warn};

use crate::error::{NetError, Result, TlsError};
use crate::identity::{
    Fingerprint, LocalEndpoint, PeerCertificate, PeerTrust, TlsIdentity, TrustTable,
};
use crate::link::{
    accept_handshake, connect, Link, LinkLimits, LinkListener, LinkReader, LinkWriter,
};
use crate::policy::{AdmissionPermit, LinkPolicy};

/// How often the manager supervises idle links and peer expiry.
const TICK: Duration = Duration::from_secs(10);

/// How long shutdown waits for link tasks before aborting them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The outbound queue for one established link.
const LINK_OUTBOUND_QUEUE: usize = 64;

/// The manager command queue.
const COMMAND_QUEUE: usize = 256;

/// The manager message queue.
const MESSAGE_QUEUE: usize = 256;

/// The recommended bound for the reliable share event channel.
///
/// Share packets are never dropped silently. The caller creates the channel;
/// this bound matches the design's 32-slot share queue.
pub const SHARE_EVENT_QUEUE: usize = 32;

/// Whether an unpaired peer may send a packet type.
///
/// Protocol 8 allows only pairing packets before trust is established. The gate
/// and the tests share this one helper.
#[must_use]
pub fn is_unpaired_allowed(ptype: &str) -> bool {
    ptype == types::PAIR
}

fn is_share_packet(ptype: &str) -> bool {
    ptype == types::SHARE_REQUEST || ptype == types::SHARE_REQUEST_UPDATE
}

/// A handle for sending packets to one established link.
#[derive(Clone, Debug)]
pub struct LinkHandle {
    device_id: DeviceId,
    tx: mpsc::Sender<Packet>,
}

impl LinkHandle {
    /// Build a handle backed by a caller-owned channel. Test support only.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn from_channel(device_id: DeviceId, tx: mpsc::Sender<Packet>) -> Self {
        Self { device_id, tx }
    }

    /// The peer this handle writes to.
    #[must_use]
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Send one packet, waiting for room in the link queue.
    ///
    /// # Errors
    ///
    /// Fails when the link has closed.
    pub async fn send(&self, packet: Packet) -> Result<()> {
        self.tx.send(packet).await.map_err(|_| NetError::LinkClosed)
    }

    /// Send one packet without waiting.
    ///
    /// # Errors
    ///
    /// Fails when the link queue is full or the link has closed.
    pub fn try_send(&self, packet: Packet) -> Result<()> {
        self.tx.try_send(packet).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                NetError::Admission("link outbound queue full".to_string())
            }
            mpsc::error::TrySendError::Closed(_) => NetError::LinkClosed,
        })
    }
}

/// A share packet routed on the reliable channel.
///
/// Share requests must not travel on the lossy [`LinkEvent::Packet`] path. A
/// dropped share request leaves the peer waiting for a payload connection that
/// never comes.
#[derive(Debug)]
pub struct ShareEvent {
    /// Peer that sent the packet.
    pub device_id: DeviceId,
    /// The share request or update packet.
    pub packet: Packet,
}

/// One event from the manager to its caller.
#[derive(Debug)]
pub enum LinkEvent {
    /// A link completed authentication and was installed.
    Connected {
        /// Full authenticated identity.
        peer: Identity,
        /// Peer certificate.
        certificate: PeerCertificate,
        /// Remote socket address of the authenticated link.
        remote_addr: SocketAddr,
        /// Handle for sending packets to the peer.
        link: LinkHandle,
        /// Whether the trust table pins this certificate.
        paired: bool,
    },
    /// A packet from a paired peer passed the gate.
    Packet {
        /// Peer that sent the packet.
        device_id: DeviceId,
        /// The packet.
        packet: Packet,
    },
    /// A pairing packet from an authenticated peer.
    PairingRequest {
        /// Full authenticated identity.
        peer: Identity,
        /// Decoded pairing request.
        request: PairingRequest,
        /// Peer certificate.
        certificate: PeerCertificate,
        /// Handle for sending packets to the peer.
        link: LinkHandle,
        /// Whether the trust table pins this certificate.
        paired: bool,
    },
    /// An installed link closed.
    Disconnected {
        /// Peer whose link closed.
        device_id: DeviceId,
        /// Why the link closed.
        reason: String,
    },
}

/// An authenticated but unpaired link waiting for a pairing decision.
///
/// The daemon owns this value between [`LinkEvent::Connected`] with
/// `paired = false` and the pairing prompt. The manager does not queue packets
/// for an unpaired peer; the gate drops them.
#[derive(Debug)]
pub struct PendingLink {
    /// Authenticated peer identity.
    pub peer: Identity,
    /// Peer certificate.
    pub certificate: PeerCertificate,
    /// Remote socket address of the authenticated link.
    pub remote_addr: SocketAddr,
    /// Handle for sending packets to the peer.
    pub link: LinkHandle,
    /// Packets received before a session existed. The caller bounds the queue.
    pub queued: VecDeque<Packet>,
}

/// A handle for driving the link manager.
#[derive(Clone)]
pub struct LinkManagerHandle {
    commands: mpsc::Sender<ManagerCommand>,
    shutdown: watch::Sender<bool>,
}

impl LinkManagerHandle {
    /// Record a discovery sighting and dial the peer when it is not active.
    ///
    /// A full command queue drops the sighting; the next announcement retries.
    pub fn consider_peer(&self, id: DeviceId, addr: SocketAddr) {
        if let Err(error) = self
            .commands
            .try_send(ManagerCommand::ConsiderPeer { id, addr })
        {
            warn!(%error, "link manager command queue full; dropping a sighting");
        }
    }

    /// Set or clear a peer pin.
    ///
    /// A pin is accepted only when it matches the active link's certificate.
    /// Clearing a pin closes the active link. The call returns after the trust
    /// table update, so the gate is already open when the pairing unit sends
    /// its acceptance.
    ///
    /// # Errors
    ///
    /// Fails when the manager has stopped, no active link exists to pin, or the
    /// pin does not match the active certificate.
    pub async fn set_trust(&self, id: DeviceId, pin: Option<Fingerprint>) -> Result<()> {
        let (ack, response) = oneshot::channel();
        self.commands
            .send(ManagerCommand::SetTrust { id, pin, ack })
            .await
            .map_err(|_| NetError::LinkClosed)?;
        response.await.map_err(|_| NetError::LinkClosed)?
    }

    /// Close the device's installed link, whether paired or waiting for
    /// pairing.
    ///
    /// Used when a share packet cannot be delivered to the session. The link
    /// task emits [`LinkEvent::Disconnected`] with the close reason. An
    /// outbound dial still in handshake is aborted and its admission permit
    /// released. Closing a device with no installed link and no pending dial is
    /// a no-op.
    ///
    /// # Errors
    ///
    /// Fails when the manager has stopped.
    pub async fn close(&self, id: DeviceId) -> Result<()> {
        let (ack, response) = oneshot::channel();
        self.commands
            .send(ManagerCommand::Close { id, ack })
            .await
            .map_err(|_| NetError::LinkClosed)?;
        response.await.map_err(|_| NetError::LinkClosed)?
    }

    /// Stop the manager and close every link.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// The single-owner link manager.
pub struct LinkManager;

impl LinkManager {
    /// Start the manager with the default supervision tick.
    ///
    /// The manager owns `listener`, the trust snapshot and the admission
    /// policy. It emits link events on `events`, share packets on
    /// `share_events`, and returns a handle plus its task.
    #[must_use]
    pub fn start(
        listener: LinkListener,
        local: Arc<LocalEndpoint>,
        trust: TrustTable,
        limits: LinkLimits,
        events: mpsc::Sender<LinkEvent>,
        share_events: mpsc::Sender<ShareEvent>,
    ) -> (LinkManagerHandle, JoinHandle<Result<()>>) {
        Self::start_with_tick(listener, local, trust, limits, events, share_events, TICK)
    }

    /// Start the manager with an explicit supervision tick.
    ///
    /// The tick closes idle unpaired links and expires stale peer entries. A
    /// short tick keeps tests fast without changing the policy.
    #[must_use]
    pub fn start_with_tick(
        listener: LinkListener,
        local: Arc<LocalEndpoint>,
        trust: TrustTable,
        limits: LinkLimits,
        events: mpsc::Sender<LinkEvent>,
        share_events: mpsc::Sender<ShareEvent>,
        tick: Duration,
    ) -> (LinkManagerHandle, JoinHandle<Result<()>>) {
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let (messages, message_rx) = mpsc::channel(MESSAGE_QUEUE);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let policy = LinkPolicy::new(limits.clone());
        let pending_slots = Arc::new(Semaphore::new(limits.max_pending));
        let tls = Arc::clone(listener.tls());
        let state = ManagerState {
            tls,
            local,
            trust,
            limits,
            policy,
            events,
            share_events,
            messages,
            peers: HashMap::new(),
            active: HashMap::new(),
            pending_outbound: HashMap::new(),
            generation: 0,
            dial_generation: 0,
            pending_slots,
            dropped_packets: 0,
        };
        let task = tokio::spawn(async move {
            state
                .run(listener, command_rx, message_rx, shutdown_rx, tick)
                .await
        });
        (LinkManagerHandle { commands, shutdown }, task)
    }
}

enum ManagerCommand {
    ConsiderPeer {
        id: DeviceId,
        addr: SocketAddr,
    },
    SetTrust {
        id: DeviceId,
        pin: Option<Fingerprint>,
        ack: oneshot::Sender<Result<()>>,
    },
    Close {
        id: DeviceId,
        ack: oneshot::Sender<Result<()>>,
    },
}

enum ManagerMessage {
    HandshakeFinished {
        expected: Option<DeviceId>,
        dial: Option<u64>,
        result: Box<Result<Link>>,
    },
    Inbound {
        device_id: Arc<DeviceId>,
        generation: u64,
        packet: Packet,
    },
    LinkClosed {
        device_id: DeviceId,
        generation: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkControl {
    Open,
    Close(String),
}

struct ActiveLink {
    generation: u64,
    peer: Identity,
    certificate: PeerCertificate,
    link: LinkHandle,
    control: watch::Sender<LinkControl>,
    paired: bool,
    last_activity: Instant,
    permit: Option<AdmissionPermit>,
}

impl ActiveLink {
    fn close(&self, reason: &str) {
        let _ = self.control.send(LinkControl::Close(reason.to_string()));
    }

    fn release_unpaired(&mut self) {
        if let Some(permit) = self.permit.take() {
            drop(permit);
        }
    }
}

struct PeerState {
    addr: SocketAddr,
    last_seen: Instant,
    failures: u32,
    next_attempt: Instant,
}

struct PendingDial {
    token: u64,
    abort: AbortHandle,
}

struct ManagerState {
    tls: Arc<TlsIdentity>,
    local: Arc<LocalEndpoint>,
    trust: TrustTable,
    limits: LinkLimits,
    policy: LinkPolicy,
    events: mpsc::Sender<LinkEvent>,
    share_events: mpsc::Sender<ShareEvent>,
    messages: mpsc::Sender<ManagerMessage>,
    peers: HashMap<DeviceId, PeerState>,
    active: HashMap<DeviceId, ActiveLink>,
    pending_outbound: HashMap<DeviceId, PendingDial>,
    generation: u64,
    dial_generation: u64,
    pending_slots: Arc<Semaphore>,
    dropped_packets: u64,
}

impl ManagerState {
    async fn run(
        mut self,
        listener: LinkListener,
        mut commands: mpsc::Receiver<ManagerCommand>,
        mut messages: mpsc::Receiver<ManagerMessage>,
        mut shutdown: watch::Receiver<bool>,
        tick: Duration,
    ) -> Result<()> {
        let mut handshakes = JoinSet::new();
        let mut link_tasks = JoinSet::new();
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                accepted = listener.accept_tcp() => {
                    match accepted {
                        Ok(tcp) => self.spawn_inbound(tcp, &mut handshakes),
                        Err(NetError::Io(error))
                            if error.kind() == std::io::ErrorKind::ConnectionAborted => {}
                        Err(error) => return Err(error),
                    }
                }
                command = commands.recv() => {
                    match command {
                        Some(ManagerCommand::ConsiderPeer { id, addr }) => {
                            self.consider_peer(id, addr, &mut handshakes);
                        }
                        Some(ManagerCommand::SetTrust { id, pin, ack }) => {
                            let _ = ack.send(self.set_trust(id, pin));
                        }
                        Some(ManagerCommand::Close { id, ack }) => {
                            let _ = ack.send(self.close(id));
                        }
                        None => break,
                    }
                }
                message = messages.recv() => {
                    match message {
                        Some(ManagerMessage::HandshakeFinished { expected, dial, result }) => {
                            self.handshake_finished(expected, dial, *result, &mut link_tasks)
                                .await;
                        }
                        Some(ManagerMessage::Inbound { device_id, generation, packet }) => {
                            self.inbound(device_id, generation, packet).await;
                        }
                        Some(ManagerMessage::LinkClosed { device_id, generation, reason }) => {
                            self.link_closed(device_id, generation, reason).await;
                        }
                        None => break,
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                Some(_) = handshakes.join_next() => {}
                Some(_) = link_tasks.join_next() => {}
                _ = ticker.tick() => self.tick(),
            }
        }

        self.shutdown(&mut handshakes, &mut link_tasks).await;
        Ok(())
    }

    fn spawn_inbound(&self, tcp: tokio::net::TcpStream, handshakes: &mut JoinSet<()>) {
        let Ok(slot) = Arc::clone(&self.pending_slots).try_acquire_owned() else {
            debug!("pending handshake cap reached; closing the connection");
            return;
        };
        let tls = Arc::clone(&self.tls);
        let local = Arc::clone(&self.local);
        let trust = self.trust.clone();
        let limits = self.limits.clone();
        let policy = self.policy.clone();
        let messages = self.messages.clone();
        handshakes.spawn(async move {
            let _slot = slot;
            let result =
                accept_handshake(tcp, &tls, local.as_ref(), &trust, &limits, &policy).await;
            let _ = messages
                .send(ManagerMessage::HandshakeFinished {
                    expected: None,
                    dial: None,
                    result: Box::new(result),
                })
                .await;
        });
    }

    fn consider_peer(&mut self, id: DeviceId, addr: SocketAddr, handshakes: &mut JoinSet<()>) {
        if &id == self.local.device_id() {
            return;
        }
        if !self.remember_peer(id.clone(), addr) {
            return;
        }
        if self.active.contains_key(&id) || self.pending_outbound.contains_key(&id) {
            return;
        }
        let now = Instant::now();
        let Some(state) = self.peers.get(&id) else {
            return;
        };
        if state.next_attempt > now {
            return;
        }
        let addr = state.addr;
        let trust = self.trust.trust(&id);
        let permit = match self.policy.reserve_outgoing(&id, trust) {
            Ok(permit) => permit,
            Err(error) => {
                debug!(%error, %id, "outgoing dial skipped by admission");
                return;
            }
        };
        let local = Arc::clone(&self.local);
        let limits = self.limits.clone();
        let messages = self.messages.clone();
        let target = id.clone();
        let token = self.dial_generation;
        self.dial_generation = self.dial_generation.wrapping_add(1);
        let abort = handshakes.spawn(async move {
            let result = connect(addr, local.as_ref(), &target, trust, &limits, permit).await;
            let _ = messages
                .send(ManagerMessage::HandshakeFinished {
                    expected: Some(target),
                    dial: Some(token),
                    result: Box::new(result),
                })
                .await;
        });
        self.pending_outbound
            .insert(id, PendingDial { token, abort });
    }

    fn remember_peer(&mut self, id: DeviceId, addr: SocketAddr) -> bool {
        let now = Instant::now();
        if let Some(state) = self.peers.get_mut(&id) {
            state.addr = addr;
            state.last_seen = now;
            return true;
        }
        if self.peers.len() >= self.limits.max_known_peers {
            let victim = self
                .peers
                .iter()
                .filter(|(peer, _)| !self.active.contains_key(*peer))
                .min_by_key(|(_, state)| state.last_seen)
                .map(|(peer, _)| peer.clone());
            match victim {
                Some(victim) => {
                    self.peers.remove(&victim);
                }
                None => {
                    warn!(%id, "peer table is full of active links; ignoring the sighting");
                    return false;
                }
            }
        }
        self.peers.insert(
            id,
            PeerState {
                addr,
                last_seen: now,
                failures: 0,
                next_attempt: now,
            },
        );
        true
    }

    async fn handshake_finished(
        &mut self,
        expected: Option<DeviceId>,
        dial: Option<u64>,
        result: Result<Link>,
        link_tasks: &mut JoinSet<()>,
    ) {
        let closed_while_dialing = match (&expected, dial) {
            (Some(id), Some(token)) => {
                let current = self.pending_outbound.get(id).map(|pending| pending.token);
                if current == Some(token) {
                    self.pending_outbound.remove(id);
                    false
                } else {
                    true
                }
            }
            _ => false,
        };
        if closed_while_dialing {
            // `close` aborted this dial; a link that finished the race is dropped.
            return;
        }
        match result {
            Ok(link) => {
                if let Some(id) = &expected {
                    if let Some(state) = self.peers.get_mut(id) {
                        state.failures = 0;
                        state.next_attempt = Instant::now();
                        state.last_seen = Instant::now();
                    }
                }
                self.install(link, link_tasks).await;
            }
            Err(error) => {
                if let Some(id) = expected {
                    if let Some(state) = self.peers.get_mut(&id) {
                        state.failures = state.failures.saturating_add(1);
                        let exponent = state.failures.saturating_sub(1).min(4);
                        let backoff = Duration::from_secs(5 * 2u64.pow(exponent))
                            .min(Duration::from_secs(60));
                        state.next_attempt = Instant::now() + backoff;
                    }
                    debug!(%error, %id, "outgoing handshake failed");
                } else {
                    debug!(%error, "incoming handshake failed");
                }
            }
        }
    }

    async fn install(&mut self, link: Link, link_tasks: &mut JoinSet<()>) {
        let (peer, reader, writer, permit) = link.into_parts();
        let remote_addr = peer.remote_addr;
        let identity = peer.identity;
        let certificate = peer.certificate;
        let device_id = Arc::new(identity.device_id.clone());

        if let Some(existing) = self.active.get(device_id.as_ref()) {
            if existing.certificate.der() != certificate.der() {
                warn!(
                    %device_id,
                    "rejecting a duplicate link with a different certificate"
                );
                writer.close().await;
                return;
            }
        }

        let fingerprint = certificate.fingerprint();
        let paired = matches!(
            self.trust.trust(device_id.as_ref()),
            PeerTrust::Pinned(pin) if pin == fingerprint
        );
        let permit = if paired {
            if permit.is_unpaired() {
                drop(permit);
                None
            } else {
                Some(permit)
            }
        } else if permit.is_unpaired() {
            Some(permit)
        } else {
            // Trust was cleared while the handshake was in flight; the link is
            // unpaired now and must consume an unpaired slot.
            match self
                .policy
                .reserve_outgoing(device_id.as_ref(), PeerTrust::Unpaired)
            {
                Ok(permit) => Some(permit),
                Err(error) => {
                    warn!(
                        %error,
                        %device_id,
                        "trust changed during the handshake and no unpaired slot is free"
                    );
                    writer.close().await;
                    return;
                }
            }
        };

        if let Some(existing) = self.active.remove(device_id.as_ref()) {
            existing.close("replaced");
            self.emit(LinkEvent::Disconnected {
                device_id: (*device_id).clone(),
                reason: "replaced".to_string(),
            })
            .await;
        }

        let generation = self.generation;
        self.generation = self.generation.wrapping_add(1);

        let (outbound, outbound_rx) = mpsc::channel(LINK_OUTBOUND_QUEUE);
        let (control, control_rx) = watch::channel(LinkControl::Open);
        let handle = LinkHandle {
            device_id: (*device_id).clone(),
            tx: outbound,
        };
        self.active.insert(
            (*device_id).clone(),
            ActiveLink {
                generation,
                peer: identity.clone(),
                certificate: certificate.clone(),
                link: handle.clone(),
                control,
                paired,
                last_activity: Instant::now(),
                permit,
            },
        );
        self.emit(LinkEvent::Connected {
            peer: identity,
            certificate,
            remote_addr,
            link: handle,
            paired,
        })
        .await;

        let messages = self.messages.clone();
        link_tasks.spawn(run_link(
            Arc::clone(&device_id),
            generation,
            reader,
            writer,
            outbound_rx,
            control_rx,
            messages,
        ));
    }

    async fn inbound(&mut self, device_id: Arc<DeviceId>, generation: u64, packet: Packet) {
        enum GateAction {
            Pair(PairingRequest),
            Close(&'static str),
            Deliver,
            Share,
            Drop,
        }

        let action = {
            let Some(active) = self.active.get_mut(device_id.as_ref()) else {
                return;
            };
            if active.generation != generation {
                return;
            }
            active.last_activity = Instant::now();
            if is_unpaired_allowed(packet.ptype.as_str()) {
                match PairingRequest::from_packet(&packet) {
                    Ok(request) => GateAction::Pair(request),
                    Err(_) => GateAction::Close("malformed pairing packet"),
                }
            } else if packet.ptype == types::IDENTITY {
                GateAction::Close("identity packet after the handshake")
            } else if active.paired {
                if is_share_packet(packet.ptype.as_str()) {
                    GateAction::Share
                } else {
                    GateAction::Deliver
                }
            } else {
                GateAction::Drop
            }
        };

        match action {
            GateAction::Pair(request) => {
                let Some(active) = self.active.get(device_id.as_ref()) else {
                    return;
                };
                let paired = matches!(self.trust.trust(device_id.as_ref()), PeerTrust::Pinned(_));
                let event = LinkEvent::PairingRequest {
                    peer: active.peer.clone(),
                    request,
                    certificate: active.certificate.clone(),
                    link: active.link.clone(),
                    paired,
                };
                self.emit(event).await;
            }
            GateAction::Close(reason) => {
                warn!(%device_id, reason, "closing the link");
                self.close_active(device_id.as_ref(), reason);
            }
            GateAction::Deliver => {
                let event = LinkEvent::Packet {
                    device_id: (*device_id).clone(),
                    packet,
                };
                if let Err(error) = self.events.try_send(event) {
                    let full = matches!(&error, mpsc::error::TrySendError::Full(_));
                    // `try_send` hands the event back; only packet events use this path.
                    if let LinkEvent::Packet { device_id, packet } = error.into_inner() {
                        self.dropped_packets = self.dropped_packets.saturating_add(1);
                        warn!(
                            %device_id,
                            ptype = %packet.ptype,
                            dropped = self.dropped_packets,
                            full,
                            "dropping a packet from the lossy event channel"
                        );
                    }
                }
            }
            GateAction::Share => {
                let event = ShareEvent {
                    device_id: (*device_id).clone(),
                    packet,
                };
                match self.share_events.try_send(event) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!(%device_id, "closing the link: share event queue full");
                        self.close_active(device_id.as_ref(), "share event queue full");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        warn!(%device_id, "closing the link: share event receiver dropped");
                        self.close_active(device_id.as_ref(), "share event receiver dropped");
                    }
                }
            }
            GateAction::Drop => {
                debug!(%device_id, ptype = %packet.ptype, "dropping a packet from an unpaired peer");
            }
        }
    }

    fn close_active(&self, device_id: &DeviceId, reason: &str) {
        if let Some(active) = self.active.get(device_id) {
            active.close(reason);
        }
    }

    async fn link_closed(&mut self, device_id: DeviceId, generation: u64, reason: String) {
        let Some(active) = self.active.get(&device_id) else {
            return;
        };
        if active.generation != generation {
            return;
        }
        self.active.remove(&device_id);
        self.emit(LinkEvent::Disconnected { device_id, reason })
            .await;
    }

    fn set_trust(&mut self, id: DeviceId, pin: Option<Fingerprint>) -> Result<()> {
        match pin {
            Some(pin) => {
                let Some(active) = self.active.get(&id) else {
                    return Err(NetError::Admission("no active link to pin".to_string()));
                };
                if active.certificate.fingerprint() != pin {
                    return Err(NetError::Tls(TlsError::PinMismatch {
                        expected: pin,
                        got: active.certificate.fingerprint(),
                    }));
                }
                self.trust.set(id.clone(), pin);
                if let Some(active) = self.active.get_mut(&id) {
                    active.paired = true;
                    // The link is paired now; its unpaired slot is free.
                    active.release_unpaired();
                }
            }
            None => {
                self.trust.remove(&id);
                if let Some(active) = self.active.get_mut(&id) {
                    active.paired = false;
                    active.close("trust cleared");
                }
            }
        }
        Ok(())
    }

    fn close(&mut self, id: DeviceId) -> Result<()> {
        if let Some(dial) = self.pending_outbound.remove(&id) {
            dial.abort.abort();
        }
        self.close_active(&id, "closed by manager");
        Ok(())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let idle: Vec<DeviceId> = self
            .active
            .iter()
            .filter(|(_, active)| {
                !active.paired
                    && now.duration_since(active.last_activity) > self.limits.unpaired_idle_timeout
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in idle {
            if let Some(active) = self.active.get(&id) {
                info!(%id, "closing an idle unpaired link");
                active.close("unpaired idle timeout");
            }
        }

        let active = &self.active;
        let ttl = self.limits.peer_ttl;
        self.peers.retain(|id, state| {
            active.contains_key(id) || now.duration_since(state.last_seen) <= ttl
        });
    }

    async fn emit(&self, event: LinkEvent) {
        if self.events.send(event).await.is_err() {
            warn!("link event receiver dropped");
        }
    }

    async fn shutdown(mut self, handshakes: &mut JoinSet<()>, link_tasks: &mut JoinSet<()>) {
        handshakes.abort_all();
        while handshakes.join_next().await.is_some() {}
        for active in self.active.values() {
            active.close("manager shutting down");
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        while !link_tasks.is_empty() {
            match tokio::time::timeout_at(deadline, link_tasks.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    link_tasks.abort_all();
                    break;
                }
            }
        }
        while link_tasks.join_next().await.is_some() {}
        self.active.clear();
    }
}

async fn run_link(
    device_id: Arc<DeviceId>,
    generation: u64,
    mut reader: LinkReader,
    mut writer: LinkWriter,
    mut outbound: mpsc::Receiver<Packet>,
    mut control: watch::Receiver<LinkControl>,
    messages: mpsc::Sender<ManagerMessage>,
) {
    let mut outbound_open = true;
    let reason = loop {
        tokio::select! {
            packet = reader.recv() => {
                match packet {
                    Ok(Some(packet)) => {
                        let message = ManagerMessage::Inbound {
                            device_id: Arc::clone(&device_id),
                            generation,
                            packet,
                        };
                        if messages.send(message).await.is_err() {
                            break "manager closed".to_string();
                        }
                    }
                    Ok(None) => break "peer closed".to_string(),
                    Err(error) => break format!("read: {error}"),
                }
            }
            packet = outbound.recv(), if outbound_open => {
                match packet {
                    Some(packet) => {
                        if let Err(error) = writer.send(&packet).await {
                            break format!("write: {error}");
                        }
                    }
                    None => outbound_open = false,
                }
            }
            changed = control.changed() => {
                if changed.is_err() {
                    break "manager closed".to_string();
                }
                let state = control.borrow();
                if let LinkControl::Close(reason) = &*state {
                    break reason.clone();
                }
            }
        }
    };

    writer.close().await;
    let _ = messages
        .send(ManagerMessage::LinkClosed {
            device_id: (*device_id).clone(),
            generation,
            reason,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{Capabilities, DeviceName, DeviceType};

    #[test]
    fn only_pairing_packets_pass_the_unpaired_gate() {
        assert!(is_unpaired_allowed(types::PAIR));
        assert!(!is_unpaired_allowed(types::PING));
        assert!(!is_unpaired_allowed(types::IDENTITY));
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn pending_link_carries_the_remote_address() {
        let device_id = DeviceId::new("00112233445566778899aabbccddeeff").unwrap();
        let tls = TlsIdentity::generate(device_id.clone()).unwrap();
        let certificate = PeerCertificate::parse(tls.cert_der()).unwrap();
        let identity = Identity::new(
            device_id.clone(),
            DeviceName::new("pending link test"),
            DeviceType::Desktop,
            Capabilities::default(),
            Some(1716),
        );
        let (link, _outbound) = mpsc::channel(1);
        let remote_addr = SocketAddr::from(([192, 0, 2, 10], 1716));
        let pending = PendingLink {
            peer: identity,
            certificate,
            remote_addr,
            link: LinkHandle::from_channel(device_id, link),
            queued: VecDeque::new(),
        };

        assert_eq!(
            pending.remote_addr, remote_addr,
            "the daemon keeps the authenticated address until the pairing decision"
        );
    }

    #[tokio::test]
    async fn a_full_event_channel_counts_a_dropped_packet() {
        let device_id = DeviceId::new("00112233445566778899aabbccddeeff").unwrap();
        let tls = Arc::new(TlsIdentity::generate(device_id.clone()).unwrap());
        let certificate = PeerCertificate::parse(tls.cert_der()).unwrap();
        let identity = Identity::new(
            device_id.clone(),
            DeviceName::new("manager drop test"),
            DeviceType::Desktop,
            Capabilities::default(),
            Some(1716),
        );
        let local = Arc::new(LocalEndpoint::new(Arc::clone(&tls), identity.clone()).unwrap());
        let limits = LinkLimits::default();
        let (events, mut events_rx) = mpsc::channel(1);
        let (share_events, _share_events_rx) = mpsc::channel(1);
        let (messages, _messages_rx) = mpsc::channel(1);
        let (outbound, _outbound_rx) = mpsc::channel(1);
        let (control, _control_rx) = watch::channel(LinkControl::Open);
        let mut state = ManagerState {
            tls,
            local,
            trust: TrustTable::default(),
            limits: limits.clone(),
            policy: LinkPolicy::new(limits),
            events,
            share_events,
            messages,
            peers: HashMap::new(),
            active: HashMap::new(),
            pending_outbound: HashMap::new(),
            generation: 0,
            dial_generation: 0,
            pending_slots: Arc::new(Semaphore::new(1)),
            dropped_packets: 0,
        };
        state.active.insert(
            device_id.clone(),
            ActiveLink {
                generation: 0,
                peer: identity,
                certificate,
                link: LinkHandle {
                    device_id: device_id.clone(),
                    tx: outbound,
                },
                control,
                paired: true,
                last_activity: Instant::now(),
                permit: None,
            },
        );

        let packet = Packet::new(types::PING, serde_json::json!({ "message": "drop" }))
            .expect("build the test packet");
        state
            .events
            .try_send(LinkEvent::Packet {
                device_id: device_id.clone(),
                packet: packet.clone(),
            })
            .expect("the first packet fills the one-slot channel");

        state.inbound(Arc::new(device_id), 0, packet).await;

        assert_eq!(state.dropped_packets, 1, "the dropped packet is counted");
        assert!(
            matches!(events_rx.try_recv(), Ok(LinkEvent::Packet { .. })),
            "the queued packet is still the first one"
        );
    }

    #[test]
    fn peer_ttl_expires_an_idle_entry() {
        let device_id = DeviceId::new("00112233445566778899aabbccddeeff").unwrap();
        let tls = Arc::new(TlsIdentity::generate(device_id.clone()).unwrap());
        let local = Arc::new(
            LocalEndpoint::new(
                Arc::clone(&tls),
                Identity::new(
                    device_id.clone(),
                    DeviceName::new("manager test"),
                    DeviceType::Desktop,
                    Capabilities::default(),
                    Some(1716),
                ),
            )
            .unwrap(),
        );
        let limits = LinkLimits {
            peer_ttl: Duration::from_secs(10),
            ..LinkLimits::default()
        };

        let (events, _events_rx) = mpsc::channel(1);
        let (share_events, _share_events_rx) = mpsc::channel(1);
        let (messages, _messages_rx) = mpsc::channel(1);
        let mut state = ManagerState {
            tls,
            local,
            trust: TrustTable::default(),
            limits: limits.clone(),
            policy: LinkPolicy::new(limits),
            events,
            share_events,
            messages,
            peers: HashMap::new(),
            active: HashMap::new(),
            pending_outbound: HashMap::new(),
            generation: 0,
            dial_generation: 0,
            pending_slots: Arc::new(Semaphore::new(1)),
            dropped_packets: 0,
        };
        state.peers.insert(
            device_id,
            PeerState {
                addr: SocketAddr::from(([127, 0, 0, 1], 1716)),
                last_seen: Instant::now() - Duration::from_secs(11),
                failures: 0,
                next_attempt: Instant::now(),
            },
        );

        state.tick();

        assert!(
            state.peers.is_empty(),
            "a peer past its TTL is evicted on the tick"
        );
    }
}
