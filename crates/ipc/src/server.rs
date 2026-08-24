//! IPC server: socket accept loop, per-client sessions and event fanout.
//!
//! [`Server::serve`] runs forever: it accepts connections, verifies each peer
//! (Linux: `SO_PEERCRED` uid check), sends a [`ServerEvent::Hello`] handshake
//! and then services the client with two concurrent loops — a read loop that
//! dispatches [`Request`]s through the caller-supplied handler, and a write
//! loop that serializes responses plus every [`ServerEvent`] broadcast.

use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::broadcast;

#[cfg(unix)]
use crate::codec::{write_frame, FrameReader};
use crate::error::{Error, Result};
use crate::proto::{Request, Response, ServerEvent};
#[cfg(unix)]
use crate::IPC_VERSION;

/// How long to pause after a failed accept before retrying.
#[cfg(unix)]
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Callback turning a decoded request into an asynchronous response.
pub type RequestHandler = Arc<dyn Fn(Request) -> BoxFuture<'static, Response> + Send + Sync>;

/// Lock a possibly poisoned mutex, recovering the guard regardless of poison.
#[cfg(unix)]
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// IPC listener bound to `socket_path`; call [`Server::serve`] to run it.
pub struct Server {
    /// Endpoint the listener was bound to.
    pub socket_path: PathBuf,
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
}

impl Server {
    /// Bind a Unix domain socket at `path`, removing any stale socket file
    /// left behind by a previous daemon and restricting permissions to owner
    /// only (`0600`).
    #[cfg(unix)]
    pub async fn bind(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        match tokio::fs::remove_file(path).await {
            Ok(()) => {
                tracing::debug!(
                    target: "handfast::ipc",
                    path = %path.display(),
                    "removed stale socket file"
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        let listener = tokio::net::UnixListener::bind(path)?;
        set_owner_only(path);

        Ok(Self {
            socket_path: path.to_path_buf(),
            listener,
        })
    }

    /// Windows/named-pipe transport is not implemented yet; always fails.
    #[cfg(not(unix))]
    pub async fn bind(path: &Path) -> Result<Self> {
        Err(Error::Other(format!(
            "IPC transport unavailable on this platform \
             (named-pipe support pending; requested {path:?})"
        )))
    }

    /// Serve clients until the process exits.
    ///
    /// Each accepted connection gets a [`ServerEvent::Hello`] followed by
    /// concurrent request dispatch (`handler`) and broadcast-event fanout from
    /// `events`. On Linux, peers whose uid differs from the daemon's effective
    /// uid are closed immediately.
    #[cfg(unix)]
    pub async fn serve(
        self,
        handler: RequestHandler,
        events: broadcast::Receiver<ServerEvent>,
    ) -> Result<()> {
        let registry: Arc<std::sync::Mutex<Vec<tokio::sync::mpsc::UnboundedSender<Outbound>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let dispatcher_registry = registry.clone();
        let _dispatcher = tokio::spawn(dispatch_events(events, dispatcher_registry));

        tracing::info!(target: "handfast::ipc", path = %self.socket_path.display(), "accepting connections");
        loop {
            match self.listener.accept().await {
                Ok((stream, _addr)) => {
                    let handler = handler.clone();
                    let registry = registry.clone();
                    let _session = tokio::spawn(handle_connection(stream, handler, registry));
                }
                Err(err) => {
                    tracing::warn!(
                        target: "handfast::ipc",
                        %err,
                        "accept failed; retrying"
                    );
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                }
            }
        }
    }

    /// Windows/named-pipe transport is not implemented yet; always fails.
    #[cfg(not(unix))]
    pub async fn serve(
        self,
        handler: RequestHandler,
        events: broadcast::Receiver<ServerEvent>,
    ) -> Result<()> {
        let _ = (handler, events);
        Err(Error::Other(
            "IPC transport requires Unix domain sockets; named-pipe support pending".to_string(),
        ))
    }
}

/// Restrict the socket file to owner access. Best effort: exotic filesystems
/// may reject chmod, in which case directory permissions still protect it.
#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            target: "handfast::ipc",
            %err,
            path = %path.display(),
            "could not chmod socket to 0600"
        );
    }
}

