//! One plugin host per paired connected device.
//!
//! The session owns the [`DeviceHost`], dispatches inbound packets, forwards
//! plugin output to the link and plugin events to the daemon, and runs
//! `on_disconnect` before it exits. The daemon event loop owns the session map;
//! the manager never sees a `DeviceHost`.

use std::sync::Arc;

use handfast_desktop::Desktop;
use handfast_net::LinkHandle;
use handfast_proto::{DeviceId, Identity, Packet};
use handfast_runtime::{Device, DeviceHost, Dispatch, PluginEvent, Registry};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Inbound queue for one session. A full queue drops packets rather than
/// blocking the daemon event loop.
const SESSION_QUEUE: usize = 64;

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

/// A running session and its handle.
pub struct DeviceSession {
    task: JoinHandle<()>,
    handle: SessionHandle,
}

impl DeviceSession {
    /// Start a session for an authenticated, paired link.
    pub fn start(
        peer: Identity,
        link: LinkHandle,
        registry: Arc<Registry>,
        desktop: Desktop,
        events: mpsc::Sender<DaemonEvent>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(SESSION_QUEUE);
        let handle = SessionHandle { tx };
        let task = tokio::spawn(run(peer, link, registry, desktop, events, rx));
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

/// Input to a session task.
pub enum SessionInput {
    /// One inbound packet.
    Packet(Packet),
    /// The link closed or the daemon is shutting down.
    Disconnected(String),
}

async fn run(
    peer: Identity,
    link: LinkHandle,
    registry: Arc<Registry>,
    desktop: Desktop,
    events: mpsc::Sender<DaemonEvent>,
    mut input: mpsc::Receiver<SessionInput>,
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

    let dispatch = host.on_connect().await;
    send_dispatch(&link, &events, &device_id, dispatch).await;

    let reason = loop {
        match input.recv().await {
            Some(SessionInput::Packet(packet)) => {
                let dispatch = host.dispatch(&packet).await;
                send_dispatch(&link, &events, &device_id, dispatch).await;
            }
            Some(SessionInput::Disconnected(reason)) => break reason,
            None => break "session channel closed".to_string(),
        }
    };

    tracing::debug!(%device_id, %reason, "device session closing");
    let dispatch = host.on_disconnect().await;
    send_dispatch(&link, &events, &device_id, dispatch).await;
}

async fn send_dispatch(
    link: &LinkHandle,
    events: &mpsc::Sender<DaemonEvent>,
    device_id: &DeviceId,
    dispatch: Dispatch,
) {
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
}
