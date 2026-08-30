#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! `handfastd` — the Handfast device-pairing daemon.
//!
//! Phase 2 wiring: full KDE Connect-compatible networking — UDP broadcast
//! discovery, TLS connections with certificate pinning, identity exchange,
//! pairing, and a device-manager actor that routes packets through plugins.
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
//! [`handfast_wayland::detect_session`]; input injection, clipboard watching
//! and idle inhibition all live behind that bridge.

#![deny(clippy::unwrap_used)]
#![forbid(unsafe_code)]

mod backends;
mod dbus;
mod device;
mod discovery;
mod handshake;
mod payload;
#[allow(dead_code)] // remote-filesystem exposure arrives with sftp wiring (Phase 4)
mod sftp;
mod tls;
mod transfer;

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use futures_util::future::BoxFuture;
use handfast_core::bus::{Bus, Event};
use handfast_core::error::Error as CoreError;
use handfast_core::error::Result as CoreResult;
use handfast_core::paths::Paths;
use handfast_core::store::Store;
use handfast_core::supervise::Supervisor;
use handfast_ipc::{Request, RequestHandler, Response, ServerEvent};
use handfast_protocol::{Identity, DEFAULT_TCP_PORT, PROTO_VERSION};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::device::{Command, Manager};

/// Global allocator policy:
/// * --features jemalloc -> jemalloc (linux-gnu packaging default),
/// * --features mimalloc -> mimalloc,
/// * neither -> the platform's system allocator (distro-friendly default).
#[cfg(all(target_os = "linux", target_env = "gnu", feature = "jemalloc"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(not(feature = "jemalloc"), feature = "mimalloc"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Command line surface (kept minimal; the control planes are `hfctl`/GUI).
#[derive(Debug, Parser)]
#[command(name = "handfastd", version, about = "Handfast device-pairing daemon")]
struct Args {
    /// Override the IPC socket path (default: $XDG_RUNTIME_DIR/handfast/handfast.sock).
    #[arg(long)]
    socket: Option<std::path::PathBuf>,
    /// Override the state directory (default: $XDG_DATA_HOME/handfast).
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Display name advertised to peers.
    #[arg(long, default_value = "Handfast Desktop")]
    name: String,
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
    let store = Arc::new(
        Store::open(&db_path)
            .with_context(|| format!("opening state database at {}", db_path.display()))?,
    );

    // Stable per-install device id; regenerated only if the store is wiped.
    let device_id = ensure_device_id(&store).context("ensuring device identity")?;

    // TLS identity: self-signed cert with CN = device id, pinned by fingerprint.
    let cert = Arc::new(
        handfast_protocol::tls::CertPair::load_or_generate(&paths.config, &device_id)
            .context("loading or generating TLS identity")?,
    );

    let session = handfast_wayland::detect_session();
    tracing::info!(
        kind = ?session.kind,
        compositor = session.compositor.unwrap_or("unknown"),
        "session detected"
    );

    let self_identity = build_self_identity(&device_id, &args.name);
    let bus = Bus::new();
    let supervisor = Supervisor::new(bus.clone());

