use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use clap::{ArgGroup, CommandFactory, Parser, Subcommand, ValueEnum};
use handfast_daemon::control::{
    send_request, CommandConfigAction, ControlError, ControlRequest, ControlResponse, DaemonStatus,
    PairingSummary, RemoteCommandRequest, RemoteInputAction, RemoteMediaCommand,
};
use handfast_daemon::service::{LocalControl, Service, SystemCommands};
use handfast_daemon::update::{Authenticode, GitHub, Updater};
use handfast_daemon::{
    default_data_dir, no_subcommand, parse_tcp_port, run, DaemonArgs, NoSubcommand,
    DEFAULT_TCP_PORT, SHUTDOWN_GRACE,
};
use handfast_plugins::remotecontrol::{RemoteClick, RemoteInputModifiers};
use handfast_proto::DeviceId;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "handfast", version, about = "Handfast KDE Connect client")]
struct Cli {
    /// State directory. Defaults to the platform data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon.
    Daemon(DaemonArgs),
    /// Send a file to a connected device.
    Share(ShareArgs),
    /// Accept or reject a pending pairing request.
    Pair(PairArgs),
    /// Ask a connected device to ring.
    Ring(RingArgs),
    /// List or change a connected device's audio sinks.
    Volume(VolumeArgs),
    /// List or control a connected device's media players.
    Media(MediaArgs),
    /// List or trigger a connected device's commands.
    #[command(name = "remote-command")]
    RemoteCommands(RemoteCommandArgs),
    /// Send mouse or keyboard input to a connected device.
    #[command(name = "remote-input")]
    RemoteInput(RemoteInputArgs),
    /// List or edit the local runcommand configuration.
    #[command(name = "command")]
    Commands(CommandArgs),
    /// Print the running daemon's status.
    Status(StatusArgs),
    /// Install and control the per-user autostart service.
    Service(ServiceArgs),
    /// Check for and install a newer release.
    Update(UpdateArgs),
    /// Open the Handfast window.
    Gui,
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
#[command(group(ArgGroup::new("answer").required(true)))]
struct PairArgs {
    /// Accept the pairing request from this device ID.
    #[arg(long, value_name = "DEVICE_ID", group = "answer")]
    accept: Option<String>,

    /// Reject the pairing request from this device ID.
    #[arg(long, value_name = "DEVICE_ID", group = "answer")]
    reject: Option<String>,
}

#[derive(Debug, Parser)]
struct RingArgs {
    /// Connected device ID from `handfast status`.
    #[arg(value_name = "DEVICE_ID")]
    device_id: String,
}

#[derive(Debug, Parser)]
struct VolumeArgs {
    /// Connected device ID from `handfast status`.
    #[arg(value_name = "DEVICE_ID")]
    device_id: String,

    /// Sink name from the listing.
    #[arg(long, value_name = "NAME")]
    sink: Option<String>,

    /// Set the sink volume as a percentage of its maximum.
    #[arg(
        long,
        value_name = "PERCENT",
        value_parser = clap::value_parser!(u8).range(0..=100),
        conflicts_with_all = ["mute", "unmute"]
    )]
    percent: Option<u8>,

    /// Mute the sink.
    #[arg(long, conflicts_with = "unmute")]
    mute: bool,

    /// Unmute the sink.
    #[arg(long)]
    unmute: bool,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum MediaActionArg {
    /// Start playback.
    Play,
    /// Pause playback.
    Pause,
    /// Toggle between play and pause.
    Toggle,
    /// Stop playback.
    Stop,
    /// Go to the next track.
    Next,
    /// Go to the previous track.
    Previous,
}

impl From<MediaActionArg> for handfast_runtime::RemoteMediaAction {
    fn from(action: MediaActionArg) -> Self {
        match action {
            MediaActionArg::Play => Self::Play,
            MediaActionArg::Pause => Self::Pause,
            MediaActionArg::Toggle => Self::PlayPause,
            MediaActionArg::Stop => Self::Stop,
            MediaActionArg::Next => Self::Next,
            MediaActionArg::Previous => Self::Previous,
        }
    }
}

