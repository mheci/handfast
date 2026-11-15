//! Daemon composition root.
//!
//! The daemon owns the store, the local TLS identity, the TCP listener, the
//! discovery socket, the link manager, one session per paired connected device,
//! the pairing prompt and shutdown ordering. The link layer owns admission and
//! the unpaired gate; the daemon never creates a `DeviceHost` for an unpaired
//! peer.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use handfast_desktop::{backends, default_incoming_dir, Desktop, Files, LocalFiles};
use handfast_net::{
    Discovery, Fingerprint, LinkEvent, LinkHandle, LinkLimits, LinkListener, LinkManager,
    LinkManagerHandle, LocalEndpoint, PayloadService, PeerCertificate, PendingLink, ShareEvent,
    TrustTable,
};
use handfast_plugins::registry;
use handfast_proto::{
    DeviceId, DeviceName, DeviceType, Identity, Packet, PairingRequest, MAX_TCP_PORT, MIN_TCP_PORT,
};
use handfast_runtime::{PluginEvent, Registry};
use handfast_store::{Store, StoreError};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::control::{
    ControlResponse, ControlServer, DaemonCommand, DaemonStatus, DeviceSummary, IdentitySummary,
    PairedDevice, TransferSummary, MAX_STATUS_DEVICES,
};
use crate::default_data_dir;
use crate::identity;
use crate::limits::{ShareLimits, STALE_TEMP_AGE};
use crate::pairing;
use crate::redact;
use crate::session::{DaemonEvent, DeviceSession, SessionLimits, TransferSetup};

/// How often to announce this device over UDP discovery.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5);

/// How often discovery may rewrite a peer record.
const DISCOVERY_WRITE_INTERVAL: Duration = Duration::from_secs(60);

const DISCOVERY_WRITE_PEERS: usize = 1024;

/// The link event channel. Connection and pairing events use `send().await`, so
/// the daemon must drain this queue without stalling on a prompt or a session.
const LINK_EVENT_QUEUE: usize = 256;

/// Pending packets held for a link that is still pairing.
const PENDING_QUEUE: usize = 32;

/// The daemon event channel from sessions and pairing tasks.
const DAEMON_EVENT_QUEUE: usize = 256;

/// The local control command channel.
const COMMAND_QUEUE: usize = 16;

/// How long shutdown waits for tasks before aborting them.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The default KDE Connect TCP port.
pub const DEFAULT_TCP_PORT: u16 = 1716;

#[derive(Debug, Parser)]
pub struct DaemonArgs {
    /// State directory. Defaults to the platform data directory.
    ///
    /// The top-level `--data-dir` option fills this; it is not a daemon flag.
    #[arg(skip)]
    pub data_dir: Option<PathBuf>,

    /// Incoming directory for received files. Defaults to the platform
    /// Downloads path.
    #[arg(long)]
    pub incoming_dir: Option<PathBuf>,

    /// TCP control port, in the KDE Connect range 1716-1764.
    #[arg(long, default_value_t = DEFAULT_TCP_PORT, value_parser = parse_tcp_port)]
    pub tcp_port: u16,

    /// UDP discovery port.
    #[arg(long, default_value_t = handfast_net::DISCOVERY_PORT)]
    pub discovery_port: u16,

    /// Write daemon diagnostics to this file. The log file rotates when it
    /// reaches the size limit.
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    /// Print this device's identity packet and exit.
    #[arg(long)]
    pub print_identity: bool,
}

impl Default for DaemonArgs {
    fn default() -> Self {
        Self {
            data_dir: None,
            incoming_dir: None,
            tcp_port: DEFAULT_TCP_PORT,
            discovery_port: handfast_net::DISCOVERY_PORT,
            log_file: None,
            print_identity: false,
        }
    }
}

