//! `handfastd` — the Handfast device-pairing daemon.
//!
//! Phase 1 wiring: XDG paths, SQLite state, certificate identity, the IPC
//! server and a supervision tree that restarts any crashed subsystem with
//! exponential backoff. Discovery, pairing and plugins land in Phases 2-3;
//! their supervised tasks are already stubbed in below so the tree shape is
//! final from day one.
//!
//! # Runtime
//!
//! A multi-threaded tokio runtime drives everything; on linux-gnu targets the
//! process allocates through jemalloc (`--features jemalloc`, enabled by
//! packaging), everywhere else through mimalloc.
//!
//! # Wayland-first
//!
//! The daemon is the only process in the Handfast suite that talks to the
//! display server. It detects the session type at startup via
//! [`handfast_wayland::detect_session`] and logs the result; input injection,
//! clipboard watching and idle inhibition all live behind that bridge.

#![deny(clippy::unwrap_used)]
#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use futures_util::future::BoxFuture;
use handfast_core::bus::{Bus, Event};
use handfast_core::paths::Paths;
use handfast_core::store::{DeviceRow, Store};
use handfast_core::supervise::Supervisor;
use handfast_ipc::{Request, RequestHandler, Response, ServerEvent};
use tokio::sync::broadcast;

/// Global allocator: jemalloc where packaging enables it (linux-gnu), mimalloc elsewhere.
#[cfg(all(target_os = "linux", target_env = "gnu", feature = "jemalloc"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(not(all(target_os = "linux", target_env = "gnu", feature = "jemalloc")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Command line surface (kept minimal; the control planes are `hfctl`/GUI).
#[derive(Debug, Parser)]
#[command(
    name = "handfastd",
    version,
    about = "Handfast device-pairing daemon"
)]
struct Args {
    /// Override the IPC socket path (default: $XDG_RUNTIME_DIR/handfast.sock).
    #[arg(long)]
    socket: Option<std::path::PathBuf>,
    /// Override the state directory (default: $XDG_DATA_HOME/handfast).
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    install_panic_hook();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("handfastd-worker")
        .build()
        .context("building tokio runtime")?;
    runtime.block_on(run(args))
}

/// Install a panic hook that logs before the default handler prints.
///
/// Supervised tasks catch panics through their `JoinHandle`; this hook only
/// adds structured logging for post-mortems.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "handfast::panic", %info, "panic caught");
        previous(info);
    }));
}

async fn run(args: Args) -> anyhow::Result<()> {
    let paths = Paths::init().context("resolving XDG directories")?;
    let db_path = match args.data_dir {
        Some(dir) => dir.join("state.db3"),
        None => paths.data.join("state.db3"),
    };
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let store = Store::open(&db_path)
        .with_context(|| format!("opening state database at {}", db_path.display()))?;
    let store = Arc::new(store);

    // Stable per-install device id; regenerated only if the store is wiped.
    let device_id = ensure_device_id(&store).context("ensuring device identity")?;

    // TLS identity: self-signed cert with CN = device_id, pinned by fingerprint.
    let _cert = handfast_protocol::tls::CertPair::load_or_generate(&paths.config, &device_id)
        .context("loading or generating TLS identity")?;

    let session = handfast_wayland::detect_session();
    tracing::info!(
        kind = ?session.kind,
        compositor = session.compositor.unwrap_or("unknown"),
        "session detected"
    );

    let bus = Bus::new();
    let supervisor = Supervisor::new(bus.clone());

    // Bridge core bus events into the IPC broadcast channel.
    let (ipc_events, _) = broadcast::channel::<ServerEvent>(1024);
    {
        let mut rx = bus.subscribe();
        let tx = ipc_events.clone();
        supervisor.spawn("event-forwarder", move || {
            let mut rx = rx.resubscribe();
            let tx = tx.clone();
            async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let _lagged = tx.send(ServerEvent::from(ev));
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!(skipped = n, "ipc event bridge lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Ok(());
                        }
                    }
                }
            }
        });
    }

    // IPC control plane.
    let handler: RequestHandler = build_handler(store.clone(), device_id.clone());
    let socket_path = args.socket.clone().unwrap_or_else(handfast_ipc::default_socket_path);
    {
        let socket_path = socket_path.clone();
        supervisor.spawn("ipc-server", move || {
            let handler = handler.clone();
            let events = ipc_events.clone();
            async move {
                let server = handfast_ipc::Server::bind(&socket_path).await?;
                sd_notify_ready();
                tracing::info!(path = %socket_path.display(), "ipc listening");
                server.serve(handler, events).await
            }
        });
    }

    // Discovery — replaced by real UDP/TLS discovery in Phase 2.
    {
        let shutdown = ShutdownListener::new();
        supervisor.spawn("discovery", move || {
            let mut shutdown = shutdown.clone();
            async move {
                tracing::info!("discovery stub active until Phase 2");
                shutdown.wait().await;
                Ok(())
            }
        });
    }

    tracing::info!(%device_id, plugins = handfast_plugins::registry().len(), "handfastd ready");

    wait_for_shutdown().await;
    tracing::info!("shutting down");
    bus.publish(Event::DaemonShutdown);
    supervisor.shutdown_all().await;
    cleanup_socket(&socket_path).await;
    Ok(())
}

