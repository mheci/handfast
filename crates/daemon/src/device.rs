//! Device management: live connections, pairing, plugin dispatch.
//!
//! A single actor task owns the device table ([`Manager::run`]). Everything
//! else — UDP discovery, the TCP listener, per-connection reader/writer
//! loops, IPC handlers — talks to it through [`Command`] messages. The actor
//! model keeps the table lock-free across await points and gives one place
//! where pairing decisions, certificate pinning and persistence happen.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use handfast_core::bus::{Bus, Event};
use handfast_core::error::{Error, Result};
use handfast_core::store::{DeviceRow, Store};
use handfast_plugins::{Plugin, PluginFactory};
use handfast_protocol::transfer::{TransferMeta, CHUNK_SIZE, UNKNOWN_SIZE};
use handfast_protocol::{
    Identity, Packet, TYPE_CLIPBOARD, TYPE_IDENTITY, TYPE_NOTIFICATION, TYPE_PAIR, TYPE_SHARE,
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

use crate::backends::{SaveTarget, SAVE_DIR_KEY};
use crate::discovery::PeerAnnouncement;
use crate::tls::Transport;
use crate::transfer::TransferEngine;

/// How long an IPC caller waits for a remote pairing decision.
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(30);

/// Messages driving the device manager actor.
pub enum Command {
    /// A peer announced itself over UDP.
    Announce(PeerAnnouncement),
    /// An outbound or inbound TLS handshake finished successfully.
    Connected {
        device_id: String,
        transport: Transport,
    },
    /// A packet arrived on an established connection.
    PacketFrom {
        device_id: String,
        packet: Box<Packet>,
    },
    /// The connection to a device dropped.
    ConnClosed { device_id: String },
    /// IPC: merged view of persisted + live devices.
    ListDevices {
        reply: oneshot::Sender<Vec<serde_json::Value>>,
    },
    /// IPC: initiate pairing; the reply resolves with the remote decision.
    StartPairing {
        device_id: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    /// IPC: revoke trust and tell the peer.
    Unpair {
        device_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// IPC: send a local file (or GVFS/KIO URI) to a device; the reply
    /// resolves with the transfer id once the transfer is registered.
    SendFile {
        device_id: String,
        path: String,
        reply: oneshot::Sender<Result<String>>,
    },
    /// IPC: list active and finished transfers.
    ListTransfers {
        reply: oneshot::Sender<Vec<serde_json::Value>>,
    },
    /// IPC: cancel an ongoing transfer (id from [`Command::SendFile`] /
    /// [`Command::ListTransfers`]).
    CancelTransfer {
        transfer_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A share header announcing a payload arrived on the control link; the
    /// data connection is already established (or the dial failed, in which
    /// case `payload` is `Err`).
    SharePayload {
        device_id: String,
        header: Box<Packet>,
        payload: std::result::Result<Transport, String>,
    },
    /// A spawned transfer task finished; update the registry and emit events.
    TransferOutcome {
        transfer_id: String,
        result: std::result::Result<(), String>,
    },
}

/// Everything the manager knows about one remote device.
struct DeviceState {
    identity: Identity,
    /// Last advertised connect address (ip + tcpSourcePort).
    addr: Option<SocketAddr>,
    /// Fingerprint of the *current* connection's certificate.
    live_fingerprint: Option<String>,
    /// Pinned fingerprint from a completed pairing, if trusted.
    pinned_fingerprint: Option<String>,
    /// Outbound queue of the live connection, if connected.
    outbound: Option<mpsc::Sender<Packet>>,
    /// Resolves the pending IPC pairing request when answered/closed.
    pending_pair_reply: Option<oneshot::Sender<Result<bool>>>,
    plugins: Vec<Box<dyn Plugin>>,
}

impl DeviceState {
    fn new(identity: Identity, factories: &[Box<dyn PluginFactory>]) -> Self {
        Self {
            identity,
            addr: None,
            live_fingerprint: None,
            pinned_fingerprint: None,
            outbound: None,
            pending_pair_reply: None,
            plugins: factories.iter().map(|f| f.create()).collect(),
        }
    }

    fn is_connected(&self) -> bool {
        self.outbound.is_some()
    }

    fn send_out(&self, packet: Packet) -> Result<()> {
        match &self.outbound {
            Some(tx) => tx
                .try_send(packet)
                .map_err(|err| Error::Other(err.to_string())),
            None => Err(Error::Other("device not connected".into())),
        }
    }
}

/// Handle for talking to the running manager actor.
#[derive(Clone)]
pub struct ManagerHandle {
    tx: mpsc::Sender<Command>,
}

impl ManagerHandle {
    pub async fn list_devices(&self) -> Vec<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::ListDevices { reply: tx })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Ask a device to pair; resolves with the remote decision or an error
    /// after [`PAIRING_TIMEOUT`].
    pub async fn start_pairing(&self, device_id: String) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::StartPairing {
                device_id,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Err(Error::Other("device manager stopped".into()));
        }
        match tokio::time::timeout(PAIRING_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::Other("pairing answer channel dropped".into())),
            Err(_) => Err(Error::Other("pairing timed out".into())),
        }
    }

    pub async fn unpair(&self, device_id: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Unpair {
                device_id,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Err(Error::Other("device manager stopped".into()));
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(Error::Other("unpair channel dropped".into())),
        }
    }

    /// Send a local file (or GVFS/KIO URI) to a device; resolves with the
    /// transfer id once registered (the stream runs in the background).
    pub async fn send_file(&self, device_id: String, path: String) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::SendFile {
                device_id,
                path,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Err(Error::Other("device manager stopped".into()));
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(Error::Other("send-file channel dropped".into())),
        }
    }

    /// Snapshot of the transfer registry.
    pub async fn transfer_list(&self) -> Vec<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::ListTransfers { reply: tx })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Cancel an ongoing transfer by id.
    pub async fn transfer_cancel(&self, transfer_id: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::CancelTransfer {
                transfer_id,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Err(Error::Other("device manager stopped".into()));
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(Error::Other("cancel channel dropped".into())),
        }
    }

    pub(crate) fn command_sender(&self) -> mpsc::Sender<Command> {
        self.tx.clone()
    }
}

