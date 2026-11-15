use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use clap::{Parser, Subcommand};
use handfast_daemon::control::{
    send_request, ControlError, ControlRequest, ControlResponse, DaemonStatus,
};
use handfast_daemon::{
    default_data_dir, parse_tcp_port, run, DaemonArgs, DEFAULT_TCP_PORT, SHUTDOWN_GRACE,
};
use handfast_proto::DeviceId;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "handfast",
    version,
    about = "Handfast KDE Connect client",
    arg_required_else_help = true
)]
struct Cli {
    /// State directory. Defaults to the platform data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon.
    Daemon(DaemonArgs),
    /// Send a file to a connected device.
    Share(ShareArgs),
    /// Print the running daemon's status.
    Status(StatusArgs),
    /// Print this device's identity packet and exit.
    Identity(IdentityArgs),
    /// Print the version.
    Version,
}

#[derive(Debug, Parser)]
struct ShareArgs {
    /// Device ID of the connected peer.
    #[arg(long, value_parser = parse_device_id)]
    device: DeviceId,

    /// Local file to send.
    path: PathBuf,
}

#[derive(Debug, Parser)]
struct StatusArgs {
    /// Print the status as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
struct IdentityArgs {
    /// TCP port advertised in the packet, in the KDE Connect range 1716-1764.
    #[arg(long, default_value_t = DEFAULT_TCP_PORT, value_parser = parse_tcp_port)]
    tcp_port: u16,
}

fn parse_device_id(value: &str) -> Result<DeviceId, String> {
    DeviceId::new(value).map_err(|error| error.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            println!("handfast {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Identity(args) => {
            let args = DaemonArgs {
                data_dir: cli.data_dir,
                tcp_port: args.tcp_port,
                print_identity: true,
                ..DaemonArgs::default()
            };
            run_daemon(args)
        }
        Command::Daemon(mut args) => {
            args.data_dir = cli.data_dir;
            if let Err(error) = init_tracing(args.log_file.as_deref()) {
                return fail(error);
            }
            run_daemon(args)
        }
        Command::Share(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_share(args, data_dir)
        }
        Command::Status(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_status(args, data_dir)
        }
    }
}

fn run_daemon(args: DaemonArgs) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    let result = runtime.block_on(run(args, shutdown_signal()));
    // A pending terminal read runs on a blocking thread. Bound runtime
    // shutdown so a prompt cannot hold the process open.
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    report(result)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                eprintln!("handfast: cannot listen for SIGTERM: {error}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn run_share(args: ShareArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_share(args, data_dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(error),
    }
}

async fn send_share(args: ShareArgs, data_dir: PathBuf) -> Result<(), ControlError> {
    let device = args.device;
    let request = ControlRequest::Share {
        device_id: device.clone(),
        path: args.path,
    };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Ok { bytes } => {
            println!("sent {bytes} bytes to {device}");
            Ok(())
        }
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        ControlResponse::Status { .. } | ControlResponse::ShuttingDown => Err(
            ControlError::Remote("unexpected control response".to_string()),
        ),
    }
}

fn run_status(args: StatusArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(fetch_status(args.json, data_dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(error),
    }
}

async fn fetch_status(json: bool, data_dir: PathBuf) -> Result<(), ControlError> {
    match send_request(&data_dir, &ControlRequest::Status).await? {
        ControlResponse::Status { status } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            Ok(())
        }
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        ControlResponse::Ok { .. } | ControlResponse::ShuttingDown => Err(ControlError::Remote(
            "unexpected control response".to_string(),
        )),
    }
}

fn print_status(status: &DaemonStatus) {
    println!("version: {}", status.version);
    println!("pid: {}", status.pid);
    println!("uptime: {}s", status.uptime_secs);
    println!("device id: {}", status.identity.device_id);
    println!("device name: {}", status.identity.device_name);
    println!("device type: {}", status.identity.device_type);
    println!("fingerprint: {}", status.identity.fingerprint);
    println!("tcp port: {}", status.identity.tcp_port);
    println!("incoming dir: {}", status.incoming_dir.display());
    println!("connected: {}", status.connected.len());
    for device in &status.connected {
        println!(
            "  {}  {}  {}",
            device.device_id, device.device_name, device.device_type
        );
    }
    println!("paired: {}", status.paired.len());
    for device in &status.paired {
        let link = if device.connected {
            "connected"
        } else {
            "offline"
        };
        println!(
            "  {}  {}  {}  {}",
            device.device_id, device.device_name, device.device_type, link
        );
    }
    println!("transfers: {}", status.transfers.len());
    for transfer in &status.transfers {
        let total = transfer
            .total
            .map_or_else(|| "?".to_string(), |total| total.to_string());
        println!(
            "  {}  {}  {}/{}",
            transfer.device_id, transfer.transfer_id, transfer.bytes, total
        );
    }
    if status.truncated {
        println!("truncated: true");
    }
}

fn report(result: Result<(), Box<dyn std::error::Error + Send + Sync>>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(error),
    }
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("handfast: {error}");
    ExitCode::FAILURE
}

fn init_tracing(log_file: Option<&Path>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);
    let Some(path) = log_file else {
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .init();
        return Ok(());
    };

    let file = open_log(path)?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(Mutex::new(file))
        .with_target(false)
        .with_ansi(false);
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();
    Ok(())
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    rotate_log(path);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

fn rotate_log(path: &Path) {
    let oversized = path
        .metadata()
        .map(|metadata| metadata.len() > LOG_ROTATE_BYTES)
        .unwrap_or(false);
    if !oversized {
        return;
    }
    let mut rotated = path.as_os_str().to_os_string();
    rotated.push(".1");
    let rotated = PathBuf::from(rotated);
    let _ = std::fs::remove_file(&rotated);
    if let Err(error) = std::fs::rename(path, &rotated) {
        eprintln!(
            "handfast: cannot rotate {} to {}: {error}",
            path.display(),
            rotated.display()
        );
    }
}