#[derive(Debug, Parser)]
struct MediaArgs {
    /// Connected device ID from `handfast status`.
    #[arg(value_name = "DEVICE_ID")]
    device_id: String,

    /// Player name from the listing.
    #[arg(long, value_name = "NAME")]
    player: Option<String>,

    /// Ask the peer for its player list.
    #[arg(long)]
    list: bool,

    /// Ask one player for its current state.
    #[arg(long)]
    status: bool,

    /// Transport action.
    #[arg(long, value_enum)]
    action: Option<MediaActionArg>,

    /// Set the player volume as a percentage.
    #[arg(long, value_name = "PERCENT", value_parser = clap::value_parser!(u8).range(0..=100))]
    volume: Option<u8>,

    /// Seek by a relative offset in microseconds.
    #[arg(long, value_name = "MICROSECONDS", allow_hyphen_values = true)]
    seek: Option<i64>,

    /// Seek to an absolute position in milliseconds.
    #[arg(long, value_name = "MS")]
    position: Option<u64>,
}

#[derive(Debug, Parser)]
struct RemoteCommandArgs {
    /// Connected device ID from `handfast status`.
    #[arg(value_name = "DEVICE_ID")]
    device_id: String,

    /// Ask the peer to resend its command list.
    #[arg(long)]
    list: bool,

    /// Trigger the command with this key.
    #[arg(long, value_name = "KEY")]
    run: Option<String>,

    /// Ask the peer to open its command editor.
    #[arg(long)]
    setup: bool,

    /// Ask the peer to close its command editor.
    #[arg(long)]
    stop: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ClickArg {
    /// Left click.
    Left,
    /// Left double click.
    Double,
    /// Middle click.
    Middle,
    /// Right click.
    Right,
    /// Left button press.
    Hold,
    /// Left button release.
    Release,
}

#[derive(Debug, Parser)]
struct RemoteInputArgs {
    /// Connected device ID from `handfast status`.
    #[arg(value_name = "DEVICE_ID")]
    device_id: String,

    /// Relative pointer move.
    #[arg(
        long = "move",
        num_args = 2,
        value_names = ["DX", "DY"],
        allow_negative_numbers = true
    )]
    r#move: Option<Vec<i32>>,

    /// Scroll step.
    #[arg(
        long,
        num_args = 2,
        value_names = ["DX", "DY"],
        allow_negative_numbers = true
    )]
    scroll: Option<Vec<i32>>,

    /// Mouse click.
    #[arg(long, value_enum)]
    click: Option<ClickArg>,

    /// Text to type on the peer.
    #[arg(long, value_name = "TEXT")]
    key: Option<String>,

    /// Special key code, 1-32.
    #[arg(long, value_name = "CODE", value_parser = clap::value_parser!(u8).range(1..=32))]
    special: Option<u8>,

    /// Hold Alt.
    #[arg(long)]
    alt: bool,

    /// Hold Control.
    #[arg(long)]
    ctrl: bool,

    /// Hold Shift.
    #[arg(long)]
    shift: bool,

    /// Hold Super.
    #[arg(long)]
    sup: bool,
}

#[derive(Debug, Subcommand)]
enum CommandAction {
    /// List the configured commands.
    List,
    /// Add a command. Everything after `--` is the argument vector.
    Add {
        /// Display name.
        #[arg(long)]
        name: String,
        /// Program and arguments after `--`.
        #[arg(last = true, required = true, value_name = "ARGV")]
        argv: Vec<String>,
    },
    /// Remove a command by key.
    Remove {
        /// Command key from the listing.
        key: String,
    },
}

