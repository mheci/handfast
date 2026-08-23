//! IPC client for talking to the Handfast daemon.
//!
//! A [`Client`] owns one connection. [`Client::request`] serializes access to
//! the socket's write half (responses are matched back by arrival order, which
//! the single serialized writer guarantees) while a background reader task
//! demultiplexes incoming frames: [`ServerEvent`]s go to an unbounded channel
//! retrievable via [`Client::take_event_receiver`], responses resolve the
//! matching in-flight request.
//!
//! Clients are cheap to clone; clones share the connection and its event feed
//! but only one holder can claim the event receiver.

use std::path::Path;

use crate::error::{Error, Result};
use crate::proto::{Request, Response, ServerEvent};

#[cfg(unix)]
mod imp {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, oneshot};

    use crate::codec::{read_raw_frame, write_frame};

    /// Lock a possibly poisoned mutex, recovering the guard regardless of
    /// poison.
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waiter slot for one in-flight request.
    type ReplySender = oneshot::Sender<Result<Response>>;

    /// FIFO of waiters whose requests were written but not yet answered.
    type PendingQueue = Arc<Mutex<VecDeque<ReplySender>>>;

    /// Shared connection state.
    pub(super) struct Connection {
        /// Serialized access to the socket's write half.
        write: tokio::sync::Mutex<OwnedWriteHalf>,
        /// Waiters queued in wire order (guarded together with `write`).
        pending: PendingQueue,
        /// Set once the reader exits or a write fails hard; later requests
        /// fail fast instead of hanging forever.
        dead: Arc<AtomicBool>,
    }

    impl Connection {
        /// Fail every outstanding request; used when the connection dies.
        fn fail_all_pending(&self) {
            self.dead.store(true, Ordering::SeqCst);
            for waiter in lock(&self.pending).drain(..) {
                let _ = waiter.send(Err(Error::Closed));
            }
        }
    }

    /// Connection to the daemon's IPC endpoint.
    pub struct Client {
        conn: Arc<Connection>,
        events: Option<mpsc::UnboundedReceiver<ServerEvent>>,
    }

    // Cloned clients share the connection; the event receiver stays unique to
    // the original handle (see `take_event_receiver`).
    impl Clone for Client {
        fn clone(&self) -> Self {
            Self {
                conn: Arc::clone(&self.conn),
                events: None,
            }
        }
    }

    impl Client {
        /// Connect to the daemon socket at `path` and start the background
        /// reader task.
        ///
        /// The daemon immediately queues a [`ServerEvent::Hello`]; retrieve it
        /// (and all later events) via
        /// [`take_event_receiver`](Client::take_event_receiver).
        pub async fn connect(path: &Path) -> Result<Self> {
            let stream = UnixStream::connect(path).await?;
            let (read_half, write_half) = stream.into_split();

            let (events_tx, events_rx) = mpsc::unbounded_channel();
            let pending: PendingQueue = Arc::new(Mutex::new(VecDeque::new()));
            let dead = Arc::new(AtomicBool::new(false));

            let _reader = tokio::spawn(reader_loop(
                read_half,
                Arc::clone(&pending),
                Arc::clone(&dead),
                events_tx,
            ));

            Ok(Self {
                conn: Arc::new(Connection {
                    write: tokio::sync::Mutex::new(write_half),
                    pending,
                    dead,
                }),
                events: Some(events_rx),
            })
        }

        /// Send one request and await its response.
        ///
        /// Requests from concurrent callers are written one at a time; each
        /// response is paired with the oldest unanswered request, preserving
        /// order. Fails with [`Error::Closed`] once the connection is gone.
        pub async fn request(&self, req: Request) -> Result<Response> {
            if self.conn.dead.load(Ordering::SeqCst) {
                return Err(Error::Closed);
            }

            let (reply_tx, reply_rx) = oneshot::channel();

            // Enqueue *while holding* the write lock so queue order always
            // matches wire order, even across concurrent callers.
            let mut write_half = self.conn.write.lock().await;
            lock(&self.conn.pending).push_back(reply_tx);
            let write_result = write_frame(&mut *write_half, &req).await;
            drop(write_half);

            match write_result {
                Ok(()) => {}
                Err(err) => {
                    // The connection is unusable; fail everything queued.
                    self.conn.fail_all_pending();
                    return Err(err);
                }
            }

            if self.conn.dead.load(Ordering::SeqCst) {
                // The reader exited between enqueue and now; depending on
                // interleaving its final drain may have missed us.
                return Err(Error::Closed);
            }

            // Bounded wait: a wedged daemon must surface as an error instead of
            // hanging callers (GUI/TUI stay responsive; ops get a signal).
            match tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_dropped_without_reply)) => Err(Error::Closed),
                Err(_elapsed) => {
                    self.conn.fail_all_pending();
                    Err(Error::Other("ipc request timed out after 10s".into()))
                }
            }
        }

        /// Take ownership of the server-event stream.
        ///
        /// Returns the receiver exactly once per connection (`None`
        /// afterwards, including on clones). Events published before this call
        /// remain buffered inside.
        pub fn take_event_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<ServerEvent>> {
            self.events.take()
        }
    }

    /// Background demultiplexer: routes frames either to event subscribers or
    /// to the oldest pending request. On exit every outstanding request is
    /// failed so its caller sees [`Error::Closed`] instead of hanging.
    async fn reader_loop(
        mut read_half: OwnedReadHalf,
        pending: PendingQueue,
        dead: Arc<AtomicBool>,
        events_tx: mpsc::UnboundedSender<ServerEvent>,
    ) {
        loop {
            let payload = match read_raw_frame(&mut read_half).await {
                Ok(payload) => payload,
                Err(err) => {
                    tracing::info!(target: "handfast::ipc", %err, "client reader: frame read ended");
                    break;
                }
            };
            tracing::trace!(target: "handfast::ipc", bytes = payload.len(), "client reader: frame received");

            if let Ok(event) = serde_json::from_slice::<ServerEvent>(&payload) {
                tracing::trace!(target: "handfast::ipc", "client reader: routed event");
                let _ = events_tx.send(event);
                continue;
            }

            match serde_json::from_slice::<Response>(&payload) {
                Ok(response) => match lock(&pending).pop_front() {
                    Some(reply) => {
                        let _ = reply.send(Ok(response));
                    }
                    None => {
                        tracing::debug!(
                            target: "handfast::ipc",
                            "response arrived with no pending request; dropped"
                        );
                        break;
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        target: "handfast::ipc",
                        %err,
                        "undecodable frame from daemon; closing connection"
                    );
                    break;
                }
            }
        }

        dead.store(true, Ordering::SeqCst);
        for waiter in lock(&pending).drain(..) {
            let _ = waiter.send(Err(Error::Closed));
        }
    }
}

// Unix implementation.
#[cfg(unix)]
pub use imp::Client;

// Non-Unix stub: everything compiles, nothing connects until named-pipe
// transport support is implemented.
#[cfg(not(unix))]
impl Client {
    /// Always fails on this platform; Unix domain sockets do not exist here.
    pub async fn connect(path: &Path) -> Result<Self> {
        Err(Error::Other(format!(
            "IPC transport unavailable on this platform \
             (named-pipe support pending; requested {path:?})"
        )))
    }

    /// Always fails on this platform; there is no connection.
    pub async fn request(&self, _req: Request) -> Result<Response> {
        Err(Error::Other(
            "IPC transport requires Unix domain sockets; named-pipe support pending".to_string(),
        ))
    }

    /// Always returns `None` on this platform.
    #[must_use]
    pub fn take_event_receiver(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<ServerEvent>> {
        None
    }
}

/// Connection to the daemon's IPC endpoint.
///
/// On Unix this wraps a framed `UnixStream` (see module docs); other platforms
/// get a stub whose methods always fail until named-pipe support lands.
#[cfg(not(unix))]
#[derive(Clone)]
pub struct Client;