/// Run the daemon until `shutdown` completes.
///
/// The caller installs the tracing subscriber.
///
/// # Errors
///
/// Fails when the state directory, store, TLS identity, listener, discovery
/// socket or control server cannot be opened, or when a startup identity check
/// fails. Every error is `Send + Sync`, so a caller may spawn this future.
pub async fn run(
    args: DaemonArgs,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let started_at = Instant::now();
    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    identity::ensure_owned_state_path(&data_dir)?;
    std::fs::create_dir_all(&data_dir)?;
    identity::secure_state_dir(&data_dir)?;
    let state_path = data_dir.join("state.db");
    identity::ensure_owned_state_path(&state_path)?;
    let store = Arc::new(Store::open(&state_path)?);
    identity::secure_state_file(&state_path)?;
    identity::ensure_owned_state_path(&state_path)?;
    let device_id = identity::load_or_create_device_id(&store)?;

    let desktop = Desktop::default();
    let registry = Arc::new(registry());
    let capabilities = registry.capabilities(&desktop)?;
    let name = device_name();

    if args.print_identity {
        let identity = Identity::new(
            device_id,
            DeviceName::new(&name),
            DeviceType::Desktop,
            capabilities,
            Some(args.tcp_port),
        );
        println!("{}", serde_json::to_string_pretty(&identity.to_packet()?)?);
        return Ok(());
    }

    // The listener is bound before discovery announces, so the announced port
    // is always one this process owns.
    let tls = Arc::new(identity::load_or_generate_tls_identity(&store, &device_id)?);
    let fingerprint = tls.fingerprint().to_hex();
    tracing::info!(fingerprint = %fingerprint, "TLS identity loaded");
    let listener = LinkListener::bind_in_range(args.tcp_port, Arc::clone(&tls)).await?;
    let tcp_port = listener.local_addr()?.port();
    if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&tcp_port) {
        return Err(format!(
            "TCP listener bound outside {MIN_TCP_PORT}-{MAX_TCP_PORT}: {tcp_port}"
        )
        .into());
    }
    tracing::info!(tcp_port, "TCP listener bound");

    // The share path is composed before the identity is built, so the running
    // daemon advertises share. The print branch above keeps the default desktop
    // and creates nothing on disk.
    let limits = ShareLimits::default();
    let incoming_dir = args.incoming_dir.unwrap_or_else(default_incoming_dir);
    let files = Arc::new(LocalFiles::new(&incoming_dir, limits.files())?);
    let swept = files.sweep_stale(STALE_TEMP_AGE).await?;
    tracing::info!(
        removed = swept.removed,
        dir = %incoming_dir.display(),
        "incoming directory ready"
    );
    let desktop = Desktop {
        files: Some(Arc::clone(&files) as Arc<dyn Files>),
        ..Desktop::default()
    };
    let desktop = install_backends(
        desktop,
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        backends::program_available,
    );
    let capabilities = registry.capabilities(&desktop)?;
    tracing::info!(
        capabilities = ?desktop.capabilities(),
        "desktop backends installed"
    );

    // One TLS identity serves the control links and the payload connections.
    let payloads = Arc::new(PayloadService::new(Arc::clone(&tls), limits.payloads()));

    let identity = Identity::new(
        device_id,
        DeviceName::new(&name),
        DeviceType::Desktop,
        capabilities,
        Some(tcp_port),
    );
    let identity_summary = IdentitySummary {
        device_id: identity.device_id.clone(),
        device_name: identity.device_name.as_str().to_string(),
        device_type: identity.device_type.as_str().to_string(),
        fingerprint,
    };
    let local = Arc::new(LocalEndpoint::new(tls, identity.clone())?);

    let mut trust = TrustTable::default();
    for (id, pin) in store.trust_table()? {
        trust.set(DeviceId::new(id)?, Fingerprint::from_bytes(pin));
    }
    let (link_tx, mut link_rx) = mpsc::channel(LINK_EVENT_QUEUE);
    let (share_tx, mut share_rx) = mpsc::channel(limits.share_event_queue);
    let (links, manager_task) = LinkManager::start(
        listener,
        local,
        trust,
        LinkLimits::default(),
        link_tx,
        share_tx,
    );

    let local_device_id = identity.device_id.clone();
    let discovery = Discovery::bind(identity, args.discovery_port, tcp_port).await?;
    let discovery_addr = discovery.local_addr()?;
    tracing::info!(address = %discovery_addr, "discovery listening");

    let (daemon_tx, mut daemon_rx) = mpsc::channel(DAEMON_EVENT_QUEUE);
    let (prompt_tx, prompt_task) =
        pairing::spawn(Arc::clone(&store), links.clone(), daemon_tx.clone());
    let (discovery_stop, mut discovery_stop_rx) = watch::channel(false);
    let discovery_store = Arc::clone(&store);
    let discovery_links = links.clone();
    let discovery_task = tokio::spawn(async move {
        let mut announce = tokio::time::interval(ANNOUNCE_INTERVAL);
        let mut writes = DiscoveryWrites::default();
        loop {
            tokio::select! {
                _ = announce.tick() => {
                    if should_announce(&discovery_links) {
                        if let Err(error) = discovery.announce_broadcast().await {
                            tracing::warn!(%error, "discovery announcement failed");
                        }
                    }
                }
                received = discovery.recv() => {
                    let (address, announcement) = match received {
                        Ok(received) => received,
                        Err(error) => {
                            tracing::error!(%error, "discovery socket failed; stopping discovery");
                            break;
                        }
                    };
                    if announcement.device_id == local_device_id {
                        continue;
                    }
                    tracing::debug!(
                        %address,
                        device_id = %announcement.device_id,
                        tcp_port = announcement.tcp_port,
                        "discovered device"
                    );
                    discovery_links.consider_peer(
                        announcement.device_id.clone(),
                        SocketAddr::new(address.ip(), announcement.tcp_port),
                    );
                    let now = Instant::now();
                    if !writes.should_write(&announcement.device_id, now) {
                        continue;
                    }
                    let id = announcement.device_id.as_str().to_string();
                    let name = announcement.device_name.as_str().to_string();
                    let device_type = announcement.device_type.as_str().to_string();
                    let last_seen = unix_now();
                    if let Err(error) = Arc::clone(&discovery_store)
                        .run_blocking(move |store| {
                            store.record_discovery(&id, &name, &device_type, last_seen)
                        })
                        .await
                    {
                        tracing::warn!(%error, "failed to persist discovered device");
                    } else {
                        writes.record_written(&announcement.device_id, now);
                    }
                }
                changed = discovery_stop_rx.changed() => {
                    if changed.is_err() || *discovery_stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
    });

    let (command_tx, mut command_rx) = mpsc::channel(COMMAND_QUEUE);
    let control = ControlServer::start(&data_dir, command_tx).await?;

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let mut daemon = Daemon {
        store,
        registry,
        desktop,
        links,
        payloads,
        limits,
        events: daemon_tx,
        sessions: HashMap::new(),
        pending: HashMap::new(),
        prompts: HashMap::new(),
        next_prompt: 0,
        prompt_tx,
        prompt_task,
        started_at,
        identity: identity_summary,
        incoming_dir: incoming_dir.clone(),
        connected: HashMap::new(),
        transfers: HashMap::new(),
        shutdown: Some(shutdown_tx),
    };

    tokio::pin!(shutdown);
    let mut command_open = true;
    loop {
        // Link events are handled before share events so a Connected event
        // creates the session before a share packet for the same link arrives.
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("shutting down");
                break;
            }
            _ = &mut shutdown_rx => {
                tracing::info!("shutting down on a control request");
                break;
            }
            event = link_rx.recv() => {
                match event {
                    Some(event) => daemon.handle_link_event(event).await,
                    None => {
                        tracing::warn!("link manager stopped");
                        break;
                    }
                }
            }
            event = share_rx.recv() => {
                match event {
                    Some(event) => daemon.handle_share(event).await,
                    None => {
                        tracing::warn!("share channel stopped");
                        break;
                    }
                }
            }
            event = daemon_rx.recv() => {
                match event {
                    Some(event) => daemon.handle_daemon_event(event).await,
                    None => break,
                }
            }
            command = command_rx.recv(), if command_open => {
                match command {
                    Some(command) => daemon.handle_command(command).await,
                    None => {
                        tracing::warn!("control command channel stopped");
                        command_open = false;
                    }
                }
            }
        }
    }

    // Dropping the receivers before shutdown keeps a full event queue from
    // stalling the manager while it closes links. The control server stops
    // accepting before sessions stop, so no command starts mid-shutdown.
    drop(share_rx);
    drop(command_rx);
    drop(link_rx);
    drop(daemon_rx);
    control.shutdown().await;
    daemon
        .shutdown(discovery_stop, discovery_task, manager_task)
        .await;
    Ok(())
}