#[derive(Debug, Parser)]
struct CommandArgs {
    #[command(subcommand)]
    action: CommandAction,
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

#[derive(Debug, Parser)]
struct ServiceArgs {
    #[command(subcommand)]
    command: ServiceCommand,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Install the per-user autostart service and start it.
    Install,
    /// Stop and remove the per-user autostart service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the daemon and the service.
    Stop,
    /// Print whether the service is installed and running.
    Status,
}

#[derive(Debug, Parser)]
struct UpdateArgs {
    #[command(subcommand)]
    command: UpdateCommand,
}

#[derive(Debug, Subcommand)]
enum UpdateCommand {
    /// Print whether a newer release exists.
    Check,
    /// Download, verify and install the latest release.
    Apply,
}

fn parse_device_id(value: &str) -> Result<DeviceId, String> {
    DeviceId::new(value).map_err(|error| error.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Ok(exe) = std::env::current_exe() {
        let _ = handfast_daemon::update::cleanup_old(&exe);
    }
    let Some(command) = cli.command else {
        return match no_subcommand() {
            NoSubcommand::Gui => run_gui(cli.data_dir, true),
            NoSubcommand::Help => print_help(),
        };
    };
    match command {
        Command::Version => {
            println!(
                "handfast {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("HANDFAST_TARGET")
            );
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
        Command::Pair(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_pair(args, data_dir)
        }
        Command::Ring(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_ring(args, data_dir)
        }
        Command::Volume(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_volume(args, data_dir)
        }
        Command::Media(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_media(args, data_dir)
        }
        Command::RemoteCommands(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_remote_command(args, data_dir)
        }
        Command::RemoteInput(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_remote_input(args, data_dir)
        }
        Command::Commands(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_command_config(args, data_dir)
        }
        Command::Status(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_status(args, data_dir)
        }
        Command::Service(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_service(args, data_dir)
        }
        Command::Update(args) => {
            let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
            run_update(args, data_dir)
        }
        Command::Gui => run_gui(cli.data_dir, false),
    }
}

#[cfg(feature = "gui")]
fn run_gui(data_dir: Option<PathBuf>, hide_console: bool) -> ExitCode {
    let data_dir = data_dir.unwrap_or_else(default_data_dir);
    handfast_daemon::gui::run(data_dir, hide_console)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_data_dir: Option<PathBuf>, _hide_console: bool) -> ExitCode {
    eprintln!("handfast: this build has no GUI; build with --features gui");
    ExitCode::from(2)
}

/// Print the command help to stderr and exit 2, as clap does for a missing
/// subcommand.
fn print_help() -> ExitCode {
    let mut command = Cli::command();
    let _ = command.write_help(&mut std::io::stderr());
    ExitCode::from(2)
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
        ControlResponse::Status { .. }
        | ControlResponse::ShuttingDown
        | ControlResponse::Decided
        | ControlResponse::Commands { .. } => Err(ControlError::Remote(
            "unexpected control response".to_string(),
        )),
    }
}

fn run_pair(args: PairArgs, data_dir: PathBuf) -> ExitCode {
    let (device_id, accept) = match (args.accept, args.reject) {
        (Some(device_id), _) => (device_id, true),
        (None, Some(device_id)) => (device_id, false),
        (None, None) => {
            return fail("pair needs --accept <device-id> or --reject <device-id>");
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_pair(device_id, accept, data_dir)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

fn run_ring(args: RingArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_ring(args.device_id, data_dir)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

async fn send_ring(device_id: String, data_dir: PathBuf) -> Result<String, ControlError> {
    if DeviceId::new(&device_id).is_err() {
        return Err(ControlError::Remote(format!(
            "{device_id} is not a device ID"
        )));
    }
    let request = ControlRequest::Ring {
        device_id: device_id.clone(),
    };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Ok { .. } => Ok(format!("ringing {device_id}")),
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        response => Err(ControlError::Remote(format!(
            "unexpected control response: {response:?}"
        ))),
    }
}

fn run_volume(args: VolumeArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_volume(args, data_dir)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

async fn send_volume(args: VolumeArgs, data_dir: PathBuf) -> Result<String, ControlError> {
    if DeviceId::new(&args.device_id).is_err() {
        return Err(ControlError::Remote(format!(
            "{} is not a device ID",
            args.device_id
        )));
    }
    let changing = args.percent.is_some() || args.mute || args.unmute;
    if !changing {
        let status = request_status(&data_dir).await?;
        if !status
            .connected
            .iter()
            .any(|device| device.device_id.as_str() == args.device_id)
        {
            return Err(ControlError::Remote(format!(
                "device {} is not connected",
                args.device_id
            )));
        }
        let Some(device) = status
            .remote_sinks
            .iter()
            .find(|device| device.device_id.as_str() == args.device_id)
        else {
            return Ok(format!(
                "{} has not reported any audio sinks",
                args.device_id
            ));
        };
        let mut lines = Vec::with_capacity(device.sinks.len());
        for sink in &device.sinks {
            let state = if sink.muted { "muted" } else { "unmuted" };
            let default = if sink.enabled { " (default)" } else { "" };
            lines.push(format!(
                "{}\t{}%\t{state}\t{}{default}",
                sink.name,
                sink.percent(),
                sink.description
            ));
        }
        return Ok(lines.join("\n"));
    }
    let Some(sink) = args.sink else {
        return Err(ControlError::Remote(
            "volume needs --sink <NAME> with --percent, --mute or --unmute".to_string(),
        ));
    };
    let (percent, muted) = if let Some(percent) = args.percent {
        (Some(percent), None)
    } else if args.mute {
        (None, Some(true))
    } else if args.unmute {
        (None, Some(false))
    } else {
        return Err(ControlError::Remote(
            "send one of --percent, --mute or --unmute".to_string(),
        ));
    };
    let request = ControlRequest::RemoteVolume {
        device_id: args.device_id.clone(),
        sink: sink.clone(),
        percent,
        muted,
    };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Ok { .. } => Ok(format!("set {sink} on {}", args.device_id)),
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        response => Err(ControlError::Remote(format!(
            "unexpected control response: {response:?}"
        ))),
    }
}

fn run_media(args: MediaArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_media(args, data_dir)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

async fn send_media(args: MediaArgs, data_dir: PathBuf) -> Result<String, ControlError> {
    if DeviceId::new(&args.device_id).is_err() {
        return Err(ControlError::Remote(format!(
            "{} is not a device ID",
            args.device_id
        )));
    }
    let operations = [
        args.list,
        args.status,
        args.action.is_some(),
        args.volume.is_some(),
        args.seek.is_some(),
        args.position.is_some(),
    ];
    if operations.iter().filter(|selected| **selected).count() > 1 {
        return Err(ControlError::Remote(
            "media needs one operation".to_string(),
        ));
    }
    if args.list {
        let request = ControlRequest::Media {
            device_id: args.device_id.clone(),
            command: RemoteMediaCommand::RequestList,
        };
        return match send_request(&data_dir, &request).await? {
            ControlResponse::Ok { .. } => {
                Ok(format!("requested the player list from {}", args.device_id))
            }
            ControlResponse::Err { message } => Err(ControlError::Remote(message)),
            response => Err(ControlError::Remote(format!(
                "unexpected control response: {response:?}"
            ))),
        };
    }
    let status = request_status(&data_dir).await?;
    if !status
        .connected
        .iter()
        .any(|device| device.device_id.as_str() == args.device_id)
    {
        return Err(ControlError::Remote(format!(
            "device {} is not connected",
            args.device_id
        )));
    }
    let players = status
        .remote_players
        .iter()
        .find(|device| device.device_id.as_str() == args.device_id)
        .map_or(&[][..], |device| device.players.as_slice());
    if players.is_empty() {
        return Ok(format!(
            "{} has not reported any media players",
            args.device_id
        ));
    }
    let player = match args.player {
        Some(player) => player,
        None if players.len() == 1 => players[0].player.clone(),
        None => {
            return Err(ControlError::Remote(
                "media needs --player <NAME>".to_string(),
            ));
        }
    };
    if !players.iter().any(|model| model.player == player) {
        return Err(ControlError::Remote(format!(
            "device {} has not reported the player {player}",
            args.device_id
        )));
    }
    let command = if args.status {
        RemoteMediaCommand::RequestStatus {
            player: player.clone(),
        }
    } else if let Some(action) = args.action {
        RemoteMediaCommand::Action {
            player: player.clone(),
            action: action.into(),
        }
    } else if let Some(volume) = args.volume {
        RemoteMediaCommand::SetVolume {
            player: player.clone(),
            volume,
        }
    } else if let Some(offset_us) = args.seek {
        RemoteMediaCommand::Seek {
            player: player.clone(),
            offset_us,
        }
    } else if let Some(position) = args.position {
        let Ok(position_ms) = i64::try_from(position) else {
            return Err(ControlError::Remote(
                "the position is too large".to_string(),
            ));
        };
        RemoteMediaCommand::SetPosition {
            player: player.clone(),
            position_ms,
        }
    } else {
        let mut lines = Vec::with_capacity(players.len());
        for model in players {
            let state = if model.playing { "playing" } else { "paused" };
            let title = if model.title.is_empty() {
                &model.player
            } else {
                &model.title
            };
            let artist = if model.artist.is_empty() {
                String::new()
            } else {
                format!(" - {}", model.artist)
            };
            lines.push(format!(
                "{}\t{state}\t{title}{artist}\t{}/{} ms\t{}%",
                model.player, model.position_ms, model.length_ms, model.volume
            ));
        }
        return Ok(lines.join("\n"));
    };
    let request = ControlRequest::Media {
        device_id: args.device_id.clone(),
        command,
    };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Ok { .. } => Ok(format!("sent media command to {player}")),
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        response => Err(ControlError::Remote(format!(
            "unexpected control response: {response:?}"
        ))),
    }
}

fn run_remote_command(args: RemoteCommandArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_remote_command(args, data_dir)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

async fn send_remote_command(
    args: RemoteCommandArgs,
    data_dir: PathBuf,
) -> Result<String, ControlError> {
    if DeviceId::new(&args.device_id).is_err() {
        return Err(ControlError::Remote(format!(
            "{} is not a device ID",
            args.device_id
        )));
    }
    let operations = [args.list, args.run.is_some(), args.setup, args.stop];
    if operations.iter().filter(|selected| **selected).count() > 1 {
        return Err(ControlError::Remote(
            "remote-command needs one operation".to_string(),
        ));
    }
    let (request, success) = if args.list {
        (
            RemoteCommandRequest::List,
            format!("requested the command list from {}", args.device_id),
        )
    } else if let Some(key) = args.run.as_deref() {
        (
            RemoteCommandRequest::Run {
                key: key.to_string(),
            },
            format!("sent command {key} to {}", args.device_id),
        )
    } else if args.setup {
        (
            RemoteCommandRequest::Setup,
            format!("sent setup to {}", args.device_id),
        )
    } else if args.stop {
        (
            RemoteCommandRequest::Stop,
            format!("sent stop to {}", args.device_id),
        )
    } else {
        return list_cached_commands(args.device_id, data_dir).await;
    };
    let request = ControlRequest::RemoteCommand {
        device_id: args.device_id.clone(),
        request,
    };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Ok { .. } => Ok(success),
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        response => Err(ControlError::Remote(format!(
            "unexpected control response: {response:?}"
        ))),
    }
}

fn run_remote_input(args: RemoteInputArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_remote_input(args, data_dir)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

async fn send_remote_input(
    args: RemoteInputArgs,
    data_dir: PathBuf,
) -> Result<String, ControlError> {
    if DeviceId::new(&args.device_id).is_err() {
        return Err(ControlError::Remote(format!(
            "{} is not a device ID",
            args.device_id
        )));
    }
    let operations = [
        args.r#move.is_some(),
        args.scroll.is_some(),
        args.click.is_some(),
        args.key.is_some(),
        args.special.is_some(),
    ];
    if operations.iter().filter(|selected| **selected).count() != 1 {
        return Err(ControlError::Remote(
            "remote-input needs one operation".to_string(),
        ));
    }
    let modifiers = RemoteInputModifiers {
        alt: args.alt,
        ctrl: args.ctrl,
        shift: args.shift,
        sup: args.sup,
    };
    if (args.alt || args.ctrl || args.shift || args.sup)
        && args.key.is_none()
        && args.special.is_none()
    {
        return Err(ControlError::Remote(
            "modifiers need --key or --special".to_string(),
        ));
    }
    let action = if let Some(delta) = args.r#move.as_deref() {
        RemoteInputAction::Move {
            dx: delta[0],
            dy: delta[1],
        }
    } else if let Some(delta) = args.scroll.as_deref() {
        RemoteInputAction::Scroll {
            dx: delta[0],
            dy: delta[1],
        }
    } else if let Some(click) = args.click {
        RemoteInputAction::Click {
            button: match click {
                ClickArg::Left => RemoteClick::Single,
                ClickArg::Double => RemoteClick::Double,
                ClickArg::Middle => RemoteClick::Middle,
                ClickArg::Right => RemoteClick::Right,
                ClickArg::Hold => RemoteClick::Hold,
                ClickArg::Release => RemoteClick::Release,
            },
        }
    } else if let Some(text) = args.key {
        RemoteInputAction::Key { text, modifiers }
    } else if let Some(code) = args.special {
        RemoteInputAction::SpecialKey { code, modifiers }
    } else {
        return Err(ControlError::Remote(
            "remote-input needs one operation".to_string(),
        ));
    };
    let request = ControlRequest::RemoteInput {
        device_id: args.device_id.clone(),
        action,
    };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Ok { .. } => Ok(format!("sent input to {}", args.device_id)),
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        response => Err(ControlError::Remote(format!(
            "unexpected control response: {response:?}"
        ))),
    }
}

async fn list_cached_commands(
    device_id: String,
    data_dir: PathBuf,
) -> Result<String, ControlError> {
    let status = request_status(&data_dir).await?;
    if !status
        .connected
        .iter()
        .any(|device| device.device_id.as_str() == device_id)
    {
        return Err(ControlError::Remote(format!(
            "device {device_id} is not connected"
        )));
    }
    let Some(device) = status
        .remote_commands
        .iter()
        .find(|device| device.device_id.as_str() == device_id)
    else {
        return Ok(format!("{device_id} has not reported any commands"));
    };
    let mut lines = Vec::with_capacity(device.commands.len());
    for command in &device.commands {
        lines.push(format!(
            "{}\t{}\t{}",
            command.key, command.name, command.command
        ));
    }
    Ok(lines.join("\n"))
}

fn run_command_config(args: CommandArgs, data_dir: PathBuf) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error),
    };
    match runtime.block_on(send_command_config(args, data_dir)) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

async fn send_command_config(args: CommandArgs, data_dir: PathBuf) -> Result<String, ControlError> {
    enum Outcome {
        Listed,
        Added(String),
        Removed(String),
    }

    let (action, outcome) = match args.action {
        CommandAction::List => (CommandConfigAction::List, Outcome::Listed),
        CommandAction::Add { name, argv } => {
            let key = uuid::Uuid::new_v4().to_string();
            (
                CommandConfigAction::Add {
                    key: key.clone(),
                    name,
                    argv,
                },
                Outcome::Added(key),
            )
        }
        CommandAction::Remove { key } => (
            CommandConfigAction::Remove { key: key.clone() },
            Outcome::Removed(key),
        ),
    };
    let request = ControlRequest::CommandConfig { action };
    match send_request(&data_dir, &request).await? {
        ControlResponse::Commands { commands } => {
            let mut lines = Vec::with_capacity(commands.len());
            for command in commands {
                lines.push(format!(
                    "{}\t{}\t{}",
                    command.key, command.name, command.command
                ));
            }
            Ok(lines.join("\n"))
        }
        ControlResponse::Ok { bytes } => match outcome {
            Outcome::Added(key) => Ok(format!("added {key}")),
            Outcome::Removed(key) if bytes == 1 => Ok(format!("removed {key}")),
            Outcome::Removed(key) => Err(ControlError::Remote(format!("no command {key}"))),
            Outcome::Listed => Err(ControlError::Remote(
                "unexpected control response".to_string(),
            )),
        },
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        response => Err(ControlError::Remote(format!(
            "unexpected control response: {response:?}"
        ))),
    }
}

async fn send_pair(
    device_id: String,
    accept: bool,
    data_dir: PathBuf,
) -> Result<String, ControlError> {
    let status = request_status(&data_dir).await?;
    let Some(fingerprint) = pending_fingerprint(&status.pending_pairings, &device_id) else {
        return Err(ControlError::Remote(format!(
            "no pairing request from {device_id}"
        )));
    };
    let request = ControlRequest::PairingDecision {
        device_id: device_id.clone(),
        accept,
        fingerprint: fingerprint.to_string(),
    };
    let response = send_request(&data_dir, &request).await?;
    decision_message(&device_id, accept, response)
}

/// The fingerprint of the pending row for `device_id`, so a stale command
/// cannot answer a newer prompt.
fn pending_fingerprint<'a>(pending: &'a [PairingSummary], device_id: &str) -> Option<&'a str> {
    pending
        .iter()
        .find(|pairing| pairing.device_id.as_str() == device_id)
        .map(|pairing| pairing.fingerprint.as_str())
}