    // Bridge core bus events into the IPC broadcast channel.
    let (ipc_events, _) = broadcast::channel::<ServerEvent>(1024);
    {
        let rx = bus.subscribe();
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
                            return CoreResult::Ok(());
                        }
                    }
                }
            }
        });
    }

    // Device manager actor — owns pairing state, pinning, plugin dispatch.
    let factories = handfast_plugins::registry();
    let (manager_handle, manager) = Manager::new(
        store.clone(),
        bus.clone(),
        device_id.clone(),
        self_identity.clone(),
        cert.clone(),
        factories,
    );
    {
        // The channel cannot be rebuilt after an actor crash (handles would go
        // stale), so supervision runs exactly one real lifecycle; afterwards
        // retries end supervision quietly instead of crash-looping uselessly.
        let cell = Arc::new(std::sync::Mutex::new(Some(manager)));
        supervisor.spawn("devices", move || {
            let manager = lock_cell(&cell).take();
            async move {
                match manager {
                    Some(mut manager) => {
                        manager.load_persisted().await?;
                        manager.run().await
                    }
                    None => CoreResult::Ok(()),
                }
            }
        });
    }

    // Platform listeners: battery and notification feeds publish into the bus.
    // Each parks harmlessly when its backing service (sysfs/DBus/UPower) is
    // absent, per the resilience contract in `dbus.rs`.
    {
        let bus_battery = bus.clone();
        supervisor.spawn("battery-sysfs", move || {
            let bus = bus_battery.clone();
            async move { dbus::battery_monitor(bus).await }
        });
        let bus_upower = bus.clone();
        supervisor.spawn("battery-upower", move || {
            let bus = bus_upower.clone();
            async move { dbus::upower_battery(bus).await }
        });
        let bus_notifs = bus.clone();
        supervisor.spawn("notifications", move || {
            let bus = bus_notifs.clone();
            async move { dbus::notifications_listener(bus).await }
        });
    }

    // UDP broadcast discovery (announce + listen in one supervised loop).
    // Announcements flow through their own channel into a small bridge that
    // converts them into manager commands.
    {
        let (announcements_tx, announcements_rx) =
            tokio::sync::mpsc::channel::<discovery::PeerAnnouncement>(64);
        let identity = self_identity.clone();
        let self_device_id = device_id.clone();
        supervisor.spawn("discovery", move || {
            let identity = identity.clone();
            let self_device_id = self_device_id.clone();
            let tx = announcements_tx.clone();
            async move {
                let socket = discovery::bind().await?;
                discovery::run(socket, identity, self_device_id, tx).await
            }
        });
        // Single-consumer receiver handed over via a cell so the `Fn` factory
        // stays callable across supervision attempts.
        let rx_cell = Arc::new(std::sync::Mutex::new(Some(announcements_rx)));
        let tx = manager_handle.command_sender();
        supervisor.spawn("announcement-bridge", move || {
            let announcements_rx = lock_cell(&rx_cell).take();
            let tx = tx.clone();
            async move {
                let Some(mut announcements_rx) = announcements_rx else {
                    return Ok(());
                };
                while let Some(announcement) = announcements_rx.recv().await {
                    if tx.send(Command::Announce(announcement)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            }
        });
    }

    // TCP/TLS listener for inbound connections on the well-known port.
    {
        let pair = cert.clone();
        let mine = self_identity.clone();
        let tx = manager_handle.command_sender();
        supervisor.spawn("tcp-listener", move || {
            let pair = pair.clone();
            let mine = mine.clone();
            let tx = tx.clone();
            async move {
                let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, DEFAULT_TCP_PORT)).await?;
                tracing::info!(port = DEFAULT_TCP_PORT, "tcp/tls listening");
                loop {
                    let (tcp, peer) = listener.accept().await?;
                    let pair = pair.clone();
                    let mine = mine.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        match async { handshake::accept_control(tcp, pair, &mine).await }.await {
                            Ok((remote, transport)) => {
                                let _ = tx
                                    .send(Command::Connected {
                                        device_id: remote.device_id,
                                        transport,
                                    })
                                    .await;
                            }
                            Err(err) => {
                                tracing::debug!(%err, %peer, "inbound handshake failed")
                            }
                        }
                    });
                }
            }
        });
    }

    // IPC control plane backed by live manager queries.
    let handler: RequestHandler =
        build_handler(store.clone(), manager_handle.clone(), device_id.clone());
    let socket_path = args
        .socket
        .clone()
        .unwrap_or_else(handfast_ipc::default_socket_path);
    {
        let socket_path = socket_path.clone();
        supervisor.spawn("ipc-server", move || {
            let handler = handler.clone();
            let socket_path = socket_path.clone();
            let events = ipc_events.subscribe();
            async move {
                let server = handfast_ipc::Server::bind(&socket_path)
                    .await
                    .map_err(|err| CoreError::Other(err.to_string()))?;
                sd_notify_ready();
                tracing::info!(path = %socket_path.display(), "ipc listening");
                server
                    .serve(handler, events)
                    .await
                    .map_err(|err| CoreError::Other(err.to_string()))
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

/// Advertised identity: capabilities aggregated from every registered plugin.
fn build_self_identity(device_id: &str, name: &str) -> Identity {
    let mut incoming: BTreeSet<String> = BTreeSet::new();
    let mut outgoing: BTreeSet<String> = BTreeSet::new();
    for factory in handfast_plugins::registry() {
        let meta = factory.meta();
        incoming.extend(meta.incoming.iter().map(|s| (*s).to_string()));
        outgoing.extend(meta.outgoing.iter().map(|s| (*s).to_string()));
    }
    Identity {
        device_id: device_id.to_string(),
        name: name.to_string(),
        device_type: "desktop".into(),
        protocol_version: PROTO_VERSION,
        incoming: incoming.into_iter().collect(),
        outgoing: outgoing.into_iter().collect(),
        tcp_source_port: DEFAULT_TCP_PORT,
    }
}

/// Lock a possibly poisoned mutex, recovering the guard regardless of poison.
fn lock_cell<T>(cell: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Load-or-create the persistent device id.
fn ensure_device_id(store: &Store) -> anyhow::Result<String> {
    if let Some(existing) = store.kv_get("device_id").context("reading device_id")? {
        return Ok(existing);
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    store
        .kv_set("device_id", &fresh)
        .context("storing device_id")?;
    Ok(fresh)
}

/// Build the request handler backed by SQLite, the wayland bridge and the
/// live device manager.
fn build_handler(
    store: Arc<Store>,
    manager: device::ManagerHandle,
    device_id: String,
) -> RequestHandler {
    Arc::new(move |req: Request| -> BoxFuture<'static, Response> {
        let store = store.clone();
        let manager = manager.clone();
        let device_id = device_id.clone();
        Box::pin(async move { handle_request(req, store, manager, device_id).await })
    })
}

async fn handle_request(
    req: Request,
    store: Arc<Store>,
    manager: device::ManagerHandle,
    device_id: String,
) -> Response {
    match req {
        Request::Ping => Response::ok_json(serde_json::json!("pong")),
        Request::DaemonInfo => {
            let session = handfast_wayland::detect_session();
            Response::ok_json(serde_json::json!({
                "app": handfast_core::APP_NAME,
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": PROTO_VERSION,
                "ipc": handfast_ipc::IPC_VERSION,
                "device_id": device_id,
                "session": {
                    "kind": format!("{:?}", session.kind),
                    "compositor": session.compositor,
                    "protocols": session.protocols,
                },
            }))
        }
        Request::DeviceList => {
            Response::ok_json(serde_json::Value::Array(manager.list_devices().await))
        }
        Request::DevicePair { device_id } => match manager.start_pairing(device_id).await {
            Ok(accepted) => Response::ok_json(serde_json::json!({ "accepted": accepted })),
            Err(err) => Response::err(2001, err.to_string()),
        },
        Request::DeviceUnpair { device_id } => match manager.unpair(device_id).await {
            Ok(()) => Response::ok_json(serde_json::json!({ "unpaired": true })),
            Err(err) => Response::err(2002, err.to_string()),
        },
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
        Request::PluginSetEnabled {
            device_id,
            plugin,
            enabled,
        } => {
            let key = format!("plugin:{device_id}:{plugin}");
            match store.kv_set(&key, if enabled { "1" } else { "0" }) {
                Ok(()) => Response::ok_json(serde_json::json!({"saved": true})),
                Err(err) => Response::err(3000, err.to_string()),
            }
        }
        Request::SendFile { device_id, path } => match manager.send_file(device_id, path).await {
            Ok(transfer_id) => Response::ok_json(serde_json::json!({ "transfer_id": transfer_id })),
            Err(err) => Response::err(4001, err.to_string()),
        },
        Request::TransferList => {
            Response::ok_json(serde_json::Value::Array(manager.transfer_list().await))
        }
        Request::TransferCancel { transfer_id } => {
            match manager.transfer_cancel(transfer_id).await {
                Ok(()) => Response::ok_json(serde_json::json!({ "cancelled": true })),
                Err(err) => Response::err(4002, err.to_string()),
            }
        }
        Request::NotificationList => Response::ok_json(serde_json::Value::Array(vec![])),
        Request::NotificationDismiss { notification_id } => {
            tracing::debug!(%notification_id, "notification dismiss is a no-op until Phase 3");
            Response::ok_json(serde_json::json!({"dismissed": true}))
        }
        Request::ClipboardGet => {
            let result =
                tokio::task::spawn_blocking(handfast_wayland::clipboard::Clipboard::get_text).await;
            match result {
                Ok(Ok(text)) => Response::ok_json(serde_json::json!({ "text": text })),
                Ok(Err(err)) => Response::err(5000, err.to_string()),
                Err(join_err) => Response::err(5000, join_err.to_string()),
            }
        }
        Request::ClipboardSet { text } => {
            let owned = text.clone();
            let result = tokio::task::spawn_blocking(move || {
                handfast_wayland::clipboard::Clipboard::set_text(&owned)
            })
            .await;
            match result {
                Ok(Ok(())) => Response::ok_json(serde_json::json!({"set": true})),
                Ok(Err(err)) => Response::err(5000, err.to_string()),
                Err(join_err) => Response::err(5000, join_err.to_string()),
            }
        }
        Request::RunCommandList { device_id } => {
            tracing::debug!(%device_id, "run-command listing not wired yet");
            Response::ok_json(serde_json::Value::Array(vec![]))
        }
        Request::RunCommand {
            device_id,
            command_name,
        } => {
            tracing::debug!(%device_id, %command_name, "remote command execution not wired");
            Response::err(
                6000,
                format!("remote command '{command_name}' arrives in Phase 4"),
            )
        }
        Request::SetVolume { percent } => {
            tracing::debug!(percent, "volume control not wired yet");
            Response::err(7000, "volume control arrives in Phase 4")
        }
        Request::GetVolume => Response::err(7001, "volume queries arrive in Phase 4"),
        Request::ShareText { device_id, .. } => {
            tracing::debug!(%device_id, "text sharing not wired yet");
            Response::err(8000, "text sharing arrives in Phase 4")
        }
        Request::ShareUrl { device_id, .. } => {
            tracing::debug!(%device_id, "URL sharing not wired yet");
            Response::err(8001, "URL sharing arrives in Phase 4")
        }
        Request::RequestBattery { device_id } => {
            tracing::debug!(%device_id, "battery requests not wired yet");
            Response::err(9000, "battery requests arrive in Phase 4")
        }
        Request::SendSms { device_id, .. } => {
            tracing::debug!(%device_id, "SMS sending not wired yet");
            Response::err(9001, "SMS sending arrives in Phase 4")
        }
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
/// Writes `READY=1` to `$NOTIFY_SOCKET` when running under systemd; silently
/// does nothing otherwise. Safe code only.
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

#[cfg(test)]
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod payload_smoke_tests {
    //! Loopback TLS payload-transfer smoke tests: prove the data-connection
    //! plumbing (PayloadListener + connect_payload + engine streaming) moves
    //! bytes over real TLS exactly like a KDE Connect peer would.

    use std::sync::Arc;

    use handfast_protocol::tls::CertPair;
    use handfast_protocol::transfer::{TransferMeta, UNKNOWN_SIZE};
    use tokio::sync::Mutex;

    use crate::payload::{connect_payload, PayloadListener};
    use crate::transfer::TransferEngine;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "handfastd-payload-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn cert_pair() -> Arc<CertPair> {
        let dir = scratch_dir("certs");
        Arc::new(CertPair::load_or_generate(&dir, "smoke-device").expect("cert pair must generate"))
    }

    /// Known-size transfer: sender announces `payloadSize`, receiver reads
    /// exactly that many bytes and finalizes the file.
    #[tokio::test]
    async fn known_size_payload_round_trips_over_tls() {
        let pair = cert_pair().await;
        let content: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        let save_dir = scratch_dir("known");

        let (port, listener) = PayloadListener::bind(pair.clone()).await.unwrap();
        let bytes = content.clone();
        let sender = tokio::spawn(async move {
            let mut transport = listener.accept().await.unwrap();
            for chunk in bytes.chunks(4096) {
                transport.write_bytes(chunk).await.unwrap();
            }
        });

        let engine = Arc::new(Mutex::new(TransferEngine::new(save_dir.clone())));
        let control_peer: std::net::SocketAddr = "127.0.0.1:1716".parse().unwrap();
        let mut transport = connect_payload(control_peer, port, pair).await.unwrap();

        let meta = TransferMeta {
            transfer_id: "known".into(),
            device_id: "peer".into(),
            file_name: "blob.bin".into(),
            file_size: content.len() as u64,
        };
        engine.lock().await.start_receive(meta).await.unwrap();

        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        while done < content.len() as u64 {
            let want = buf.len().min((content.len() as u64 - done) as usize);
            transport.read_bytes(&mut buf[..want]).await.unwrap();
            engine
                .lock()
                .await
                .write_chunk("known", &buf[..want])
                .await
                .unwrap();
            done += want as u64;
        }
        let final_path = engine.lock().await.finish_receive("known").await.unwrap();
        sender.await.unwrap();

        let written = std::fs::read(&final_path).unwrap();
        assert_eq!(written, content, "payload bytes must arrive intact");
        let _ = std::fs::remove_dir_all(&save_dir);
    }

    /// Unknown-size transfer: `payloadSize: -1` semantics — read until EOF.
    #[tokio::test]
    async fn unknown_size_payload_reads_until_eof() {
        let pair = cert_pair().await;
        let content: Vec<u8> = vec![0xAB; 128 * 1024 + 7];
        let save_dir = scratch_dir("unknown");

        let (port, listener) = PayloadListener::bind(pair.clone()).await.unwrap();
        let bytes = content.clone();
        let sender = tokio::spawn(async move {
            let mut transport = listener.accept().await.unwrap();
            for chunk in bytes.chunks(8192) {
                transport.write_bytes(chunk).await.unwrap();
            }
        });

        let engine = Arc::new(Mutex::new(TransferEngine::new(save_dir.clone())));
        let control_peer: std::net::SocketAddr = "127.0.0.1:1716".parse().unwrap();
        let mut transport = connect_payload(control_peer, port, pair).await.unwrap();

        let meta = TransferMeta {
            transfer_id: "unknown".into(),
            device_id: "peer".into(),
            file_name: "stream.bin".into(),
            file_size: UNKNOWN_SIZE,
        };
        engine.lock().await.start_receive(meta).await.unwrap();

        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = transport.read_some(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            engine
                .lock()
                .await
                .write_chunk("unknown", &buf[..n])
                .await
                .unwrap();
        }
        let final_path = engine.lock().await.finish_receive("unknown").await.unwrap();
        sender.await.unwrap();

        let written = std::fs::read(&final_path).unwrap();
        assert_eq!(written, content);
        let _ = std::fs::remove_dir_all(&save_dir);
    }

    /// Empty payload (no data connection): engine must produce an empty file.
    #[tokio::test]
    async fn empty_file_receive_produces_empty_file() {
        let save_dir = scratch_dir("empty");
        let engine = Arc::new(Mutex::new(TransferEngine::new(save_dir.clone())));

        let meta = TransferMeta {
            transfer_id: "empty".into(),
            device_id: "peer".into(),
            file_name: "empty.txt".into(),
            file_size: 0,
        };
        engine.lock().await.start_receive(meta).await.unwrap();
        let final_path = engine.lock().await.finish_receive("empty").await.unwrap();

        assert_eq!(std::fs::metadata(&final_path).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&save_dir);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod control_handshake_tests {
    //! Loopback control-plane handshake tests: prove the upstream KDE Connect
    //! wire behavior — plaintext identity first, TLS roles inverted
    //! (acceptor = TLS client, dialer = TLS server), secure identity
    //! re-exchange inside TLS with deviceId/protocolVersion cross-checks —
    //! exactly as the Android app and kdeconnect-kde expect.

    use handfast_protocol::tls::CertPair;
    use handfast_protocol::{Identity, Packet, PROTO_VERSION, TYPE_IDENTITY};
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    use crate::tls::Transport;

    /// 32-character device ids, matching Android's `^[a-zA-Z0-9_-]{32,38}$`.
    const DIALER_ID: &str = "dialerdevice0000000000000000000000";
    const ACCEPTOR_ID: &str = "acceptordevice00000000000000000000";

    fn test_identity(device_id: &str, name: &str) -> Identity {
        Identity {
            device_id: device_id.to_string(),
            name: name.to_string(),
            device_type: "desktop".into(),
            protocol_version: PROTO_VERSION,
            incoming: vec!["kdeconnect.ping".into()],
            outgoing: vec!["kdeconnect.ping".into()],
            tcp_source_port: handfast_protocol::DEFAULT_TCP_PORT,
        }
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "handfastd-control-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn cert_pair(tag: &str) -> std::sync::Arc<CertPair> {
        let dir = scratch_dir(tag);
        std::sync::Arc::new(CertPair::load_or_generate(&dir, tag).expect("cert pair must generate"))
    }

    /// Android-style handshake-line read: byte by byte, never over-reading
    /// into the TLS bytes that follow.
    async fn read_line_raw(tcp: &mut TcpStream) -> Vec<u8> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = tcp.read(&mut byte).await.unwrap();
            assert!(n > 0, "peer closed mid-identity-line");
            if byte[0] == b'\n' {
                return line;
            }
            line.push(byte[0]);
        }
    }

    /// The two real handshake entry points complete against each other over
    /// loopback, each side seeing the other's secure identity.
    #[tokio::test]
    async fn dial_and_accept_handshake_completes() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let pair_dialer = cert_pair("dialer").await;
        let mine_dialer = test_identity(DIALER_ID, "Dialer Box");
        let dialer_pair = pair_dialer.clone();
        let dialer = tokio::spawn(async move {
            crate::handshake::dial_control(addr, dialer_pair, &mine_dialer).await
        });

        let (tcp, _) = listener.accept().await.unwrap();
        let pair_acceptor = cert_pair("acceptor").await;
        let mine_acceptor = test_identity(ACCEPTOR_ID, "Acceptor Box");
        let (secure, transport) =
            crate::handshake::accept_control(tcp, pair_acceptor.clone(), &mine_acceptor)
                .await
                .unwrap();

        assert_eq!(secure.device_id, DIALER_ID);
        assert_eq!(secure.name, "Dialer Box");
        assert_eq!(secure.protocol_version, PROTO_VERSION);
        // The acceptor pinned the dialer's TLS fingerprint — that is the
        // data the pairing flow stores.
        assert_eq!(
            transport.peer_fingerprint(),
            pair_dialer.fingerprint_sha256,
            "peer fingerprint must be the dialer's certificate"
        );

        let (dialer_secure, dialer_transport) = dialer.await.unwrap().unwrap();
        assert_eq!(dialer_secure.device_id, ACCEPTOR_ID);
        assert_eq!(dialer_secure.name, "Acceptor Box");
        assert_eq!(
            dialer_transport.peer_fingerprint(),
            pair_acceptor.fingerprint_sha256
        );
    }

    /// The very first bytes a control dialer emits must be a plaintext
    /// `kdeconnect.identity` JSON line with the upstream field names — this
    /// is what Android's `tcpPacketReceived`/`udpPacketReceived` parse before
    /// any TLS bytes are exchanged.
    #[tokio::test]
    async fn dialer_writes_plaintext_identity_before_tls() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let pair_dialer = cert_pair("dialer").await;
        let mine_dialer = test_identity(DIALER_ID, "Dialer Box");
        let dialer = tokio::spawn(async move {
            crate::handshake::dial_control(addr, pair_dialer, &mine_dialer).await
        });

        let (mut tcp, _) = listener.accept().await.unwrap();
        let line = read_line_raw(&mut tcp).await;
        let text = String::from_utf8(line.clone()).unwrap();

        // Upstream field spellings, byte for byte.
        assert!(text.contains("\"deviceName\":\"Dialer Box\""), "{text}");
        assert!(text.contains("\"protocolVersion\":8"), "{text}");
        assert!(text.contains("\"tcpPort\":1716"), "{text}");
        assert!(text.contains("\"deviceType\":\"desktop\""), "{text}");

        let packet: Packet = serde_json::from_slice(&line).unwrap();
        assert_eq!(packet.ptype, TYPE_IDENTITY);
        let plain: Identity = serde_json::from_value(packet.body).unwrap();
        assert_eq!(plain.device_id, DIALER_ID);

        // Finish the handshake exactly like the acceptor would (TLS client
        // role, then the secure exchange) so the dialer completes too.
        let pair_acceptor = cert_pair("acceptor").await;
        let mine_acceptor = test_identity(ACCEPTOR_ID, "Acceptor Box");
        let transport = Transport::accept_control(tcp, pair_acceptor).await.unwrap();
        let (secure, _) = crate::handshake::exchange(transport, &mine_acceptor)
            .await
            .unwrap();
        assert_eq!(secure.device_id, DIALER_ID);
        assert_eq!(secure.name, "Dialer Box");

        let (dialer_secure, _) = dialer.await.unwrap().unwrap();
        assert_eq!(dialer_secure.device_id, ACCEPTOR_ID);
    }

    /// A peer whose in-TLS identity disagrees with its plaintext identity on
    /// `deviceId` is a spoofing attempt and must be rejected (mirrors
    /// android's "Device ID changed half-way through the handshake").
    #[tokio::test]
    async fn secure_identity_mismatch_is_rejected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let pair_acceptor = cert_pair("acceptor").await;
        let mine_acceptor = test_identity(ACCEPTOR_ID, "Acceptor Box");
        let acceptor = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            crate::handshake::accept_control(tcp, pair_acceptor, &mine_acceptor).await
        });

        // Dialer writes plaintext as DIALER_ID, then presents a DIFFERENT
        // identity inside TLS.
        let pair_dialer = cert_pair("dialer").await;
        let plain = test_identity(DIALER_ID, "Dialer Box");
        let spoof = test_identity(ACCEPTOR_ID, "Impostor");
        let transport = Transport::connect_control(addr, pair_dialer, &plain)
            .await
            .unwrap();
        // Secure identity re-exchange with the spoofed id.
        let mut transport = transport;
        transport
            .write_packet(&Packet::identity(spoof.clone()))
            .await
            .unwrap();
        // Drain the acceptor's secure identity so it can finish its write.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), transport.read_packet())
            .await
            .expect("acceptor must send its secure identity")
            .unwrap();

        let result = acceptor.await.unwrap();
        assert!(
            result.is_err(),
            "accept must reject a device id that changes mid-handshake"
        );
    }

    /// Same cross-check for the protocol version (anti-downgrade).
    #[tokio::test]
    async fn protocol_version_mismatch_is_rejected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let pair_acceptor = cert_pair("acceptor").await;
        let mine_acceptor = test_identity(ACCEPTOR_ID, "Acceptor Box");
        let acceptor = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            crate::handshake::accept_control(tcp, pair_acceptor, &mine_acceptor).await
        });

        let pair_dialer = cert_pair("dialer").await;
        let mut downgraded = test_identity(DIALER_ID, "Dialer Box");
        downgraded.protocol_version = 5; // plaintext claims 5
        let mut secure = test_identity(DIALER_ID, "Dialer Box");
        secure.protocol_version = PROTO_VERSION; // in-TLS claims 8

        let transport = Transport::connect_control(addr, pair_dialer, &downgraded)
            .await
            .unwrap();
        let mut transport = transport;
        transport
            .write_packet(&Packet::identity(secure))
            .await
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), transport.read_packet())
            .await
            .expect("acceptor must send its secure identity")
            .unwrap();

        let result = acceptor.await.unwrap();
        assert!(
            result.is_err(),
            "accept must reject a protocol version change mid-handshake"
        );
    }

    /// A peer that skips the plaintext identity and sends TLS bytes straight
    /// away (e.g. an old Handfast, or a payload-style client) must be
    /// rejected fast — never parsed as JSON, never a 10s stall.
    #[tokio::test]
    async fn missing_plaintext_identity_is_rejected() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let pair_acceptor = cert_pair("acceptor").await;
        let mine_acceptor = test_identity(ACCEPTOR_ID, "Acceptor Box");
        let acceptor = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            crate::handshake::accept_control(tcp, pair_acceptor, &mine_acceptor).await
        });

        // Client goes straight to TLS (old behavior) — no plaintext identity.
        let pair_dialer = cert_pair("dialer").await;
        let _old_style = Transport::connect(addr, pair_dialer).await;

        let start = std::time::Instant::now();
        let result = acceptor.await.unwrap();
        assert!(result.is_err(), "non-identity first bytes must be rejected");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "rejection must be fast, not a handshake-timeout stall"
        );
    }
}
