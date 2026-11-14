//! `hfctl` local control client.
//!
//! `hfctl share` sends one framed request to the running `handfastd` over the
//! same-user control channel and prints the framed result. A remote error or an
//! IO failure exits 1; a clap usage error exits 2.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use handfast_daemon::control::{send_request, ControlError, ControlRequest, ControlResponse};
use handfast_daemon::default_data_dir;
use handfast_proto::DeviceId;

#[derive(Debug, Parser)]
#[command(name = "hfctl", version, about = "Handfast local control client")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Send a file to a connected device.
    Share {
        /// Device ID of the connected peer.
        #[arg(long, value_parser = parse_device_id)]
        device: DeviceId,

        /// State directory. Defaults to the platform data directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Local file to send.
        path: PathBuf,
    },
}

fn parse_device_id(value: &str) -> Result<DeviceId, String> {
    DeviceId::new(value).map_err(|error| error.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("hfctl: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hfctl: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), ControlError> {
    let Command::Share {
        device,
        data_dir,
        path,
    } = args.command;
    let data_dir = data_dir.unwrap_or_else(default_data_dir);
    let request = ControlRequest::Share {
        device_id: device.clone(),
        path,
    };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Ok { bytes } => {
            println!("sent {bytes} bytes to {device}");
            Ok(())
        }
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
    }
}
