//! `handfastd` composition root.
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
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use handfast_desktop::Desktop;
use handfast_net::{
    Discovery, Fingerprint, LinkEvent, LinkLimits, LinkListener, LinkManager, LinkManagerHandle,
    LocalEndpoint, PendingLink, TrustTable, SHARE_EVENT_QUEUE,
};
use handfast_plugins::registry;
use handfast_proto::{
    DeviceId, DeviceName, DeviceType, Identity, PairingRequest, MAX_TCP_PORT, MIN_TCP_PORT,
};
use handfast_runtime::Registry;
use handfast_store::Store;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

mod identity;
mod pairing;
mod session;

use session::{DaemonEvent, DeviceSession};

/// How often to announce this device over UDP discovery.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5);

/// The link event channel. Connection and pairing events use `send().await`, so
/// the daemon must drain this queue without stalling on a prompt or a session.
const LINK_EVENT_QUEUE: usize = 256;

/// The daemon event channel from sessions and pairing tasks.
const DAEMON_EVENT_QUEUE: usize = 256;

/// How long shutdown waits for tasks before aborting them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(name = "handfastd", version, about = "Handfast KDE Connect daemon")]
struct Args {
    /// State directory. Defaults to the XDG data directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// TCP control port, in the KDE Connect range 1716-1764.
    #[arg(long, default_value = "1716", value_parser = parse_tcp_port)]
    tcp_port: u16,

    /// UDP discovery port.
    #[arg(long, default_value_t = handfast_net::DISCOVERY_PORT)]
    discovery_port: u16,

    /// Print this device's identity packet and exit.
    #[arg(long)]
    print_identity: bool,
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("handfastd: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run());
    // A pending terminal read runs on a blocking thread. Bound runtime
    // shutdown so a prompt cannot hold the process open.
    runtime.shutdown_timeout(SHUTDOWN_GRACE);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("handfastd: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;
    identity::secure_state_dir(&data_dir)?;
    let state_path = data_dir.join("state.db");
    let store = Arc::new(Store::open(&state_path)?);
    identity::secure_state_file(&state_path)?;
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
    tracing::info!(
        fingerprint = %tls.fingerprint().to_hex(),
        "TLS identity loaded"
    );
    let listener = LinkListener::bind_in_range(args.tcp_port, Arc::clone(&tls)).await?;
    let tcp_port = listener.local_addr()?.port();
    if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&tcp_port) {
        return Err(format!(
            "TCP listener bound outside {MIN_TCP_PORT}-{MAX_TCP_PORT}: {tcp_port}"
        )
        .into());
    }
    tracing::info!(tcp_port, "TCP listener bound");

    let identity = Identity::new(
        device_id,
        DeviceName::new(&name),
        DeviceType::Desktop,
        capabilities,
        Some(tcp_port),
    );
    let local = Arc::new(LocalEndpoint::new(tls, identity.clone())?);

    let mut trust = TrustTable::default();
    for (id, pin) in store.trust_table()? {
        trust.set(DeviceId::new(id)?, Fingerprint::from_bytes(pin));
    }
    let (link_tx, mut link_rx) = mpsc::channel(LINK_EVENT_QUEUE);
    let (share_tx, mut share_rx) = mpsc::channel(SHARE_EVENT_QUEUE);
    let (links, manager_task) = LinkManager::start(
        listener,
        local,
        trust,
        LinkLimits::default(),
        link_tx,
        share_tx,
    );
    // The inbound transfer runner lands with the share session unit. Until
    // then, drain the reliable channel so a share request cannot fill it and
    // close the link.
    let _share_drain = tokio::spawn(async move {
        while let Some(event) = share_rx.recv().await {
            tracing::warn!(
                device_id = %event.device_id,
                ptype = %event.packet.ptype,
                "dropping a share packet; the session transfer runner is not implemented"
            );
        }
    });

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
        loop {
            tokio::select! {
                _ = announce.tick() => {
                    if let Err(error) = discovery.announce_broadcast().await {
                        tracing::warn!(%error, "discovery announcement failed");
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

    let mut daemon = Daemon {
        store,
        registry,
        desktop,
        links,
        events: daemon_tx,
        sessions: HashMap::new(),
        pending: HashMap::new(),
        prompts: HashMap::new(),
        next_prompt: 0,
        prompt_tx,
        prompt_task,
    };

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
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
            event = daemon_rx.recv() => {
                match event {
                    Some(event) => daemon.handle_daemon_event(event).await,
                    None => break,
                }
            }
        }
    }

    // Dropping the receivers before shutdown keeps a full event queue from
    // stalling the manager while it closes links.
    drop(link_rx);
    drop(daemon_rx);
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
    events: mpsc::Sender<DaemonEvent>,
    sessions: HashMap<DeviceId, DeviceSession>,
    pending: HashMap<DeviceId, PendingLink>,
    prompts: HashMap<DeviceId, (u64, oneshot::Sender<()>)>,
    next_prompt: u64,
    prompt_tx: mpsc::Sender<pairing::PromptRequest>,
    prompt_task: JoinHandle<()>,
}

impl Daemon {
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
                    let session = DeviceSession::start(
                        peer,
                        link,
                        Arc::clone(&self.registry),
                        self.desktop.clone(),
                        self.events.clone(),
                    );
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
                } else if let Some(link) = self.pending.get_mut(&device_id) {
                    if link.queued.len() < 32 {
                        link.queued.push_back(packet);
                    } else {
                        tracing::debug!(%device_id, "dropping a packet for a full pending queue");
                    }
                } else {
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
                tracing::info!(%device_id, events = ?events, "plugin events");
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
                let session = DeviceSession::start(
                    pending.peer,
                    pending.link,
                    Arc::clone(&self.registry),
                    self.desktop.clone(),
                    self.events.clone(),
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

fn parse_tcp_port(value: &str) -> Result<u16, String> {
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

fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(value) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(value).join("handfast");
        }
    }
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(value).join("handfast");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/handfast");
    }
    PathBuf::from("handfast-data")
}