fn decision_message(
    device_id: &str,
    accept: bool,
    response: ControlResponse,
) -> Result<String, ControlError> {
    let answer = if accept { "accepted" } else { "rejected" };
    match response {
        ControlResponse::Decided => Ok(format!("pairing {answer} for {device_id}")),
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        ControlResponse::Ok { .. }
        | ControlResponse::Status { .. }
        | ControlResponse::ShuttingDown
        | ControlResponse::Commands { .. } => Err(ControlError::Remote(
            "unexpected control response".to_string(),
        )),
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
    let status = request_status(&data_dir).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status(&status);
    }
    Ok(())
}

async fn request_status(data_dir: &Path) -> Result<DaemonStatus, ControlError> {
    match send_request(data_dir, &ControlRequest::Status).await? {
        ControlResponse::Status { status } => Ok(*status),
        ControlResponse::Err { message } => Err(ControlError::Remote(message)),
        ControlResponse::Ok { .. }
        | ControlResponse::ShuttingDown
        | ControlResponse::Decided
        | ControlResponse::Commands { .. } => Err(ControlError::Remote(
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
    println!("pairing requests: {}", status.pending_pairings.len());
    for pairing in &status.pending_pairings {
        println!(
            "  {}  {}  {}",
            pairing.device_id, pairing.device_name, pairing.fingerprint
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

fn run_service(args: ServiceArgs, data_dir: PathBuf) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return fail(error),
    };
    let log = data_dir.join("handfast.log");
    let commands = SystemCommands;
    let service = match Service::new(exe, data_dir.clone(), log, &commands) {
        Ok(service) => service,
        Err(error) => return fail(error),
    };
    let control = match LocalControl::new(data_dir) {
        Ok(control) => control,
        Err(error) => return fail(error),
    };

    let result = match args.command {
        ServiceCommand::Install => service.install(&control).map(|notes| {
            for note in notes {
                println!("{note}");
            }
        }),
        ServiceCommand::Uninstall => service.uninstall(),
        ServiceCommand::Start => service.start(&control),
        ServiceCommand::Stop => service.stop(&control),
        ServiceCommand::Status => {
            let status = service.status(&control);
            println!("{status}");
            return if status.running {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(error),
    }
}

fn run_update(args: UpdateArgs, data_dir: PathBuf) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return fail(error),
    };
    let commands = SystemCommands;
    let log = data_dir.join("handfast.log");
    let service = match Service::new(exe.clone(), data_dir.clone(), log, &commands) {
        Ok(service) => service,
        Err(error) => return fail(error),
    };
    let control = match LocalControl::new(data_dir) {
        Ok(control) => control,
        Err(error) => return fail(error),
    };
    let source = GitHub::new();
    let verifier = Authenticode;
    let updater = Updater::new(&source, &verifier, &control, &service);

    let result = match args.command {
        UpdateCommand::Check => updater.check().map(|check| println!("{check}")),
        UpdateCommand::Apply => updater.apply(&exe).map(|applied| println!("{applied}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(error),
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
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "0123456789abcdef0123456789abcdef";

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("handfast").chain(args.iter().copied()))
    }

    #[test]
    fn pair_takes_exactly_one_answer() {
        let cli = parse(&["pair", "--accept", DEVICE]).expect("--accept parses");
        let Some(Command::Pair(args)) = cli.command else {
            panic!("expected the pair subcommand");
        };
        assert_eq!(args.accept.as_deref(), Some(DEVICE));
        assert!(args.reject.is_none());

        let cli = parse(&["pair", "--reject", DEVICE]).expect("--reject parses");
        let Some(Command::Pair(args)) = cli.command else {
            panic!("expected the pair subcommand");
        };
        assert_eq!(args.reject.as_deref(), Some(DEVICE));
        assert!(args.accept.is_none());

        assert!(parse(&["pair"]).is_err(), "an answer is required");
        assert!(
            parse(&["pair", "--accept", DEVICE, "--reject", DEVICE]).is_err(),
            "the two answers are exclusive"
        );
    }

    #[test]
    fn a_decided_response_prints_the_answer() {
        assert_eq!(
            decision_message(DEVICE, true, ControlResponse::Decided)
                .expect("a decided response is a success"),
            format!("pairing accepted for {DEVICE}")
        );
        assert_eq!(
            decision_message(DEVICE, false, ControlResponse::Decided)
                .expect("a decided response is a success"),
            format!("pairing rejected for {DEVICE}")
        );
    }

    #[test]
    fn the_cli_takes_the_fingerprint_from_the_pending_row() {
        let device = DeviceId::new(DEVICE).expect("a valid device id");
        let fingerprint = "cd".repeat(32);
        let rows = vec![PairingSummary {
            device_id: device,
            device_name: "Tablet".to_string(),
            fingerprint: fingerprint.clone(),
        }];
        assert_eq!(
            pending_fingerprint(&rows, DEVICE),
            Some(fingerprint.as_str())
        );
        assert_eq!(
            pending_fingerprint(&rows, "fedcba9876543210fedcba9876543210"),
            None,
            "another device has no row"
        );
        assert_eq!(pending_fingerprint(&[], DEVICE), None, "no prompt is open");
    }

    #[test]
    fn a_pairing_error_is_relayed() {
        let error = decision_message(
            DEVICE,
            true,
            ControlResponse::Err {
                message: format!("no pairing request from {DEVICE}"),
            },
        )
        .expect_err("an error response fails");
        assert!(error.to_string().contains("no pairing request"), "{error}");

        let error = decision_message(DEVICE, true, ControlResponse::ShuttingDown)
            .expect_err("an unexpected response fails");
        assert!(
            error.to_string().contains("unexpected control response"),
            "{error}"
        );
    }
}