struct Daemon {
    store: Arc<Store>,
    registry: Arc<Registry>,
    desktop: Desktop,
    links: LinkManagerHandle,
    payloads: Arc<PayloadService>,
    limits: ShareLimits,
    events: mpsc::Sender<DaemonEvent>,
    sessions: HashMap<DeviceId, DeviceSession>,
    pending: HashMap<DeviceId, PendingLink>,
    prompts: HashMap<DeviceId, (u64, oneshot::Sender<()>)>,
    next_prompt: u64,
    prompt_tx: mpsc::Sender<pairing::PromptRequest>,
    prompt_task: JoinHandle<()>,
    started_at: Instant,
    identity: IdentitySummary,
    incoming_dir: PathBuf,
    connected: HashMap<DeviceId, DeviceSummary>,
    transfers: HashMap<(DeviceId, u64), TransferSummary>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Daemon {
    /// Start a session with the transfer runner when the desktop has a Files
    /// adapter, and without one otherwise.
    fn start_session(
        &self,
        peer: Identity,
        link: LinkHandle,
        remote_addr: SocketAddr,
        certificate: PeerCertificate,
    ) -> DeviceSession {
        let session_limits = SessionLimits {
            delivery_timeout: self.limits.share_delivery_timeout,
            max_queued_receives: self.limits.max_queued_receives,
        };
        if self.desktop.files.is_some() {
            DeviceSession::start_with_transfers(
                peer,
                link,
                Arc::clone(&self.registry),
                self.desktop.clone(),
                self.events.clone(),
                TransferSetup {
                    remote_addr,
                    certificate,
                    payloads: Arc::clone(&self.payloads),
                    limits: session_limits,
                },
            )
        } else {
            DeviceSession::start(
                peer,
                link,
                Arc::clone(&self.registry),
                self.desktop.clone(),
                self.events.clone(),
                session_limits,
            )
        }
    }

    /// Queue one packet on a pending link and report whether one exists.
    fn queue_pending(&mut self, device_id: &DeviceId, packet: Packet) -> bool {
        let Some(link) = self.pending.get_mut(device_id) else {
            return false;
        };
        if link.queued.len() < PENDING_QUEUE {
            link.queued.push_back(packet);
        } else {
            tracing::debug!(%device_id, "dropping a packet for a full pending queue");
        }
        true
    }

    async fn handle_link_event(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Connected {
                peer,
                certificate,
                remote_addr,
                link,
                paired,
            } => {
                let device_id = peer.device_id.clone();
                let id = device_id.as_str().to_string();
                let name = peer.device_name.as_str().to_string();
                let device_type = peer.device_type.as_str().to_string();
                self.connected.insert(
                    device_id.clone(),
                    DeviceSummary {
                        device_id: device_id.clone(),
                        device_name: name.clone(),
                        device_type: device_type.clone(),
                    },
                );
                if let Err(error) = Arc::clone(&self.store)
                    .run_blocking(move |store| {
                        store.record_authenticated_identity(&id, &name, &device_type)
                    })
                    .await
                {
                    tracing::warn!(
                        %error,
                        %device_id,
                        "failed to persist authenticated identity"
                    );
                }

                if paired {
                    self.pending.remove(&device_id);
                    if let Some(previous) = self.sessions.remove(&device_id) {
                        previous.handle().stop("replaced".to_string());
                    }
                    let session = self.start_session(peer, link, remote_addr, certificate);
                    tracing::info!(%device_id, %remote_addr, "paired link connected");
                    self.sessions.insert(device_id, session);
                } else {
                    self.pending.insert(
                        device_id.clone(),
                        PendingLink {
                            peer,
                            certificate,
                            remote_addr,
                            link,
                            queued: VecDeque::new(),
                        },
                    );
                    tracing::info!(
                        %device_id,
                        %remote_addr,
                        "unpaired link authenticated; waiting for a pairing request"
                    );
                }
            }
            LinkEvent::Packet { device_id, packet } => {
                if let Some(session) = self.sessions.get(&device_id) {
                    session.deliver(packet);
                } else if !self.queue_pending(&device_id, packet) {
                    tracing::debug!(%device_id, "packet for an unknown device");
                }
            }
            LinkEvent::PairingRequest {
                peer,
                request,
                link,
                paired,
                ..
            } => {
                self.handle_pairing_request(peer, request, link, paired)
                    .await;
            }
            LinkEvent::Disconnected { device_id, reason } => {
                self.connected.remove(&device_id);
                if let Some(session) = self.sessions.remove(&device_id) {
                    session.handle().stop(reason.clone());
                }
                self.pending.remove(&device_id);
                self.cancel_prompt(&device_id);
                tracing::info!(%device_id, %reason, "device link closed");
            }
        }
    }

    async fn handle_pairing_request(
        &mut self,
        peer: Identity,
        request: PairingRequest,
        link: handfast_net::LinkHandle,
        paired: bool,
    ) {
        let device_id = peer.device_id.clone();
        if paired {
            if request.pair {
                tracing::debug!(%device_id, "ignoring a pairing request from a paired device");
            } else {
                // Clear durable trust before the reject so a crash cannot leave
                // the pin. The manager pin is cleared after the reject is sent,
                // because clearing it closes the link.
                let id = device_id.as_str().to_string();
                if let Err(error) = Arc::clone(&self.store)
                    .run_blocking(move |store| store.set_trusted(&id, None))
                    .await
                {
                    tracing::warn!(%error, %device_id, "failed to clear the stored pin");
                }
                if let Ok(packet) = PairingRequest::reject().to_packet() {
                    if let Err(error) = link.send(packet).await {
                        tracing::debug!(%error, %device_id, "failed to send the unpair rejection");
                    }
                }
                self.unpair(device_id).await;
            }
            return;
        }

        if self.prompts.contains_key(&device_id) {
            tracing::debug!(%device_id, "pairing prompt already open");
            return;
        }
        let (peer, certificate, link) = {
            let Some(pending) = self.pending.get(&device_id) else {
                tracing::warn!(%device_id, "pairing request without a pending link");
                return;
            };
            (
                pending.peer.clone(),
                pending.certificate.clone(),
                pending.link.clone(),
            )
        };

        let token = self.next_prompt;
        self.next_prompt = self.next_prompt.wrapping_add(1);
        let (cancel, cancel_rx) = oneshot::channel();
        self.prompts.insert(device_id.clone(), (token, cancel));
        if let Err(error) = self.prompt_tx.try_send(pairing::PromptRequest {
            peer,
            certificate,
            link,
            token,
            cancel: cancel_rx,
        }) {
            self.prompts.remove(&device_id);
            tracing::warn!(?error, %device_id, "pairing prompt queue is full");
        }
    }