/// Anything queued toward one client.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
#[cfg(unix)]
enum Outbound {
    /// A reply to a specific request.
    Response(Response),
    /// A broadcast event.
    Event(ServerEvent),
}

/// Registry of per-client outbound queues used by the broadcast relay.
#[cfg(unix)]
type ClientSinks = Arc<std::sync::Mutex<Vec<tokio::sync::mpsc::UnboundedSender<Outbound>>>>;

/// Relay broadcast events into every live client's write queue.
///
/// Clients whose queue failed (disconnected) are pruned on each delivery.
#[cfg(unix)]
async fn dispatch_events(mut events: broadcast::Receiver<ServerEvent>, sinks: ClientSinks) {
    loop {
        match events.recv().await {
            Ok(event) => {
                let message = Outbound::Event(event);
                lock(&sinks).retain(|tx| tx.send(message.clone()).is_ok());
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::debug!(
                    target: "handfast::ipc",
                    skipped,
                    "event relay lagged behind producers"
                );
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Per-connection session: credential check, handshake, concurrent IO loops.
#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    handler: RequestHandler,
    sinks: ClientSinks,
) -> Result<()> {
    // Linux: reject cross-user connections before touching them further.
    #[cfg(target_os = "linux")]
    {
        if !crate::peercred::peer_is_owner(&stream) {
            return Ok(());
        }
    }

    tracing::info!(target: "handfast::ipc", "client accepted; credentials verified");
    let (mut read_half, write_half) = stream.into_split();
    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<Outbound>();

    // Writer task: the single serializer of everything this client receives.
    // Spawned FIRST so the handshake below flows through the same queue as
    // every later frame — one writer, one ordering, no inline-write races.
    let writer = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(message) = outbound_rx.recv().await {
            let kind = match &message {
                Outbound::Response(r) => format!("response:{r:?}"),
                Outbound::Event(e) => format!("event:{e:?}"),
            };
            if write_frame(&mut write_half, &message).await.is_err() {
                tracing::info!(target: "handfast::ipc", %kind, "outbound write failed; closing");
                break;
            }
            tracing::trace!(target: "handfast::ipc", %kind, "outbound written");
        }
    });

    // Register for broadcasts before the handshake so nothing races in.
    lock(&sinks).push(outbound_tx.clone());

    let hello = Outbound::Event(ServerEvent::Hello {
        version: IPC_VERSION,
        app: handfast_core::APP_NAME.to_string(),
        pid: std::process::id(),
    });
    if outbound_tx.send(hello).is_err() {
        // Writer already gone: nothing else to do; read half errors out below.
        tracing::warn!(target: "handfast::ipc", "hello enqueue failed; closing session");
        return Ok(());
    }

    // Read loop: decode requests, dispatch, queue responses. Stateful
    // decoder so pipelined requests from a client are queued, not dropped.
    let mut frames = FrameReader::new();
    let read_result: Result<()> = loop {
        let decoded = frames.next(&mut read_half).await;
        match decoded {
            Ok(payload) => {
                let request: Request = match serde_json::from_slice(&payload) {
                    Ok(req) => req,
                    Err(err) => break Err(err.into()),
                };
                tracing::info!(target: "handfast::ipc", method = ?request, "request received");
                let response = (handler)(request).await;
                tracing::info!(target: "handfast::ipc", "response queued");
                if outbound_tx.send(Outbound::Response(response)).is_err() {
                    break Err(Error::Closed);
                }
            }
            Err(Error::Closed) => break Ok(()),
            Err(err) => break Err(err),
        }
    };

    // Tear down: unregister from broadcasts first, then drop our sender so the
    // writer observes channel closure and exits once remaining frames drain.
    lock(&sinks).retain(|tx| !tx.same_channel(&outbound_tx));
    drop(outbound_tx);
    drop(read_half);
    let _ = writer.await;

    read_result
}
