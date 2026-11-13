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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use handfast_desktop::Desktop;
    use handfast_proto::{types, Capabilities, DeviceName, DeviceType};
    use handfast_runtime::{
        DesktopRequirement, LogLevel, Plugin, PluginContext, PluginError, PluginEvent,
        PluginFactory, PluginMeta, PluginOutput, PluginStatus, Registry,
    };

    static RECORDING: PluginMeta = PluginMeta {
        name: "recording",
        title: "Recording",
        status: PluginStatus::Implemented,
        incoming: &[],
        outgoing: &[],
        desktop: DesktopRequirement::None,
    };

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
        let peer = Identity::new(
            DeviceId::new("00112233445566778899aabbccddeeff").expect("a valid test ID"),
            DeviceName::new("Recording peer"),
            DeviceType::Phone,
            Capabilities::default(),
            Some(1716),
        );
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
            other => panic!("expected plugin events, got {other:?}"),
        }
    }
}