    /// Wake a prompt whose link has closed and drop its generation.
    fn cancel_prompt(&mut self, device_id: &DeviceId) {
        if let Some((_, cancel)) = self.prompts.remove(device_id) {
            let _ = cancel.send(());
        }
    }

    async fn unpair(&mut self, device_id: DeviceId) {
        let id = device_id.as_str().to_string();
        if let Err(error) = Arc::clone(&self.store)
            .run_blocking(move |store| store.set_trusted(&id, None))
            .await
        {
            tracing::warn!(%error, %device_id, "failed to clear the stored pin");
        }
        if let Err(error) = self.links.set_trust(device_id.clone(), None).await {
            tracing::warn!(%error, %device_id, "failed to clear the manager pin");
        }
        if let Some(session) = self.sessions.remove(&device_id) {
            session.handle().stop("unpaired".to_string());
        }
        self.pending.remove(&device_id);
        self.cancel_prompt(&device_id);
    }

    async fn handle_daemon_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::PluginEvents { device_id, events } => {
                self.record_transfers(&device_id, &events);
                redact::log_plugin_events(&device_id, &events);
            }
            DaemonEvent::PairingAccepted { device_id, token } => {
                if !self.take_prompt(&device_id, token) {
                    return;
                }
                let Some(pending) = self.pending.remove(&device_id) else {
                    tracing::warn!(%device_id, "pairing accepted without a pending link");
                    return;
                };
                if let Some(previous) = self.sessions.remove(&device_id) {
                    previous.handle().stop("replaced".to_string());
                }
                let session = self.start_session(
                    pending.peer,
                    pending.link,
                    pending.remote_addr,
                    pending.certificate,
                );
                for packet in pending.queued {
                    session.deliver(packet);
                }
                self.sessions.insert(device_id.clone(), session);
                tracing::info!(%device_id, "pairing accepted");
            }
            DaemonEvent::PairingRejected { device_id, token } => {
                if self.take_prompt(&device_id, token) {
                    tracing::info!(%device_id, "pairing rejected");
                }
            }
        }
    }

    /// Route one reliable share packet to its session.
    ///
    /// A share packet is either queued for the plugin or the link closes with
    /// an observable failure. A session that cannot take the packet is stopped
    /// and removed, and a `ShareFailed` with id 0 is emitted. A share for a
    /// link that is still pairing waits on the pending link and is delivered
    /// when `PairingAccepted` creates the session.
    async fn handle_share(&mut self, event: ShareEvent) {
        let ShareEvent { device_id, packet } = event;
        let handle = self.sessions.get(&device_id).map(DeviceSession::handle);
        let Some(handle) = handle else {
            if self.queue_pending(&device_id, packet) {
                return;
            }
            tracing::warn!(%device_id, "share packet without a session; closing the link");
            if let Err(error) = self.links.close(device_id.clone()).await {
                tracing::warn!(%error, %device_id, "failed to close the link");
            }
            return;
        };

        if let Err(error) = handle.deliver_share(packet).await {
            tracing::warn!(%error, %device_id, "share delivery failed; closing the link");
            if let Some(session) = self.sessions.remove(&device_id) {
                session.handle().stop("share delivery failed".to_string());
            }
            if let Err(close_error) = self.links.close(device_id.clone()).await {
                tracing::warn!(%close_error, %device_id, "failed to close the link");
            }
            let failed = DaemonEvent::PluginEvents {
                device_id,
                events: vec![PluginEvent::ShareFailed {
                    transfer_id: 0,
                    reason: error.to_string(),
                }],
            };
            let events = self.events.clone();
            tokio::spawn(async move {
                if events.send(failed).await.is_err() {
                    tracing::warn!("daemon event channel closed; share failure event dropped");
                }
            });
        }
    }

    fn record_transfers(&mut self, device_id: &DeviceId, events: &[PluginEvent]) {
        for event in events {
            match event {
                PluginEvent::ShareProgress {
                    transfer_id,
                    bytes,
                    total,
                } => {
                    self.transfers.insert(
                        (device_id.clone(), *transfer_id),
                        TransferSummary {
                            device_id: device_id.clone(),
                            transfer_id: *transfer_id,
                            bytes: *bytes,
                            total: *total,
                        },
                    );
                }
                PluginEvent::ShareCompleted { transfer_id, .. }
                | PluginEvent::ShareFailed { transfer_id, .. } => {
                    self.transfers.remove(&(device_id.clone(), *transfer_id));
                }
                _ => {}
            }
        }
    }

    async fn status(&self) -> Result<DaemonStatus, StoreError> {
        let records = Arc::clone(&self.store)
            .run_blocking(|store| {
                let mut records = Vec::new();
                for (id, _pin) in store.trust_table()? {
                    if let Some(record) = store.device(&id)? {
                        records.push(record);
                    }
                }
                Ok(records)
            })
            .await?;

        let mut paired = Vec::with_capacity(records.len());
        for record in records {
            let device_id =
                DeviceId::new(&record.id).map_err(|error| StoreError::Data(error.to_string()))?;
            paired.push(PairedDevice {
                connected: self.connected.contains_key(&device_id),
                device_id,
                device_name: record.name,
                device_type: record.device_type,
            });
        }
        paired.sort_by(|left, right| left.device_id.as_str().cmp(right.device_id.as_str()));

        let mut connected: Vec<DeviceSummary> = self.connected.values().cloned().collect();
        connected.sort_by(|left, right| left.device_id.as_str().cmp(right.device_id.as_str()));

        let mut transfers: Vec<TransferSummary> = self.transfers.values().cloned().collect();
        transfers.sort_by(|left, right| {
            left.device_id
                .as_str()
                .cmp(right.device_id.as_str())
                .then(left.transfer_id.cmp(&right.transfer_id))
        });

        let truncated = connected.len() > MAX_STATUS_DEVICES || paired.len() > MAX_STATUS_DEVICES;
        connected.truncate(MAX_STATUS_DEVICES);
        paired.truncate(MAX_STATUS_DEVICES);

        Ok(DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            identity: self.identity.clone(),
            incoming_dir: self.incoming_dir.clone(),
            connected,
            paired,
            transfers,
            truncated,
        })
    }

    async fn handle_command(&mut self, command: DaemonCommand) {
        match command {
            DaemonCommand::Share {
                device_id,
                path,
                reply,
            } => {
                let Some(session) = self.sessions.get(&device_id) else {
                    let _ = reply.send(ControlResponse::Err {
                        message: format!("device {device_id} is not connected"),
                    });
                    return;
                };
                let handle = session.handle();
                tokio::spawn(async move {
                    let response = match handle.share_file(path).await {
                        Ok(outcome) => ControlResponse::Ok {
                            bytes: outcome.bytes,
                        },
                        Err(error) => ControlResponse::Err {
                            message: error.to_string(),
                        },
                    };
                    let _ = reply.send(response);
                });
            }
            DaemonCommand::Status { reply } => {
                let response = match self.status().await {
                    Ok(status) => ControlResponse::Status {
                        status: Box::new(status),
                    },
                    Err(error) => ControlResponse::Err {
                        message: error.to_string(),
                    },
                };
                let _ = reply.send(response);
            }
            DaemonCommand::Shutdown { reply } => {
                let _ = reply.send(ControlResponse::ShuttingDown);
                if let Some(shutdown) = self.shutdown.take() {
                    let _ = shutdown.send(());
                }
            }
        }
    }

    /// Remove the prompt handle only when the outcome belongs to the current
    /// prompt. A stale outcome must not clear a newer prompt.
    fn take_prompt(&mut self, device_id: &DeviceId, token: u64) -> bool {
        let Some((current, _)) = self.prompts.get(device_id) else {
            return false;
        };
        if *current != token {
            tracing::debug!(%device_id, "ignoring a stale pairing outcome");
            return false;
        }
        self.prompts.remove(device_id);
        true
    }

    async fn shutdown(
        self,
        discovery_stop: watch::Sender<bool>,
        discovery_task: JoinHandle<()>,
        manager_task: JoinHandle<handfast_net::Result<()>>,
    ) {
        let Daemon {
            links,
            sessions,
            prompts,
            prompt_task,
            mut pending,
            ..
        } = self;
        let _ = discovery_stop.send(true);
        links.shutdown();

        let mut session_tasks = Vec::new();
        for (_, session) in sessions {
            session.handle().stop("daemon shutting down".to_string());
            session_tasks.push(session.into_task());
        }
        for (_, (_, cancel)) in prompts {
            let _ = cancel.send(());
        }
        prompt_task.abort();
        pending.clear();

        let mut discovery_task = discovery_task;
        let mut manager_task = manager_task;
        let joined = tokio::time::timeout(SHUTDOWN_GRACE, async {
            let _ = (&mut discovery_task).await;
            let manager_result = (&mut manager_task).await;
            for task in &mut session_tasks {
                let _ = task.await;
            }
            manager_result
        })
        .await;

        match joined {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                tracing::warn!(%error, "link manager stopped with an error");
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "link manager task failed");
            }
            Err(_) => {
                tracing::warn!("shutdown timed out; aborting the remaining tasks");
                discovery_task.abort();
                manager_task.abort();
                for task in session_tasks {
                    task.abort();
                }
            }
        }
    }
}