/// Registry entry for one transfer (active, finished or failed).
#[derive(Debug)]
struct TransferRecord {
    device_id: String,
    direction: String,
    file_name: String,
    total: u64,
    state: String,
    done: u64,
    reason: Option<String>,
    cancel: Arc<AtomicBool>,
}

/// The device manager actor.
pub struct Manager {
    devices: HashMap<String, DeviceState>,
    store: Arc<Store>,
    bus: Bus,
    self_device_id: String,
    self_identity: Identity,
    pair: Arc<handfast_protocol::tls::CertPair>,
    factories: Vec<Box<dyn PluginFactory>>,
    /// Where received files land (local dir or GVFS/KIO URI).
    save_target: SaveTarget,
    /// Receive-side staging engine; shared with spawned transfer tasks.
    engine: Arc<Mutex<TransferEngine>>,
    /// Transfer registry (active + terminal), for `hfctl transfers` / IPC.
    transfers: HashMap<String, TransferRecord>,
    /// Monotonic transfer-id counter.
    next_transfer_seq: u64,
    /// Clone of our own command inbox so spawned helper tasks can report back.
    self_tx: mpsc::Sender<Command>,
    rx: mpsc::Receiver<Command>,
}

impl Manager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        bus: Bus,
        self_device_id: String,
        self_identity: Identity,
        pair: Arc<handfast_protocol::tls::CertPair>,
        factories: Vec<Box<dyn PluginFactory>>,
    ) -> (ManagerHandle, Self) {
        // Resolve the receive destination once: a plain path (or file://) is
        // used directly by the engine; a GVFS/KIO URI means the engine stages
        // in a temp dir and we copy the finished file into the URI.
        let save_raw = store
            .kv_get(SAVE_DIR_KEY)
            .ok()
            .flatten()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| crate::backends::DEFAULT_SAVE_DIR.to_string());
        let save_target = crate::backends::resolve_save_dir(&save_raw);
        let staging_dir = match &save_target {
            SaveTarget::Local(dir) => dir.clone(),
            SaveTarget::Uri(_) => std::env::temp_dir().join("handfast-received"),
        };
        let engine = Arc::new(Mutex::new(TransferEngine::new(staging_dir)));

        let (tx, rx) = mpsc::channel(256);
        (
            ManagerHandle { tx: tx.clone() },
            Self {
                devices: HashMap::new(),
                store,
                bus,
                self_device_id,
                self_identity,
                pair,
                factories,
                save_target,
                engine,
                transfers: HashMap::new(),
                next_transfer_seq: 0,
                self_tx: tx,
                rx,
            },
        )
    }

    /// Allocate a fresh transfer id.
    fn new_transfer_id(&mut self) -> String {
        self.next_transfer_seq += 1;
        format!("t{}", self.next_transfer_seq)
    }

    /// Register a transfer and publish the IPC-visible `TransferAdded` event.
    fn register_transfer(
        &mut self,
        id: String,
        device_id: String,
        direction: String,
        file_name: String,
        total: u64,
        cancel: Arc<AtomicBool>,
    ) {
        self.transfers.insert(
            id.clone(),
            TransferRecord {
                device_id: device_id.clone(),
                direction: direction.clone(),
                file_name: file_name.clone(),
                total,
                state: "active".into(),
                done: 0,
                reason: None,
                cancel,
            },
        );
        self.bus.publish(Event::TransferAdded {
            id,
            device_id,
            direction,
            file_name,
            total,
        });
    }

    /// Restore persisted trust rows so pinned fingerprints survive restarts.
    pub async fn load_persisted(&mut self) -> Result<()> {
        for row in self.store.list_devices()? {
            if row.device_id == self.self_device_id {
                continue;
            }
            let identity = Identity {
                device_id: row.device_id.clone(),
                name: row.name.clone(),
                device_type: row.device_type.clone(),
                protocol_version: handfast_protocol::PROTO_VERSION,
                incoming: Vec::new(),
                outgoing: Vec::new(),
                tcp_source_port: 0,
            };
            let mut state = DeviceState::new(identity, &self.factories);
            state.pinned_fingerprint = row.paired.then(|| row.cert_fingerprint.clone());
            self.devices.insert(row.device_id, state);
        }
        Ok(())
    }

    /// Actor loop. Returns once every [`ManagerHandle`] has been dropped.
    pub async fn run(mut self) -> Result<()> {
        info!("device manager started");
        while let Some(command) = self.rx.recv().await {
            match command {
                Command::Announce(a) => self.on_announce(a),
                Command::Connected {
                    device_id,
                    transport,
                } => {
                    self.on_connected(device_id, transport);
                }
                Command::PacketFrom { device_id, packet } => {
                    self.on_packet(&device_id, *packet).await;
                }
                Command::ConnClosed { device_id } => self.on_closed(&device_id),
                Command::ListDevices { reply } => {
                    let _ = reply.send(self.snapshot());
                }
                Command::StartPairing { device_id, reply } => {
                    self.begin_pairing(&device_id, reply);
                }
                Command::Unpair { device_id, reply } => {
                    let _ = reply.send(self.unpair(&device_id).await);
                }
                Command::SendFile {
                    device_id,
                    path,
                    reply,
                } => {
                    let result = self.send_file(&device_id, &path).await;
                    let _ = reply.send(result);
                }
                Command::ListTransfers { reply } => {
                    let _ = reply.send(self.transfer_snapshot());
                }
                Command::CancelTransfer { transfer_id, reply } => {
                    let _ = reply.send(self.cancel_transfer(&transfer_id));
                }
                Command::SharePayload {
                    device_id,
                    header,
                    payload,
                } => {
                    self.on_share_payload(&device_id, *header, payload);
                }
                Command::TransferOutcome {
                    transfer_id,
                    result,
                } => {
                    self.on_transfer_outcome(&transfer_id, result).await;
                }
            }
        }
        info!("device manager stopped");
        Ok(())
    }

    /// IPC-visible transfer registry (active + finished + failed).
    fn transfer_snapshot(&self) -> Vec<serde_json::Value> {
        let mut rows: Vec<serde_json::Value> = self
            .transfers
            .iter()
            .map(|(id, record)| {
                json!({
                    "id": id,
                    "device_id": record.device_id,
                    "direction": record.direction,
                    "file_name": record.file_name,
                    "total": record.total,
                    "state": record.state,
                    "done": record.done,
                    "reason": record.reason,
                })
            })
            .collect();
        rows.sort_by_key(|row| row["id"].as_str().unwrap_or("").to_string());
        rows
    }

    /// Cancel an active transfer by flipping its cancel flag.
    fn cancel_transfer(&mut self, transfer_id: &str) -> Result<()> {
        let record = self
            .transfers
            .get(transfer_id)
            .ok_or_else(|| Error::Other(format!("unknown transfer '{transfer_id}'")))?;
        if record.state != "active" {
            return Err(Error::Other(format!(
                "transfer '{transfer_id}' is not active"
            )));
        }
        record.cancel.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Terminal-state handler for spawned transfer tasks.
    async fn on_transfer_outcome(
        &mut self,
        transfer_id: &str,
        result: std::result::Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                if let Some(record) = self.transfers.get_mut(transfer_id) {
                    record.state = "finished".into();
                    record.reason = None;
                }
                self.bus.publish(Event::TransferFinished {
                    id: transfer_id.to_string(),
                });
            }
            Err(reason) => {
                // Sweep any partial staging file the task may have left.
                let mut engine = self.engine.lock().await;
                let _ = engine.abort(transfer_id).await;
                drop(engine);
                if let Some(record) = self.transfers.get_mut(transfer_id) {
                    record.state = "failed".into();
                    record.reason = Some(reason.clone());
                }
                self.bus.publish(Event::TransferFailed {
                    id: transfer_id.to_string(),
                    reason,
                });
            }
        }
    }

    /// Update the table from an announcement; returns true if a dial attempt
    /// is warranted (fresh device or known-but-disconnected).
    fn entry_for_announcement(&mut self, a: &PeerAnnouncement) -> bool {
        let id = a.identity.device_id.clone();
        let fresh_new = !self.devices.contains_key(&id);
        if fresh_new {
            let state = DeviceState::new(a.identity.clone(), &self.factories);
            self.devices.insert(id.clone(), state);
        }
        if let Some(entry) = self.devices.get_mut(&id) {
            entry.identity = a.identity.clone();
            entry.addr = Some(SocketAddr::new(a.ip, a.identity.tcp_source_port));
        }
        if fresh_new {
            self.bus.publish(Event::DeviceFound {
                id: id.clone(),
                name: a.identity.name.clone(),
            });
        }
        fresh_new || !self.devices.get(&id).is_some_and(|d| d.is_connected())
    }

    fn on_announce(&mut self, a: PeerAnnouncement) {
        if !self.entry_for_announcement(&a) {
            return;
        }
        let device_id = a.identity.device_id.clone();
        let Some(addr) = self.devices.get(&device_id).and_then(|d| d.addr) else {
            return;
        };
        let pair = self.pair.clone();
        let identity = self.self_identity.clone();
        let tx = self.self_tx.clone();
        // Fire-and-forget dial; success lands back as Command::Connected.
        // The secure identity returned by the handshake must still match the
        // announced one, or a spoofed announcement could hijack the link.
        tokio::spawn(async move {
            match async { crate::handshake::dial_control(addr, pair, &identity).await }.await {
                Ok((secure, transport)) if secure.device_id == device_id => {
                    let _ = tx
                        .send(Command::Connected {
                            device_id: secure.device_id,
                            transport,
                        })
                        .await;
                }
                Ok((secure, _)) => {
                    debug!(
                        announced = %device_id,
                        secure = %secure.device_id,
                        "secure identity did not match announcement"
                    );
                }
                Err(err) => debug!(%err, %addr, "outbound dial failed"),
            }
        });
    }

    fn on_connected(&mut self, device_id: String, transport: Transport) {
        // Pinning gate BEFORE any traffic flows: paired devices must present
        // exactly the certificate we originally paired with.
        let fingerprint = hex(&transport.peer_fingerprint());
        if let Some(state) = self.devices.get(&device_id) {
            if let Some(pinned) = &state.pinned_fingerprint {
                if pinned != &fingerprint {
                    warn!(device = %device_id, "certificate mismatch for paired device; dropping");
                    self.bus.publish(Event::LogRecord {
                        level: "error".into(),
                        msg: format!("cert mismatch for {device_id}: possible spoofing"),
                    });
                    return; // transport drops here, closing the TCP stream
                }
            }
        }

        let (out_tx, mut out_rx) = mpsc::channel::<Packet>(64);
        let cmd_tx = self.self_tx.clone();
        let pair = self.pair.clone();
        let peer_addr = transport.peer_addr();
        let (mut reader, mut writer) = transport.into_parts();

        // Writer loop: drain the outbound queue onto the wire.
        tokio::spawn(async move {
            while let Some(packet) = out_rx.recv().await {
                if packet.write_to(&mut writer).await.is_err() {
                    break;
                }
            }
            // Channel closed (device dropped/manager gone): half closes via drop.
        });

        // Reader loop: push every packet to the actor; report closure once.
        // Share headers announcing a payload get their data connection dialed
        // here (we have the peer address and certificate pair) so the actor
        // receives the stream ready to consume.
        let reader_device_id = device_id.clone();
        let closer_device_id = device_id.clone();
        tokio::spawn(async move {
            loop {
                match Packet::read_from(&mut reader).await {
                    Ok(packet) => {
                        let payload = match packet.payload_transfer_port() {
                            Some(port) => Some(
                                crate::payload::connect_payload(peer_addr, port, pair.clone())
                                    .await
                                    .map_err(|err| err.to_string()),
                            ),
                            None => None,
                        };
                        let cmd = match payload {
                            Some(result) => Command::SharePayload {
                                device_id: reader_device_id.clone(),
                                header: Box::new(packet),
                                payload: result,
                            },
                            None => Command::PacketFrom {
                                device_id: reader_device_id.clone(),
                                packet: Box::new(packet),
                            },
                        };
                        if cmd_tx.send(cmd).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        debug!(%err, "connection read ended");
                        break;
                    }
                }
            }
            let _ = cmd_tx
                .send(Command::ConnClosed {
                    device_id: closer_device_id,
                })
                .await;
        });

        if let Some(state) = self.devices.get_mut(&device_id) {
            state.outbound = Some(out_tx);
            state.live_fingerprint = Some(fingerprint);
            self.bus.publish(Event::DeviceStateChanged {
                id: device_id.clone(),
                state: "online".into(),
            });
            info!(device = %device_id, "connection established");
        } else {
            // Unknown device (inbound without a prior announcement): shell entry.
            let mut state = DeviceState::new(
                Identity {
                    device_id: device_id.clone(),
                    name: device_id.clone(),
                    device_type: "unknown".into(),
                    protocol_version: handfast_protocol::PROTO_VERSION,
                    incoming: Vec::new(),
                    outgoing: Vec::new(),
                    tcp_source_port: 0,
                },
                &self.factories,
            );
            state.outbound = Some(out_tx);
            state.live_fingerprint = Some(fingerprint);
            self.devices.insert(device_id.clone(), state);
            self.bus.publish(Event::DeviceFound {
                id: device_id,
                name: "unknown".into(),
            });
        }
    }

    /// Send a local file (or a GVFS/KIO URI) to `device_id`.
    ///
    /// Streams the payload over a second TLS connection exactly like upstream:
    /// bind a data port, announce `payloadSize` + `payloadTransferInfo.port`
    /// in the share header, then pump the file bytes to whoever dials us.
    async fn send_file(&mut self, device_id: &str, path: &str) -> Result<String> {
        let materialized = crate::backends::materialize_source(path).await?;
        let local = materialized.local;
        let cleanup = materialized.cleanup;

        let metadata = match tokio::fs::metadata(&local).await {
            Ok(meta) => meta,
            Err(err) => {
                if let Some(tmp) = cleanup {
                    let _ = tokio::fs::remove_file(tmp).await;
                }
                return Err(Error::Other(format!(
                    "cannot stat {}: {err}",
                    local.display()
                )));
            }
        };
        if !metadata.is_file() {
            if let Some(tmp) = cleanup {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            return Err(Error::Other(format!(
                "{} is not a regular file",
                local.display()
            )));
        }
        let size = metadata.len();
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let Some(state) = self.devices.get_mut(device_id) else {
            if let Some(tmp) = cleanup {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            return Err(Error::Other(format!("device '{device_id}' is unknown")));
        };
        if !state.is_connected() {
            if let Some(tmp) = cleanup {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            return Err(Error::Other(format!("device '{device_id}' is offline")));
        }

        let file_name = local
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();

        // Empty file: a bare header (no payloadSize / no port), matching
        // upstream's hasPayload()==false shortcut.
        if size == 0 {
            let header = Packet::new(
                TYPE_SHARE,
                json!({
                    "filename": file_name,
                    "lastModified": modified_ms,
                    "numberOfFiles": 1,
                    "totalPayloadSize": 0,
                }),
            );
            state.send_out(header)?;
            let transfer_id = self.new_transfer_id();
            self.register_transfer(
                transfer_id.clone(),
                device_id.to_string(),
                "outgoing".into(),
                file_name,
                0,
                Arc::new(AtomicBool::new(false)),
            );
            if let Some(record) = self.transfers.get_mut(&transfer_id) {
                record.state = "finished".into();
            }
            self.bus.publish(Event::TransferFinished {
                id: transfer_id.clone(),
            });
            return Ok(transfer_id);
        }

        let (port, listener) = crate::payload::PayloadListener::bind(self.pair.clone()).await?;

        let header = Packet::new(
            TYPE_SHARE,
            json!({
                "filename": file_name,
                "creationTime": modified_ms,
                "lastModified": modified_ms,
                "numberOfFiles": 1,
                "totalPayloadSize": size,
            }),
        )
        .with_payload(size as i64, port);
        if let Err(err) = state.send_out(header) {
            if let Some(tmp) = cleanup {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            return Err(err);
        }

        let transfer_id = self.new_transfer_id();
        let cancel = Arc::new(AtomicBool::new(false));
        self.register_transfer(
            transfer_id.clone(),
            device_id.to_string(),
            "outgoing".into(),
            file_name.clone(),
            size,
            cancel.clone(),
        );

        let bus = self.bus.clone();
        let self_tx = self.self_tx.clone();
        let tx_id = transfer_id.clone();
        let dev_id = device_id.to_string();
        tokio::spawn(async move {
            let result =
                stream_outgoing(tx_id.clone(), listener, local, cleanup, size, bus, cancel).await;
            let _ = self_tx
                .send(Command::TransferOutcome {
                    transfer_id: tx_id,
                    result: result.map_err(|err| format!("{dev_id}: {err}")),
                })
                .await;
        });

        Ok(transfer_id)
    }

    /// A share header with a payload arrived (or its data dial failed).
    fn on_share_payload(
        &mut self,
        device_id: &str,
        header: Packet,
        payload: std::result::Result<Transport, String>,
    ) {
        match payload {
            Ok(transport) => self.spawn_receive(device_id, header, Some(transport)),
            Err(reason) => {
                // Data connection could not be established; record a failed
                // transfer so the user sees why the file never arrived.
                let file_name = header
                    .body
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("received-file")
                    .to_string();
                let transfer_id = self.new_transfer_id();
                self.register_transfer(
                    transfer_id.clone(),
                    device_id.to_string(),
                    "incoming".into(),
                    file_name,
                    0,
                    Arc::new(AtomicBool::new(false)),
                );
                if let Some(record) = self.transfers.get_mut(&transfer_id) {
                    record.state = "failed".into();
                    record.reason = Some(reason.clone());
                }
                self.bus.publish(Event::TransferFailed {
                    id: transfer_id,
                    reason,
                });
            }
        }
    }

    /// Start a receive for a share header: either with an established data
    /// connection (payload) or without one (empty file).
    fn spawn_receive(&mut self, device_id: &str, header: Packet, payload: Option<Transport>) {
        let Some(_state) = self.devices.get(device_id) else {
            warn!(device = %device_id, "share from unknown device dropped");
            return;
        };
        let file_name = header
            .body
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("received-file")
            .to_string();
        let announced = header.payload_size().unwrap_or(0);
        let size = if announced < 0 {
            UNKNOWN_SIZE
        } else {
            announced as u64
        };

        let transfer_id = self.new_transfer_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let total = if size == UNKNOWN_SIZE { 0 } else { size };
        self.register_transfer(
            transfer_id.clone(),
            device_id.to_string(),
            "incoming".into(),
            file_name.clone(),
            total,
            cancel.clone(),
        );

        let engine = self.engine.clone();
        let bus = self.bus.clone();
        let self_tx = self.self_tx.clone();
        let save_target = self.save_target.clone();
        let tx_id = transfer_id.clone();
        let dev_id = device_id.to_string();
        tokio::spawn(async move {
            let result = receive_stream(
                engine,
                save_target,
                bus,
                tx_id.clone(),
                dev_id.clone(),
                file_name,
                size,
                payload,
                cancel,
            )
            .await;
            let _ = self_tx
                .send(Command::TransferOutcome {
                    transfer_id: tx_id,
                    result: result.map(|_| ()).map_err(|err| err.to_string()),
                })
                .await;
        });
    }

    async fn on_packet(&mut self, device_id: &str, packet: Packet) {
        if packet.ptype == TYPE_IDENTITY {
            if let Ok(identity) = serde_json::from_value::<Identity>(packet.body.clone()) {
                if let Some(state) = self.devices.get_mut(device_id) {
                    state.identity = identity;
                }
            }
            return;
        }

        if packet.ptype == TYPE_PAIR {
            self.on_pair_packet(device_id, packet).await;
            return;
        }

        // Inbound file shares with a `filename` (and no payload connection —
        // those arrive via Command::SharePayload) are empty files: create
        // them, mirroring upstream's hasPayload()==false receive path.
        if packet.ptype == TYPE_SHARE && packet.body.get("filename").is_some() {
            info!(device = %device_id, "receiving empty file share");
            self.spawn_receive(device_id, packet, None);
            return;
        }

        let replies = self.dispatch_to_plugins(device_id, &packet);
        self.emit_side_events(device_id, &packet);

        if !replies.is_empty() {
            if let Some(state) = self.devices.get(device_id) {
                for reply in replies {
                    if let Err(err) = state.send_out(reply) {
                        debug!(%err, "dropping reply; device offline");
                    }
                }
            }
        }
    }

    fn dispatch_to_plugins(&mut self, device_id: &str, packet: &Packet) -> Vec<Packet> {
        let mut replies = Vec::new();
        if let Some(state) = self.devices.get_mut(device_id) {
            for plugin in &mut state.plugins {
                if plugin.meta().incoming.contains(&packet.ptype.as_str()) {
                    let produced = plugin.handle(packet);
                    if !produced.is_empty() {
                        debug!(
                            device = %device_id,
                            plugin = plugin.meta().name,
                            replies = produced.len(),
                            "plugin handled packet"
                        );
                    }
                    replies.extend(produced);
                }
            }
        }
        replies
    }

    fn emit_side_events(&self, device_id: &str, packet: &Packet) {
        let body = &packet.body;
        match packet.ptype.as_str() {
            TYPE_NOTIFICATION => {
                let get = |key: &str| {
                    body.get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                self.bus.publish(Event::NotificationReceived {
                    id: get("id"),
                    app: get("appName"),
                    title: get("title"),
                    body: get("text"),
                });
            }
            TYPE_CLIPBOARD => {
                if let Some(text) = body.get("content").and_then(|v| v.as_str()) {
                    self.bus.publish(Event::ClipboardUpdated {
                        text: text.to_string(),
                    });
                }
            }
            other => {
                debug!(device = %device_id, kind = other, "packet routed (no UI event)");
            }
        }
    }

    async fn on_pair_packet(&mut self, device_id: &str, packet: Packet) {
        let wants_pair = packet.body.get("pair").and_then(|v| v.as_bool());
        let Some(wants_pair) = wants_pair else {
            debug!(device = %device_id, "malformed pair packet");
            return;
        };

        // Case 1: answer to a pairing WE initiated.
        if let Some(reply) = self
            .devices
            .get_mut(device_id)
            .and_then(|s| s.pending_pair_reply.take())
        {
            let _ = reply.send(Ok(wants_pair));
            if wants_pair {
                self.persist_trust(device_id, true).await;
            }
            return;
        }

        // Case 2: the remote wants to pair with us.
        if wants_pair {
            let auto_accept = self
                .store
                .kv_get("pairing.auto_accept")
                .ok()
                .flatten()
                .is_some_and(|v| v == "1");
            info!(device = %device_id, auto_accept, "incoming pairing request");
            let accepted = auto_accept
                && self
                    .devices
                    .get(device_id)
                    .is_some_and(|s| s.is_connected());
            if accepted {
                self.persist_trust(device_id, true).await;
            }
            let response = Packet::new(TYPE_PAIR, serde_json::json!({ "pair": accepted }));
            if let Some(state) = self.devices.get(device_id) {
                let _ = state.send_out(response);
            }
        } else {
            // Remote revoked pairing.
            self.persist_trust(device_id, false).await;
            self.bus.publish(Event::DeviceStateChanged {
                id: device_id.to_string(),
                state: "unpaired".into(),
            });
        }
    }

    fn begin_pairing(&mut self, device_id: &str, reply: oneshot::Sender<Result<bool>>) {
        let Some(state) = self.devices.get_mut(device_id) else {
            let _ = reply.send(Err(Error::Other(format!("unknown device '{device_id}'"))));
            return;
        };
        if !state.is_connected() {
            let _ = reply.send(Err(Error::Other(
                "device not connected yet; wait for discovery".into(),
            )));
            return;
        }
        if state.pinned_fingerprint.is_some() {
            let _ = reply.send(Err(Error::Other("already paired".into())));
            return;
        }
        state.pending_pair_reply = Some(reply);
        let request = Packet::new(TYPE_PAIR, serde_json::json!({ "pair": true }));
        if let Err(err) = state.send_out(request) {
            if let Some(reply) = state.pending_pair_reply.take() {
                let _ = reply.send(Err(err));
            }
        }
    }

    async fn unpair(&mut self, device_id: &str) -> Result<()> {
        let revoke = Packet::new(TYPE_PAIR, serde_json::json!({ "pair": false }));
        if let Some(state) = self.devices.get(device_id) {
            let _ = state.send_out(revoke);
        }
        self.persist_trust(device_id, false).await;
        Ok(())
    }

    fn on_closed(&mut self, device_id: &str) {
        if let Some(state) = self.devices.get_mut(device_id) {
            state.outbound = None;
            state.live_fingerprint = None;
            if let Some(reply) = state.pending_pair_reply.take() {
                let _ = reply.send(Err(Error::Other("connection closed during pairing".into())));
            }
        }
        self.bus.publish(Event::DeviceStateChanged {
            id: device_id.to_string(),
            state: "offline".into(),
        });
    }

    async fn persist_trust(&mut self, device_id: &str, paired: bool) {
        let Some(state) = self.devices.get_mut(device_id) else {
            return;
        };
        let fingerprint = if paired {
            state
                .live_fingerprint
                .clone()
                .or_else(|| state.pinned_fingerprint.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        state.pinned_fingerprint = paired.then(|| fingerprint.clone());

        let identity = state.identity.clone();

        let row = DeviceRow {
            device_id: device_id.to_string(),
            name: identity.name,
            device_type: identity.device_type,
            cert_fingerprint: fingerprint,
            paired,
            last_seen: Some(unix_now()),
        };
        if let Err(err) = self.store.upsert_device(&row) {
            warn!(%err, "failed to persist device trust");
        }
        self.bus.publish(Event::DeviceStateChanged {
            id: device_id.to_string(),
            state: if paired { "paired" } else { "unpaired" }.into(),
        });
    }

    fn snapshot(&self) -> Vec<serde_json::Value> {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for (id, state) in &self.devices {
            rows.push(serde_json::json!({
                "device_id": id,
                "name": state.identity.name,
                "type": state.identity.device_type,
                "paired": state.pinned_fingerprint.is_some(),
                "online": state.is_connected(),
                "capabilities_incoming": state.identity.incoming,
                "capabilities_outgoing": state.identity.outgoing,
            }));
        }
        rows.sort_by(|a, b| {
            let ka = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let kb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            ka.cmp(kb)
        });
        rows
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Pump a local file over the accepted data connection in 4 KiB chunks
/// (upstream's `UploadJob` granularity), reporting progress on the bus.
async fn stream_outgoing(
    transfer_id: String,
    listener: crate::payload::PayloadListener,
    local: PathBuf,
    cleanup: Option<PathBuf>,
    size: u64,
    bus: Bus,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let result: Result<()> = async {
        let mut transport = listener.accept().await?;
        let mut file = tokio::fs::File::open(&local).await?;
        let mut buf = vec![0u8; handfast_protocol::transfer::PAYLOAD_CHUNK_SIZE];
        let mut done: u64 = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Other("transfer cancelled".into()));
            }
            let read = file.read(&mut buf).await?;
            if read == 0 {
                break;
            }
            transport.write_bytes(&buf[..read]).await?;
            done += read as u64;
            bus.publish(Event::TransferProgress {
                id: transfer_id.clone(),
                bytes_done: done,
                bytes_total: size,
            });
            if done >= size {
                break;
            }
        }
        Ok(())
    }
    .await;

    if let Some(tmp) = cleanup {
        let _ = tokio::fs::remove_file(tmp).await;
    }
    result
}

/// Receive a payload stream (or an empty file) into the transfer engine,
/// publishing progress, then finalize it into the save target.
#[allow(clippy::too_many_arguments)]
async fn receive_stream(
    engine: Arc<Mutex<TransferEngine>>,
    save_target: SaveTarget,
    bus: Bus,
    transfer_id: String,
    device_id: String,
    file_name: String,
    size: u64,
    payload: Option<Transport>,
    cancel: Arc<AtomicBool>,
) -> Result<PathBuf> {
    let meta = TransferMeta {
        transfer_id: transfer_id.clone(),
        device_id: device_id.clone(),
        file_name: file_name.clone(),
        file_size: size,
    };
    {
        let mut engine = engine.lock().await;
        engine.start_receive(meta).await?;
    }

    let mut done: u64 = 0;
    if let Some(mut transport) = payload {
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            if cancel.load(Ordering::Relaxed) {
                let mut engine = engine.lock().await;
                let _ = engine.abort(&transfer_id).await;
                return Err(Error::Other("transfer cancelled".into()));
            }
            let read = if size == UNKNOWN_SIZE {
                let chunk = tokio::time::timeout(
                    handfast_protocol::transfer::PAYLOAD_READ_TIMEOUT,
                    transport.read_some(&mut buf),
                )
                .await
                .map_err(|_| Error::Other("payload read timed out".into()))??;
                if chunk == 0 {
                    break; // clean EOF for unknown-size streams
                }
                chunk
            } else {
                let remaining = size - done;
                let want = buf.len().min(remaining as usize);
                if want == 0 {
                    break;
                }
                tokio::time::timeout(
                    handfast_protocol::transfer::PAYLOAD_READ_TIMEOUT,
                    transport.read_bytes(&mut buf[..want]),
                )
                .await
                .map_err(|_| Error::Other("payload read timed out".into()))??;
                want
            };
            engine
                .lock()
                .await
                .write_chunk(&transfer_id, &buf[..read])
                .await?;
            done += read as u64;
            bus.publish(Event::TransferProgress {
                id: transfer_id.clone(),
                bytes_done: done,
                bytes_total: size,
            });
            if size != UNKNOWN_SIZE && done >= size {
                break;
            }
        }
    }

    let final_path = engine.lock().await.finish_receive(&transfer_id).await?;

    // GVFS/KIO destination: copy the finished file into the URI and drop the
    // local staging copy (the engine staged in a temp dir for URI targets).
    if let SaveTarget::Uri(uri) = &save_target {
        if let Err(err) = crate::backends::move_into_uri(&final_path, uri).await {
            let _ = tokio::fs::remove_file(&final_path).await;
            return Err(err);
        }
    }

    let share_path = match &save_target {
        SaveTarget::Uri(uri) => format!("{}/{}", uri.trim_end_matches('/'), file_name),
        SaveTarget::Local(_) => final_path.display().to_string(),
    };
    bus.publish(Event::ShareReceived {
        path: share_path,
        device_id,
    });
    Ok(final_path)
}
