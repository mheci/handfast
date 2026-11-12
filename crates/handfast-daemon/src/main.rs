//! `handfastd` composition root.
//!
//! P0 exposes a stable identity and capability set. Discovery is wired here;
//! TLS, pairing and the link layer land in P1.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use handfast_net::Discovery;
use handfast_plugins::registry;
use handfast_proto::{DeviceId, DeviceName, DeviceType, Identity};
use handfast_store::{DeviceRecord, Store};
use tracing_subscriber::EnvFilter;

/// How often to announce this device over UDP discovery.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(name = "handfastd", version, about = "Handfast KDE Connect daemon")]
struct Args {
    /// State directory. Defaults to the XDG data directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// TCP control port announced to peers.
    #[arg(long, default_value_t = 1716)]
    tcp_port: u16,

    /// UDP discovery port.
    #[arg(long, default_value_t = handfast_net::DISCOVERY_PORT)]
    discovery_port: u16,

    /// Print this device's identity packet and exit.
    #[arg(long)]
    print_identity: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("handfastd: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(Store::open(data_dir.join("state.db"))?);
    let device_id = load_or_create_device_id(&store)?;
    let capabilities = registry().capabilities(&Default::default())?;
    let identity = Identity::new(
        device_id,
        DeviceName::new(&device_name()),
        DeviceType::Desktop,
        capabilities,
        Some(args.tcp_port),
    );

    if args.print_identity {
        println!("{}", serde_json::to_string_pretty(&identity.to_packet()?)?);
        return Ok(());
    }

    tracing::info!(device_id = %identity.device_id, "handfastd starting");
    let discovery = Discovery::bind(identity.clone(), args.discovery_port, args.tcp_port).await?;
    let local_addr = discovery.local_addr()?;
    tracing::info!(address = %local_addr, "discovery listening");

    let local_device_id = identity.device_id.clone();
    let discovery_store = Arc::clone(&store);
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
                    let (address, peer) = match received {
                        Ok(received) => received,
                        Err(error) => {
                            tracing::warn!(%error, "discovery receive failed");
                            continue;
                        }
                    };
                    if peer.device_id == local_device_id {
                        continue;
                    }
                    tracing::debug!(%address, device_id = %peer.device_id, "discovered device");
                    let record = DeviceRecord {
                        id: peer.device_id.as_str().to_string(),
                        name: peer.device_name.as_str().to_string(),
                        device_type: peer.device_type.as_str().to_string(),
                        paired: false,
                        last_seen: unix_now(),
                    };
                    if let Err(error) = discovery_store
                        .clone()
                        .run_blocking(move |store| store.upsert_device(&record))
                        .await
                    {
                        tracing::warn!(%error, "failed to persist discovered device");
                    }
                }
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    discovery_task.abort();
    tracing::info!("shutting down");
    Ok(())
}

fn unix_now() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
}

fn load_or_create_device_id(store: &Store) -> Result<DeviceId, Box<dyn std::error::Error>> {
    if let Some(value) = store.setting("device_id")? {
        return Ok(DeviceId::new(value)?);
    }
    let device_id = DeviceId::generate();
    store.set_setting("device_id", device_id.as_str())?;
    Ok(device_id)
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
