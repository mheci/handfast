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
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use handfast_core::bus::{Bus, Event};
use handfast_core::error::{Error, Result};
use handfast_core::store::{DeviceRow, Store};
use handfast_plugins::{Plugin, PluginFactory};
use handfast_protocol::transfer::TransferMeta;
use handfast_protocol::{
    Identity, Packet, PAYLOAD_TRANSFER_MIN_PORT, TYPE_CLIPBOARD, TYPE_IDENTITY, TYPE_NOTIFICATION,
    TYPE_PAIR, TYPE_SHARE, TYPE_SHARE_UPDATE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::discovery::PeerAnnouncement;
use crate::tls::{PayloadChannel, Transport};
use crate::transfer::{TransferEngine, TransferInfo};

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
    /// IPC: send a local file to a device over the payload channel.
    ShareFile {
        device_id: String,
        path: PathBuf,
        reply: oneshot::Sender<Result<()>>,
    },
    /// IPC: list in-flight transfers (incoming + outgoing).
    TransferList {
        reply: oneshot::Sender<Vec<TransferInfo>>,
    },
    /// IPC: cancel an in-flight transfer.
    TransferCancel {
        transfer_id: String,
        reply: oneshot::Sender<Result<()>>,
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

    /// Queue `path` for transfer to `device_id` over the payload channel.
    ///
    /// Resolves once the transfer has been announced and the payload channel
    /// is listening; byte streaming then proceeds in the background with
    /// progress events on the bus.
    pub async fn share_file(&self, device_id: String, path: PathBuf) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::ShareFile {
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
            Err(_) => Err(Error::Other("share reply channel dropped".into())),
        }
    }

    /// Snapshot of every in-flight transfer.
    pub async fn transfer_list(&self) -> Vec<TransferInfo> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::TransferList { reply: tx })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Cancel an in-flight transfer, deleting any partial staging file.
    pub async fn transfer_cancel(&self, transfer_id: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::TransferCancel {
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
            Err(_) => Err(Error::Other("cancel reply channel dropped".into())),
        }
    }

    pub(crate) fn command_sender(&self) -> mpsc::Sender<Command> {
        self.tx.clone()
    }
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
    /// File-transfer bookkeeping (both directions).
    engine: Arc<tokio::sync::Mutex<TransferEngine>>,
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
        engine: Arc<tokio::sync::Mutex<TransferEngine>>,
    ) -> (ManagerHandle, Self) {
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
                engine,
                self_tx: tx,
                rx,
            },
        )
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
                Command::ShareFile {
                    device_id,
                    path,
                    reply,
                } => {
                    let _ = reply.send(self.on_share_file(&device_id, &path).await);
                }
                Command::TransferList { reply } => {
                    let _ = reply.send(self.engine.lock().await.snapshot());
                }
                Command::TransferCancel { transfer_id, reply } => {
                    let _ = reply.send(self.on_transfer_cancel(&transfer_id).await);
                }
            }
        }
        info!("device manager stopped");
        Ok(())
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
        tokio::spawn(async move {
            match async {
                let transport = Transport::connect(addr, pair).await?;
                crate::handshake::complete_outbound(transport, &identity).await
            }
            .await
            {
                Ok((_, transport)) => {
                    let _ = tx
                        .send(Command::Connected {
                            device_id,
                            transport,
                        })
                        .await;
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
        let reader_device_id = device_id.clone();
        let closer_device_id = device_id.clone();
        tokio::spawn(async move {
            loop {
                match Packet::read_from(&mut reader).await {
                    Ok(packet) => {
                        let cmd = Command::PacketFrom {
                            device_id: reader_device_id.clone(),
                            packet: Box::new(packet),
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

        // File transfers announce themselves as payload-bearing share
        // requests; the file bytes never travel inside the packet itself.
        if packet.ptype == TYPE_SHARE && packet.payload_size.is_some() {
            self.begin_inbound_transfer(device_id, &packet).await;
            return;
        }
        // Composite-transfer progress totals carry no actionable payload for
        // us; our progress is derived from the payload stream directly.
        if packet.ptype == TYPE_SHARE_UPDATE {
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

    /// Start an inbound file transfer announced by a payload-bearing
    /// `kdeconnect.share.request` packet.
    ///
    /// Reserves the destination name immediately (matching upstream, which
    /// also reserves on announce), then dials the sender's announced payload
    /// port as the TLS client, streams exactly `payloadSize` raw bytes into
    /// the staging file, and atomically renames it on completion. Progress
    /// and lifecycle events go out on the bus.
    async fn begin_inbound_transfer(&mut self, device_id: &str, packet: &Packet) {
        let Some(port) = packet.payload_port() else {
            debug!(device = %device_id, "share request missing payload port; ignoring");
            return;
        };
        let Some(file_name) = packet
            .body
            .get("filename")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
        else {
            debug!(device = %device_id, "payload share request without filename; ignoring");
            return;
        };
        let file_size = packet.payload_size.unwrap_or(0).max(0) as u64;

        // Transfers are only accepted from paired devices, mirroring upstream
        // `Device::sendPacket`'s paired gate.
        let (Some(pinned), Some(addr)) = self
            .devices
            .get(device_id)
            .map(|s| (s.pinned_fingerprint.clone(), s.addr))
            .unwrap_or((None, None))
        else {
            debug!(device = %device_id, "ignoring transfer from unpaired or unknown device");
            return;
        };

        let transfer_id = uuid::Uuid::new_v4().to_string();
        let meta = TransferMeta {
            transfer_id: transfer_id.clone(),
            device_id: device_id.to_string(),
            file_name: file_name.clone(),
            file_size,
        };
        if let Err(err) = self.engine.lock().await.start_receive(meta).await {
            warn!(device = %device_id, %err, "could not start inbound transfer");
            self.bus.publish(Event::TransferFailed {
                id: transfer_id,
                reason: err.to_string(),
            });
            return;
        }
        self.bus.publish(Event::TransferAdded {
            id: transfer_id.clone(),
            device_id: device_id.to_string(),
            direction: "incoming".into(),
            file_name,
            file_size,
        });

        let peer_ip = addr.ip();
        let engine = self.engine.clone();
        let bus = self.bus.clone();
        let pair = self.pair.clone();
        tokio::spawn(async move {
            let result = async {
                let channel = tokio::time::timeout(
                    Duration::from_secs(10),
                    PayloadChannel::connect((peer_ip, port), pair),
                )
                .await
                .map_err(|_| Error::Other("payload connect timed out".into()))??;
                if pinned != hex(&channel.peer_fingerprint()) {
                    return Err(Error::Other(
                        "payload certificate does not match paired device".into(),
                    ));
                }
                let (mut reader, _writer) = channel.into_raw();
                let mut done: u64 = 0;
                let mut buf = vec![0u8; handfast_protocol::transfer::CHUNK_SIZE];
                while done < file_size {
                    if engine.lock().await.is_cancelled(&transfer_id) {
                        return Err(Error::Other("cancelled".into()));
                    }
                    let want = usize::try_from(file_size - done)
                        .unwrap_or(buf.len())
                        .min(buf.len());
                    let n = reader.read(&mut buf[..want]).await?;
                    if n == 0 {
                        return Err(Error::Other(format!(
                            "payload stream ended early ({done}/{file_size} bytes)"
                        )));
                    }
                    engine
                        .lock()
                        .await
                        .write_chunk(&transfer_id, &buf[..n])
                        .await?;
                    done += n as u64;
                    bus.publish(Event::TransferProgress {
                        id: transfer_id.clone(),
                        bytes_done: done,
                        bytes_total: file_size,
                    });
                }
                let destination = engine.lock().await.finish_receive(&transfer_id).await?;
                bus.publish(Event::TransferFinished {
                    id: transfer_id.clone(),
                });
                info!(id = %transfer_id, to = %destination.display(), "inbound transfer complete");
                Ok::<_, Error>(())
            }
            .await;
            if let Err(err) = result {
                let reason = err.to_string();
                let _ = engine.lock().await.abort(&transfer_id).await;
                bus.publish(Event::TransferFailed {
                    id: transfer_id,
                    reason,
                });
            }
        });
    }

    /// Send a local file to `device_id` over the payload channel.
    ///
    /// Opens the file, binds a payload listener at or above
    /// [`PAYLOAD_TRANSFER_MIN_PORT`], announces `kdeconnect.share.request`
    /// with `payloadSize` + `payloadTransferInfo`, then accepts the peer's
    /// payload connection as the TLS server and streams the file's bytes
    /// unframed. Resolves once the announce has been queued.
    async fn on_share_file(&mut self, device_id: &str, path: &PathBuf) -> Result<()> {
        let (pinned, connected) = match self.devices.get(device_id) {
            Some(state) => (state.pinned_fingerprint.clone(), state.is_connected()),
            None => return Err(Error::Other(format!("unknown device '{device_id}'"))),
        };
        if pinned.is_none() {
            return Err(Error::Other(format!("device '{device_id}' is not paired")));
        }
        if !connected {
            return Err(Error::Other(format!(
                "device '{device_id}' is not connected"
            )));
        }

        let file = tokio::fs::File::open(path)
            .await
            .map_err(|err| Error::Other(format!("cannot open '{}': {err}", path.display())))?;
        let file_size = file.metadata().await?.len();
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".into());

        let listener = bind_payload_listener().await?;
        let port = listener.local_addr()?.port();

        let transfer_id = uuid::Uuid::new_v4().to_string();
        self.engine
            .lock()
            .await
            .start_upload(
                transfer_id.clone(),
                device_id.to_string(),
                file_name.clone(),
                file_size,
            )
            .map_err(|err| Error::Other(err.to_string()))?;

        let packet = Packet::new(
            TYPE_SHARE,
            serde_json::json!({ "filename": file_name.clone() }),
        )
        .with_payload(file_size, port);
        let state = self
            .devices
            .get(device_id)
            .ok_or_else(|| Error::Other(format!("device '{device_id}' disappeared")))?;
        state.send_out(packet)?;

        self.bus.publish(Event::TransferAdded {
            id: transfer_id.clone(),
            device_id: device_id.to_string(),
            direction: "outgoing".into(),
            file_name,
            file_size,
        });

        let engine = self.engine.clone();
        let bus = self.bus.clone();
        let pair = self.pair.clone();
        let peer_fingerprint = pinned;
        let transfer_id_clone = transfer_id.clone();
        tokio::spawn(async move {
            let result = async {
                let tcp = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                    .await
                    .map_err(|_| Error::Other("peer never fetched the payload".into()))?
                    .map_err(|err| Error::Other(err.to_string()))?
                    .0;
                let channel = PayloadChannel::accept(tcp, pair).await?;
                if peer_fingerprint.as_deref() != Some(hex(&channel.peer_fingerprint()).as_str()) {
                    return Err(Error::Other(
                        "payload certificate does not match paired device".into(),
                    ));
                }
                let (_reader, mut writer) = channel.into_raw();
                let mut file = file;
                let mut sent: u64 = 0;
                let mut buf = vec![0u8; handfast_protocol::transfer::CHUNK_SIZE];
                loop {
                    if engine.lock().await.is_cancelled(&transfer_id_clone) {
                        return Err(Error::Other("cancelled".into()));
                    }
                    let n = file.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    writer.write_all(&buf[..n]).await?;
                    sent += n as u64;
                    engine
                        .lock()
                        .await
                        .upload_progress(&transfer_id_clone, sent);
                    bus.publish(Event::TransferProgress {
                        id: transfer_id_clone.clone(),
                        bytes_done: sent,
                        bytes_total: file_size,
                    });
                }
                writer.flush().await?;
                writer.shutdown().await?;
                engine.lock().await.finish_upload(&transfer_id_clone)?;
                bus.publish(Event::TransferFinished {
                    id: transfer_id_clone.clone(),
                });
                info!(id = %transfer_id_clone, bytes = sent, "outbound transfer complete");
                Ok::<_, Error>(())
            }
            .await;
            if let Err(err) = result {
                let reason = err.to_string();
                let _ = engine.lock().await.fail_upload(&transfer_id_clone);
                bus.publish(Event::TransferFailed {
                    id: transfer_id_clone,
                    reason,
                });
            }
        });
        Ok(())
    }

    /// Cancel an in-flight transfer by id (incoming or outgoing).
    async fn on_transfer_cancel(&mut self, transfer_id: &str) -> Result<()> {
        let mut engine = self.engine.lock().await;
        if engine.is_cancelled(transfer_id) {
            return Ok(());
        }
        // Incoming transfers: abort deletes the staging file and flags the id.
        if engine.abort(transfer_id).await.is_ok() {
            return Ok(());
        }
        // Outgoing transfers: flag the id; the streaming task notices on its
        // next chunk poll and cleans up the tracking entry itself.
        if engine
            .snapshot()
            .iter()
            .any(|t| t.transfer_id == transfer_id && t.direction == "outgoing")
        {
            engine.cancel_outgoing(transfer_id);
            Ok(())
        } else {
            Err(Error::Other(format!("unknown transfer '{transfer_id}'")))
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

/// Bind a payload-channel listener on the first free port at or above
/// [`PAYLOAD_TRANSFER_MIN_PORT`], mirroring the upstream sender behavior.
///
/// Ports below the floor are never used for payloads, so a KDE Connect peer
/// opening its own listener on the same machine will not collide with ours.
async fn bind_payload_listener() -> Result<tokio::net::TcpListener> {
    let last = PAYLOAD_TRANSFER_MIN_PORT.saturating_add(1024);
    for port in PAYLOAD_TRANSFER_MIN_PORT..=last {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).await {
            Ok(listener) => return Ok(listener),
            Err(_) => continue,
        }
    }
    Err(Error::Other(format!(
        "no free payload port in [{PAYLOAD_TRANSFER_MIN_PORT}..{last}]"
    )))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