/// Load-or-create the persistent device id.
fn ensure_device_id(store: &Store) -> anyhow::Result<String> {
    if let Some(existing) = store.kv_get("device_id").context("reading device_id")? {
        return Ok(existing);
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    store.kv_set("device_id", &fresh).context("storing device_id")?;
    Ok(fresh)
}

/// Build the phase-1 request handler backed by SQLite and the wayland bridge.
fn build_handler(store: Arc<Store>, device_id: String) -> RequestHandler {
    Arc::new(move |req: Request| -> BoxFuture<'static, Response> {
        let store = store.clone();
        let device_id = device_id.clone();
        Box::pin(async move { handle_request(req, store, device_id).await })
    })
}

async fn handle_request(req: Request, store: Arc<Store>, device_id: String) -> Response {
    match req {
        Request::Ping => Response::ok_json(serde_json::json!("pong")),
        Request::DaemonInfo => {
            let session = handfast_wayland::detect_session();
            Response::ok_json(serde_json::json!({
                "app": handfast_core::APP_NAME,
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": handfast_protocol::PROTO_VERSION,
                "ipc": handfast_ipc::IPC_VERSION,
                "device_id": device_id,
                "session": {
                    "kind": format!("{:?}", session.kind),
                    "compositor": session.compositor,
                    "protocols": session.protocols,
                },
            }))
        }
        Request::DeviceList => match store.list_devices() {
            Ok(rows) => {
                let rows: Vec<serde_json::Value> =
                    rows.iter().map(device_row_json).collect();
                Response::ok_json(serde_json::Value::Array(rows))
            }
            Err(err) => Response::err(1000, err.to_string()),
        },
        Request::DevicePair { device_id } => set_paired(&store, &device_id, true).await,
        Request::DeviceUnpair { device_id } => set_paired(&store, &device_id, false).await,
        Request::PluginList { .. } => {
            let plugins: Vec<serde_json::Value> = handfast_plugins::registry()
                .iter()
                .map(|factory| {
                    let meta = factory.meta();
                    serde_json::json!({
                        "name": meta.name,
                        "title": meta.title,
                        "default_enabled": meta.default_enabled,
                        "requires_wayland": meta.requires_wayland,
                        "requires_dbus": meta.requires_dbus,
                    })
                })
                .collect();
            Response::ok_json(serde_json::Value::Array(plugins))
        }
        Request::PluginSetEnabled { device_id, plugin, enabled } => {
            let key = format!("plugin:{device_id}:{plugin}");
            match store.kv_set(&key, if enabled { "1" } else { "0" }) {
                Ok(()) => Response::ok_json(serde_json::json!({"saved": true})),
                Err(err) => Response::err(3000, err.to_string()),
            }
        }
        Request::SendFile { .. } => {
            Response::err(4000, "file transfers arrive in Phase 3")
        }
        Request::NotificationList => Response::ok_json(serde_json::Value::Array(vec![])),
        Request::NotificationDismiss { notification_id } => {
            tracing::debug!(%notification_id, "notification dismiss is a no-op until Phase 3");
            Response::ok_json(serde_json::json!({"dismissed": true}))
        }
        Request::ClipboardGet => match handfast_wayland::clipboard::Clipboard::get_text() {
            Ok(text) => Response::ok_json(serde_json::json!({ "text": text })),
            Err(err) => Response::err(5000, err.to_string()),
        },
        Request::ClipboardSet { text } => {
            match handfast_wayland::clipboard::Clipboard::set_text(&text) {
                Ok(()) => Response::ok_json(serde_json::json!({"set": true})),
                Err(err) => Response::err(5000, err.to_string()),
            }
        }
    }
}