/// Parse and range-check a KDE Connect TCP port.
///
/// # Errors
///
/// Fails when the value is not a number or outside the KDE Connect TCP range.
pub fn parse_tcp_port(value: &str) -> Result<u16, String> {
    let port: u16 = value
        .parse()
        .map_err(|_| format!("TCP port {value} is not a number"))?;
    if (MIN_TCP_PORT..=MAX_TCP_PORT).contains(&port) {
        Ok(port)
    } else {
        Err(format!(
            "TCP port {port} is outside {MIN_TCP_PORT}-{MAX_TCP_PORT}"
        ))
    }
}

/// The last persisted discovery sighting per peer. Held in memory so the
/// throttle costs no store reads.
#[derive(Default)]
struct DiscoveryWrites {
    peers: HashMap<DeviceId, DiscoverySighting>,
}

struct DiscoverySighting {
    written_at: Instant,
}

impl DiscoveryWrites {
    fn should_write(&self, device_id: &DeviceId, now: Instant) -> bool {
        match self.peers.get(device_id) {
            Some(sighting) => {
                now.saturating_duration_since(sighting.written_at) >= DISCOVERY_WRITE_INTERVAL
            }
            None => {
                self.peers.len() < DISCOVERY_WRITE_PEERS
                    || self.peers.values().any(|sighting| {
                        now.saturating_duration_since(sighting.written_at)
                            >= DISCOVERY_WRITE_INTERVAL
                    })
            }
        }
    }

    fn record_written(&mut self, device_id: &DeviceId, now: Instant) {
        if !self.peers.contains_key(device_id) && self.peers.len() >= DISCOVERY_WRITE_PEERS {
            let oldest = self
                .peers
                .iter()
                .min_by_key(|(_, sighting)| sighting.written_at)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                self.peers.remove(&oldest);
            }
        }
        self.peers
            .insert(device_id.clone(), DiscoverySighting { written_at: now });
    }
}

