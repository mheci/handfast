//! `handfastd` composition root.
//!
//! P0 exposes a stable identity and capability set. The discovery, TLS and
//! link tasks land in P1.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use handfast_plugins::registry;
use handfast_proto::{DeviceId, DeviceName, DeviceType, Identity};
use handfast_store::Store;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "handfastd", version, about = "Handfast KDE Connect daemon")]
struct Args {
    /// TCP control port announced to peers.
    #[arg(long, default_value_t = 1716)]
    tcp_port: u16,

    /// State directory. Defaults to the XDG data directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,

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
    let store = Store::open(data_dir.join("state.db"))?;
    let device_id = load_or_create_device_id(&store)?;
    let capabilities = registry().capabilities()?;
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

    tracing::info!(
        device_id = %identity.device_id,
        tcp_port = args.tcp_port,
        "handfastd P0 starting; link layer lands in P1"
    );
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    Ok(())
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
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(value).join("handfast");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/handfast");
    }
    PathBuf::from("handfast-data")
}