fn device_row_json(row: &DeviceRow) -> serde_json::Value {
    serde_json::json!({
        "device_id": row.device_id,
        "name": row.name,
        "type": row.device_type,
        "paired": row.paired,
        "last_seen": row.last_seen,
    })
}

async fn set_paired(store: &Arc<Store>, device_id: &str, paired: bool) -> Response {
    let rows = match store.list_devices() {
        Ok(rows) => rows,
        Err(err) => return Response::err(2000, err.to_string()),
    };
    let Some(mut row) = rows.into_iter().find(|row| row.device_id == device_id) else {
        return Response::err(2004, format!("unknown device '{device_id}'"));
    };
    row.paired = paired;
    if let Err(err) = store.upsert_device(&row) {
        return Response::err(2000, err.to_string());
    }
    Response::ok_json(serde_json::json!({"paired": paired}))
}

/// Shutdown signal listener shared by supervised stub tasks.
#[derive(Clone)]
struct ShutdownListener(Arc<tokio::sync::Notify>);

impl ShutdownListener {
    fn new() -> Self {
        Self(Arc::new(tokio::sync::Notify::new()))
    }
    async fn wait(&mut self) {
        self.0.notified().await;
    }
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        let mut sigint = signal(SignalKind::interrupt()).ok();
        tokio::select! {
            _ = async {
                match sigterm.as_mut() {
                    Some(sigterm) => { sigterm.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
            _ = async {
                match sigint.as_mut() {
                    Some(sigint) => { sigint.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Best-effort systemd readiness notification (`Type=notify` compatible).
///
/// Writes `READY=1` to `$NOTIFY_SOCKET` when running under systemd with
/// notify-type supervision; silently does nothing otherwise. Safe code only:
/// an unbound datagram socket to the abstract/filesystem socket path.
fn sd_notify_ready() {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixDatagram;
        let Some(socket_path) = std::env::var_os("NOTIFY_SOCKET") else {
            return;
        };
        let socket_path = std::path::PathBuf::from(socket_path);
        // Abstract namespace sockets start with '@' and map to a leading NUL byte.
        let addr_is_abstract = socket_path
            .as_os_str()
            .to_str()
            .is_some_and(|p| p.starts_with('@'));
        let path = if addr_is_abstract {
            std::path::PathBuf::from(format!("\0{}", socket_path.display()))
        } else {
            socket_path
        };
        if let Ok(sock) = UnixDatagram::unbound() {
            if sock.connect(&path).is_ok() {
                let _sent = sock.send(b"READY=1");
            }
        }
    }
}

async fn cleanup_socket(path: &std::path::Path) {
    #[cfg(unix)]
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(%err, "failed to remove socket file"),
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}