fn should_announce(links: &LinkManagerHandle) -> bool {
    links.active_links() == 0
}

fn unix_now() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
}

fn device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "Handfast desktop".to_string())
}

fn install_backends(
    mut desktop: Desktop,
    wayland: bool,
    available: impl Fn(&str) -> bool,
) -> Desktop {
    let clipboard_available = if wayland {
        available("wl-paste") && available("wl-copy")
    } else {
        available("xclip")
    };
    if clipboard_available {
        desktop.clipboard = Some(backends::clipboard());
    }
    if available("notify-send") {
        desktop.notifier = Some(backends::notifier());
    }
    if available("playerctl") {
        desktop.media = Some(backends::media());
    }
    if available("wpctl") {
        desktop.volume = Some(backends::volume());
    }
    if available("xdg-open") {
        desktop.shell = Some(backends::shell());
    }
    if available("systemd-inhibit") {
        desktop.screensaver = Some(backends::screensaver());
    }
    if available("loginctl") {
        desktop.lock = Some(backends::lock());
    }
    desktop
}

#[cfg(test)]
mod tests {
    use handfast_desktop::DesktopCapabilities;
    use handfast_proto::types;

    use super::*;

    fn share_text_wire(text: &str) -> handfast_testkit::WirePacket {
        handfast_testkit::WirePacket {
            id: 0,
            ptype: types::SHARE_REQUEST.to_string(),
            body: serde_json::json!({ "text": text })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    #[test]
    fn available_tools_select_their_backends() {
        let none = install_backends(Desktop::default(), false, |_| false);
        assert_eq!(none.capabilities(), DesktopCapabilities::default());

        let all = install_backends(Desktop::default(), false, |_| true);
        assert_eq!(
            all.capabilities(),
            DesktopCapabilities {
                clipboard: true,
                notifications: true,
                media: true,
                volume: true,
                input: false,
                screensaver: true,
                lock: true,
                shell: true,
                files: false,
            }
        );

        let wayland = install_backends(Desktop::default(), true, |program| {
            program == "wl-paste" || program == "wl-copy"
        });
        assert!(wayland.capabilities().clipboard);

        let missing_wl_copy =
            install_backends(Desktop::default(), true, |program| program == "wl-paste");
        assert!(!missing_wl_copy.capabilities().clipboard);

        let x11 = install_backends(Desktop::default(), false, |program| program == "xclip");
        assert!(x11.capabilities().clipboard);

        let xclip_on_wayland =
            install_backends(Desktop::default(), true, |program| program == "xclip");
        assert!(!xclip_on_wayland.capabilities().clipboard);
    }

    #[test]
    fn capabilities_follow_the_installed_adapters() {
        let registry = registry();
        let packets = [
            ("xclip", &[types::CLIPBOARD, types::CLIPBOARD_CONNECT][..]),
            (
                "notify-send",
                &[
                    types::NOTIFICATION,
                    types::FINDMYPHONE_REQUEST,
                    types::FINDTHISDEVICE_PRIVATE,
                ][..],
            ),
            ("playerctl", &[types::MPRIS_REQUEST, types::TELEPHONY][..]),
            (
                "wpctl",
                &[types::SYSTEMVOLUME, types::SYSTEMVOLUME_REQUEST][..],
            ),
            ("loginctl", &[types::LOCK_REQUEST][..]),
        ];

        for (tool, accepted) in packets {
            let without = install_backends(Desktop::default(), false, |program| program != tool);
            let capabilities = registry.capabilities(&without).unwrap();
            for packet in accepted {
                assert!(
                    !capabilities.accepts(packet),
                    "{packet} advertised without {tool}"
                );
            }

            let with = install_backends(Desktop::default(), false, |program| program == tool);
            let capabilities = registry.capabilities(&with).unwrap();
            for packet in accepted {
                assert!(capabilities.accepts(packet), "{packet} missing with {tool}");
            }
        }
    }

    #[test]
    fn shell_and_screensaver_adapters_add_no_packets() {
        let registry = registry();
        let desktop = install_backends(Desktop::default(), false, |program| {
            program == "xdg-open" || program == "systemd-inhibit"
        });
        assert!(desktop.capabilities().shell);
        assert!(desktop.capabilities().screensaver);

        let capabilities = registry.capabilities(&desktop).unwrap();
        assert!(!capabilities.accepts(types::RUNCOMMAND_REQUEST));
        assert!(!capabilities.accepts(types::LOCK_REQUEST));
    }

    #[tokio::test]
    async fn link_events_are_preferred_over_share_events() {
        for _ in 0..64 {
            let (link_tx, mut link_rx) = mpsc::channel(1);
            let (share_tx, mut share_rx) = mpsc::channel(1);
            link_tx.send(()).await.expect("send a link event");
            share_tx.send(()).await.expect("send a share event");
            let winner = tokio::select! {
                biased;
                _ = link_rx.recv() => "link",
                _ = share_rx.recv() => "share",
            };
            assert_eq!(
                winner, "link",
                "the daemon select must handle a pending link event before a share event"
            );
        }
    }

    #[test]
    fn discovery_writes_are_throttled_by_device_id_until_the_interval() {
        let peer = DeviceId::new("0123456789abcdef0123456789abcdef").unwrap();
        let start = Instant::now();
        let mut writes = DiscoveryWrites::default();

        assert!(writes.should_write(&peer, start), "a new peer is written");
        writes.record_written(&peer, start);
        assert!(
            !writes.should_write(&peer, start + Duration::from_secs(59)),
            "a sighting inside the interval is skipped"
        );
        assert!(
            writes.should_write(&peer, start + DISCOVERY_WRITE_INTERVAL),
            "an expired sighting is written"
        );

        writes.record_written(&peer, start + Duration::from_secs(10));
        assert!(
            !writes.should_write(&peer, start + Duration::from_secs(11)),
            "a write restarts the interval"
        );
        let other = DeviceId::new("fedcba9876543210fedcba9876543210").unwrap();
        assert!(
            writes.should_write(&other, start + Duration::from_secs(11)),
            "another peer has its own entry"
        );
    }

    #[test]
    fn discovery_write_cache_evicts_the_oldest_peer() {
        let start = Instant::now();
        let mut writes = DiscoveryWrites::default();
        let ids: Vec<DeviceId> = (0..DISCOVERY_WRITE_PEERS)
            .map(|index| DeviceId::new(format!("{index:032x}")).unwrap())
            .collect();
        for (index, id) in ids.iter().enumerate() {
            writes.record_written(id, start + Duration::from_millis(index as u64));
        }
        assert_eq!(writes.peers.len(), DISCOVERY_WRITE_PEERS);

        let newest = DeviceId::new(format!("{:032x}", DISCOVERY_WRITE_PEERS)).unwrap();
        writes.record_written(&newest, start + Duration::from_secs(1));

        assert_eq!(
            writes.peers.len(),
            DISCOVERY_WRITE_PEERS,
            "the cache stays capped"
        );
        assert!(
            !writes.peers.contains_key(&ids[0]),
            "the oldest entry is evicted"
        );
        assert!(
            !writes.should_write(&ids[1], start + Duration::from_secs(1)),
            "the next oldest entry is kept"
        );
        assert!(
            !writes.should_write(&newest, start + Duration::from_secs(1)),
            "the new entry is kept"
        );

        writes.record_written(&ids[1], start + Duration::from_secs(2));
        assert!(
            writes.peers.contains_key(&ids[2]),
            "rewriting an existing peer evicts no one"
        );
    }

    #[test]
    fn discovery_write_cache_skips_new_peers_until_an_entry_expires() {
        let start = Instant::now();
        let mut writes = DiscoveryWrites::default();
        for index in 0..DISCOVERY_WRITE_PEERS {
            let id = DeviceId::new(format!("{index:032x}")).unwrap();
            writes.record_written(&id, start);
        }

        let fresh = DeviceId::new(format!("{:032x}", DISCOVERY_WRITE_PEERS)).unwrap();
        assert!(
            !writes.should_write(&fresh, start + Duration::from_secs(1)),
            "a new peer is skipped while every entry is fresh"
        );
        assert!(
            writes.should_write(&fresh, start + DISCOVERY_WRITE_INTERVAL),
            "a new peer is written once an entry expires"
        );
    }

    #[tokio::test]
    async fn discovery_announces_only_while_no_link_is_active() {
        use std::net::Ipv4Addr;

        use handfast_net::TlsIdentity;
        use handfast_proto::Capabilities;
        use handfast_testkit::peer::PeerConfig;
        use handfast_testkit::tls_peer::{dial, TlsPeerConfig, TlsVersion};

        let local_id = DeviceId::new("ffeeddccbbaa99887766554433221100").expect("valid local ID");
        let tls = Arc::new(TlsIdentity::generate(local_id.clone()).expect("generate the identity"));
        let local = Arc::new(
            LocalEndpoint::new(
                Arc::clone(&tls),
                Identity::new(
                    local_id.clone(),
                    DeviceName::new("Discovery pause test"),
                    DeviceType::Desktop,
                    Capabilities::default(),
                    Some(1716),
                ),
            )
            .expect("the IDs match"),
        );
        let listener = LinkListener::bind_in_range(1716, Arc::clone(&tls))
            .await
            .expect("bind a listener in the KDE Connect range");
        let address = SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            listener
                .local_addr()
                .expect("read the bound address")
                .port(),
        ));
        let (link_tx, mut link_rx) = mpsc::channel(16);
        let (share_tx, _share_rx) = mpsc::channel(16);
        let (links, manager_task) = LinkManager::start(
            listener,
            local,
            TrustTable::default(),
            LinkLimits::default(),
            link_tx,
            share_tx,
        );

        assert!(should_announce(&links), "an idle daemon announces");

        let mut peer_identity = PeerConfig::client();
        peer_identity.device_id = "fedcba9876543210fedcba9876543210".to_string();
        let peer_config =
            TlsPeerConfig::ecdsa(peer_identity, TlsVersion::Both).expect("build the testkit peer");
        let peer = dial(address, local_id.as_str(), &peer_config)
            .await
            .expect("the testkit peer completes the link");
        let connected = tokio::time::timeout(Duration::from_secs(5), link_rx.recv())
            .await
            .expect("Connected arrives")
            .expect("the manager is running");
        assert!(matches!(connected, LinkEvent::Connected { .. }));
        assert!(
            !should_announce(&links),
            "an active link pauses the broadcast"
        );

        peer.close().await.expect("close the testkit link");
        let disconnected = tokio::time::timeout(Duration::from_secs(5), link_rx.recv())
            .await
            .expect("Disconnected arrives")
            .expect("the manager is running");
        assert!(matches!(disconnected, LinkEvent::Disconnected { .. }));
        assert!(
            should_announce(&links),
            "the tick after the last close announces again"
        );

        links.shutdown();
        let _ = manager_task.await;
    }

    #[tokio::test]
    async fn a_share_during_pairing_is_queued_and_delivered_after_pairing() {
        use std::net::Ipv4Addr;

        use handfast_desktop::MockDesktop;
        use handfast_net::{PayloadLimits, TlsIdentity};
        use handfast_proto::{text_request, Capabilities};
        use handfast_testkit::peer::PeerConfig;
        use handfast_testkit::tls_peer::{dial, TlsPeerConfig, TlsVersion};

        let directory = tempfile::tempdir().expect("create the test state directory");
        let store =
            Arc::new(Store::open(directory.path().join("state.db")).expect("open the store"));
        let registry = Arc::new(registry());
        let mock = MockDesktop::with_files().expect("build the mock desktop");
        let desktop = mock.desktop();

        let local_id = DeviceId::new("ffeeddccbbaa99887766554433221100").expect("valid local ID");
        let tls =
            Arc::new(TlsIdentity::generate(local_id.clone()).expect("generate the local identity"));
        let local = Arc::new(
            LocalEndpoint::new(
                Arc::clone(&tls),
                Identity::new(
                    local_id.clone(),
                    DeviceName::new("Daemon test"),
                    DeviceType::Desktop,
                    Capabilities::default(),
                    Some(1716),
                ),
            )
            .expect("the IDs match"),
        );
        let listener = LinkListener::bind_in_range(1716, Arc::clone(&tls))
            .await
            .expect("bind a listener in the KDE Connect range");
        let address = SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            listener
                .local_addr()
                .expect("read the bound address")
                .port(),
        ));
        let (link_tx, mut link_rx) = mpsc::channel(16);
        let (share_tx, mut share_rx) = mpsc::channel(16);
        let (links, manager_task) = LinkManager::start(
            listener,
            local,
            TrustTable::default(),
            LinkLimits::default(),
            link_tx,
            share_tx,
        );

        let payloads = Arc::new(PayloadService::new(
            Arc::clone(&tls),
            PayloadLimits::default(),
        ));
        let (events, mut events_rx) = mpsc::channel(16);
        let (prompt_tx, mut prompt_rx) = mpsc::channel(8);
        let (shutdown, _shutdown_rx) = oneshot::channel();
        let mut daemon = Daemon {
            store,
            registry,
            desktop,
            links: links.clone(),
            payloads,
            limits: ShareLimits::default(),
            events,
            sessions: HashMap::new(),
            pending: HashMap::new(),
            prompts: HashMap::new(),
            next_prompt: 0,
            prompt_tx,
            prompt_task: tokio::spawn(async {}),
            started_at: Instant::now(),
            identity: IdentitySummary {
                device_id: local_id.clone(),
                device_name: "Daemon test".to_string(),
                device_type: "desktop".to_string(),
                fingerprint: "00".repeat(32),
            },
            incoming_dir: directory.path().join("incoming"),
            connected: HashMap::new(),
            transfers: HashMap::new(),
            shutdown: Some(shutdown),
        };

        let mut peer_identity = PeerConfig::client();
        peer_identity.device_id = "fedcba9876543210fedcba9876543210".to_string();
        let peer_config =
            TlsPeerConfig::ecdsa(peer_identity, TlsVersion::Both).expect("build the testkit peer");
        let device_id = DeviceId::new(&peer_config.identity.device_id).expect("valid peer ID");
        let mut peer = dial(address, local_id.as_str(), &peer_config)
            .await
            .expect("the testkit peer completes the link");

        let connected = tokio::time::timeout(Duration::from_secs(5), link_rx.recv())
            .await
            .expect("Connected arrives")
            .expect("the manager is running");
        let LinkEvent::Connected { paired, .. } = &connected else {
            panic!("expected Connected");
        };
        assert!(!*paired, "the link starts unpaired");
        daemon.handle_link_event(connected).await;
        assert!(
            daemon.pending.contains_key(&device_id),
            "the link waits for pairing"
        );

        // Hold the answer so the manager pin and the share both land before
        // PairingAccepted reaches the daemon.
        peer.send(&peer_config.identity.pair_packet(true, unix_now()))
            .await
            .expect("send the pairing request");
        let event = tokio::time::timeout(Duration::from_secs(5), link_rx.recv())
            .await
            .expect("the pairing request arrives")
            .expect("the manager is running");
        daemon.handle_link_event(event).await;
        let prompt = tokio::time::timeout(Duration::from_secs(5), prompt_rx.recv())
            .await
            .expect("the prompt is queued")
            .expect("the daemon is running");

        let pin = Fingerprint::of_certificate(peer_config.cert_der());
        links
            .set_trust(device_id.clone(), Some(pin))
            .await
            .expect("the manager opens the trust gate before the session exists");

        peer.send(&share_text_wire("during pairing"))
            .await
            .expect("send a share during pairing");
        let racing_share = tokio::time::timeout(Duration::from_secs(5), share_rx.recv())
            .await
            .expect("the share reaches the daemon")
            .expect("the manager is running");
        daemon.handle_share(racing_share).await;

        assert!(
            daemon.sessions.is_empty(),
            "a share during pairing creates no session"
        );
        assert!(
            daemon.pending.contains_key(&device_id),
            "a share during pairing keeps the pending link"
        );
        assert!(
            !matches!(link_rx.try_recv(), Ok(LinkEvent::Disconnected { .. })),
            "a share during pairing does not close the link"
        );

        daemon
            .handle_daemon_event(DaemonEvent::PairingAccepted {
                device_id: device_id.clone(),
                token: prompt.token,
            })
            .await;
        assert!(
            daemon.sessions.contains_key(&device_id),
            "PairingAccepted creates the session"
        );

        peer.send(&share_text_wire("after pairing"))
            .await
            .expect("send a share after pairing");
        let later_share = tokio::time::timeout(Duration::from_secs(5), share_rx.recv())
            .await
            .expect("the later share reaches the daemon")
            .expect("the manager is running");
        daemon.handle_share(later_share).await;

        let texts = tokio::time::timeout(Duration::from_secs(5), async {
            let mut texts = Vec::new();
            while let Some(DaemonEvent::PluginEvents { events, .. }) = events_rx.recv().await {
                for event in events {
                    if let PluginEvent::ShareText { text } = event {
                        texts.push(text);
                        if texts.len() == 2 {
                            return texts;
                        }
                    }
                }
            }
            panic!("the daemon event channel closed");
        })
        .await
        .expect("both shares reach the plugin");
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0], "during pairing", "the queued share is delivered");
        assert_eq!(texts[1], "after pairing", "the later share is delivered");

        let session = daemon
            .sessions
            .remove(&device_id)
            .expect("the session exists");
        session.handle().stop("test done".to_string());
        tokio::time::timeout(Duration::from_secs(5), session.into_task())
            .await
            .expect("the session exits")
            .expect("the session task joins");

        daemon
            .handle_share(ShareEvent {
                device_id: device_id.clone(),
                packet: text_request("no session").expect("build the share"),
            })
            .await;
        let disconnected = tokio::time::timeout(Duration::from_secs(5), link_rx.recv())
            .await
            .expect("the manager closes the link")
            .expect("the manager is running");
        assert!(
            matches!(
                disconnected,
                LinkEvent::Disconnected { reason, .. } if reason == "closed by manager"
            ),
            "a share with no session and no pending link closes the link"
        );

        links.shutdown();
        let _ = manager_task.await;
    }
}
