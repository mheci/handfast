//! Minimal same-user local control channel.
//!
//! The channel carries same-user requests from the CLI and the window. Unix
//! uses a Unix domain socket with a peer-credential check; Windows uses a named
//! pipe with a current-user DACL. The frame is a 4-byte big-endian length
//! followed by JSON, capped at 64 KiB.
//!
//! The server never touches the session map. It forwards a [`DaemonCommand`] to
//! the daemon event loop and relays the framed result. The path is validated by
//! `Files::open_outgoing` inside the session, so a directory or special file
//! fails before a packet is sent.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use handfast_plugins::remotecontrol::{RemoteClick, RemoteInputModifiers};
use handfast_proto::DeviceId;
use handfast_runtime::{RemoteCommand, RemoteMediaAction, RemotePlayer, RemoteSink};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(windows)]
mod windows;

/// Largest accepted frame body.
pub const MAX_FRAME: usize = 64 * 1024;

/// Deadline for one frame read or write.
pub const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The message the server writes when the daemon has stopped.
const DAEMON_SHUT_DOWN: &str = "daemon shut down";

/// Maximum device rows in one status snapshot.
///
/// A frame body over [`MAX_FRAME`] fails at `write_frame`, so a device list
/// that would overflow the frame is capped and reported with
/// [`DaemonStatus::truncated`].
pub const MAX_STATUS_DEVICES: usize = 128;

/// One request on the local control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Send a local file to a connected device.
    Share {
        /// Paired device ID.
        device_id: DeviceId,
        /// Local source path. The daemon validates it.
        path: PathBuf,
    },
    /// Read one daemon status snapshot.
    Status,
    /// Stop the daemon through its bounded shutdown path.
    Shutdown,
    /// Ask a connected device to ring.
    Ring {
        /// Paired device ID. The daemon validates it.
        device_id: String,
    },
    /// Change one audio sink on a connected device.
    RemoteVolume {
        /// Paired device ID. The daemon validates it.
        device_id: String,
        /// Sink name from the device's reported list.
        sink: String,
        /// Volume as a percentage of the sink's maximum.
        percent: Option<u8>,
        /// Mute state. Exactly one of `percent` and `muted` is set.
        muted: Option<bool>,
    },
    /// Control the media players on a connected device.
    Media {
        /// Paired device ID. The daemon validates it.
        device_id: String,
        /// The command to send.
        command: RemoteMediaCommand,
    },
    /// Trigger commands configured on a connected device.
    RemoteCommand {
        /// Paired device ID. The daemon validates it.
        device_id: String,
        /// The command to send.
        request: RemoteCommandRequest,
    },
    /// Send mouse or keyboard input to a connected device.
    RemoteInput {
        /// Paired device ID. The daemon validates it.
        device_id: String,
        /// The input to send.
        action: RemoteInputAction,
    },
    /// Read or refresh the contacts a connected device reports.
    Contacts {
        /// Paired device ID. The daemon validates it.
        device_id: String,
        /// Ask the peer for a fresh sync instead of listing the cache.
        refresh: bool,
    },
    /// List, request or send SMS messages on a connected device.
    Sms {
        /// Paired device ID. The daemon validates it.
        device_id: String,
        /// The SMS operation.
        action: SmsAction,
    },
    /// List or edit the local runcommand configuration.
    CommandConfig {
        /// The action to apply.
        action: CommandConfigAction,
    },
    /// Accept or reject a pending pairing request.
    PairingDecision {
        /// Device ID shown in the pairing prompt. The daemon validates it.
        device_id: String,
        /// Whether to accept the request.
        accept: bool,
        /// Fingerprint of the certificate the caller saw, so a stale decision
        /// cannot answer a newer prompt for the same device.
        fingerprint: String,
    },
}

/// One response on the local control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResponse {
    /// The request completed.
    Ok {
        /// Bytes written to the payload, or 0 when the request carries none.
        bytes: u64,
    },
    /// The configured local commands.
    Commands {
        /// Commands in key order.
        commands: Vec<CommandSummary>,
    },
    /// The contacts cached for one device.
    Contacts {
        /// Contacts in name order.
        contacts: Vec<ContactSummary>,
    },
    /// The SMS conversations cached for one device.
    SmsConversations {
        /// Conversations, newest first.
        conversations: Vec<SmsConversationSummary>,
    },
    /// The SMS messages cached for one thread.
    SmsThread {
        /// Messages, oldest first.
        messages: Vec<SmsMessageSummary>,
    },
    /// The daemon status snapshot.
    Status {
        /// Boxed so the variant stays small next to `Ok`.
        status: Box<DaemonStatus>,
    },
    /// The daemon accepted the shutdown request.
    ShuttingDown,
    /// The daemon recorded the pairing decision.
    Decided,
    /// The request failed. The message is safe to print.
    Err {
        /// Failure reason.
        message: String,
    },
}

/// One daemon status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// The daemon's crate version.
    pub version: String,
    /// Process ID.
    pub pid: u32,
    /// Whole seconds since the daemon started.
    pub uptime_secs: u64,
    /// This device's public identity.
    pub identity: IdentitySummary,
    /// Resolved incoming directory.
    pub incoming_dir: PathBuf,
    /// Connected devices, sorted by device ID.
    pub connected: Vec<DeviceSummary>,
    /// Paired devices from the store, sorted by device ID.
    pub paired: Vec<PairedDevice>,
    /// Pairing requests waiting for an answer, sorted by device ID.
    pub pending_pairings: Vec<PairingSummary>,
    /// In-flight transfers, sorted by device ID then transfer ID.
    pub transfers: Vec<TransferSummary>,
    /// Peer audio sinks, sorted by device ID.
    pub remote_sinks: Vec<RemoteDeviceSinks>,
    /// Peer media players, sorted by device ID.
    pub remote_players: Vec<RemoteDevicePlayers>,
    /// Peer commands, sorted by device ID.
    pub remote_commands: Vec<RemoteDeviceCommands>,
    /// Whether a device list hit [`MAX_STATUS_DEVICES`].
    pub truncated: bool,
}

/// One remote media request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteMediaCommand {
    /// Ask the peer for its player list.
    RequestList,
    /// Ask one player for its current state.
    RequestStatus {
        /// Player name from the device's reported list.
        player: String,
    },
    /// Send one transport action.
    Action {
        /// Player name from the device's reported list.
        player: String,
        /// Transport action.
        action: RemoteMediaAction,
    },
    /// Set one player's volume percentage.
    SetVolume {
        /// Player name from the device's reported list.
        player: String,
        /// Volume percentage, 0-100.
        volume: u8,
    },
    /// Seek one player by a relative offset.
    Seek {
        /// Player name from the device's reported list.
        player: String,
        /// Offset in microseconds. Negative rewinds.
        offset_us: i64,
    },
    /// Seek one player to an absolute position.
    SetPosition {
        /// Player name from the device's reported list.
        player: String,
        /// Position in milliseconds.
        position_ms: i64,
    },
}

/// One remote command request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteCommandRequest {
    /// Ask the peer for its command list.
    List,
    /// Run one command by its key.
    Run {
        /// Command key from the device's reported list.
        key: String,
    },
    /// Ask the peer to open its command editor.
    Setup,
    /// Ask the peer to close its command editor.
    Stop,
}

/// One input action sent to a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteInputAction {
    /// Relative pointer movement.
    Move {
        /// Horizontal delta.
        dx: i32,
        /// Vertical delta.
        dy: i32,
    },
    /// Scroll step.
    Scroll {
        /// Horizontal delta.
        dx: i32,
        /// Vertical delta.
        dy: i32,
    },
    /// One mouse click or button action.
    Click {
        /// The click to perform.
        button: RemoteClick,
    },
    /// Text typed on the peer.
    Key {
        /// Text to type.
        text: String,
        /// Modifiers held while typing.
        modifiers: RemoteInputModifiers,
    },
    /// One special key press.
    SpecialKey {
        /// Special key code, 1-32.
        code: u8,
        /// Modifiers held while pressing.
        modifiers: RemoteInputModifiers,
    },
}

/// One local command configuration action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandConfigAction {
    /// List the configured commands.
    List,
    /// Add one command with a caller-generated key.
    Add {
        /// Stable command key.
        key: String,
        /// Display name.
        name: String,
        /// Argument vector. The first element is the program.
        argv: Vec<String>,
    },
    /// Remove one command.
    Remove {
        /// Stable command key.
        key: String,
    },
}

/// One locally configured command in a control reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSummary {
    /// Stable command key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Display form of the argument vector.
    pub command: String,
}

/// One contact in a control reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSummary {
    /// Stable contact identifier from the peer.
    pub uid: String,
    /// Display name.
    pub name: String,
    /// Phone numbers.
    pub phones: Vec<String>,
    /// Email addresses.
    pub emails: Vec<String>,
}

/// One SMS operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmsAction {
    /// List the cached conversations.
    Conversations,
    /// Ask the peer for the conversation list.
    Refresh,
    /// List the cached messages in one thread.
    Thread {
        /// Conversation thread identifier.
        thread_id: i64,
    },
    /// Ask the peer for every message in one thread.
    RefreshThread {
        /// Conversation thread identifier.
        thread_id: i64,
    },
    /// Send one message.
    Send {
        /// Destination phone number.
        address: String,
        /// Message body.
        body: String,
    },
}

/// One SMS conversation in a control reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsConversationSummary {
    /// Conversation thread identifier.
    pub thread_id: i64,
    /// Phone number.
    pub address: String,
    /// Latest message body.
    pub body: String,
    /// Latest message timestamp in milliseconds.
    pub date: i64,
    /// Whether the latest message is unread.
    pub unread: bool,
}

/// One SMS message in a control reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsMessageSummary {
    /// Phone number.
    pub address: String,
    /// Message body.
    pub body: String,
    /// Message timestamp in milliseconds.
    pub date: i64,
    /// Whether the message was sent by this phone.
    pub outgoing: bool,
    /// Whether the message is read.
    pub read: bool,
}

/// Audio sinks reported by one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDeviceSinks {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Sinks in name order.
    pub sinks: Vec<RemoteSink>,
}

/// Media players reported by one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDevicePlayers {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Players in name order.
    pub players: Vec<RemotePlayer>,
}

/// Commands reported by one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDeviceCommands {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Commands in key order.
    pub commands: Vec<RemoteCommand>,
    /// Whether the peer lets the desktop add commands.
    pub can_add: bool,
}

/// This device's public identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentitySummary {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Display name.
    pub device_name: String,
    /// Device type string.
    pub device_type: String,
    /// SHA-256 of the DER certificate as hex.
    pub fingerprint: String,
    /// Advertised KDE Connect TCP port.
    pub tcp_port: u16,
}

/// A connected device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSummary {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Display name.
    pub device_name: String,
    /// Device type string.
    pub device_type: String,
    /// Whether a link to this device is open. Always `true` in `connected`.
    pub connected: bool,
}

/// A paired device from the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Display name.
    pub device_name: String,
    /// Device type string.
    pub device_type: String,
    /// Whether a link to this device is open.
    pub connected: bool,
}

/// A pairing request waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingSummary {
    pub device_id: DeviceId,
    pub device_name: String,
    /// SHA-256 of the peer certificate as hex.
    pub fingerprint: String,
}

/// An in-flight transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferSummary {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Transfer identifier assigned by the session.
    pub transfer_id: u64,
    /// Bytes moved so far.
    pub bytes: u64,
    /// Declared total, or `None` when the peer sent no size.
    pub total: Option<u64>,
}

/// One command for the daemon event loop.
#[derive(Debug)]
pub enum DaemonCommand {
    /// Queue an outbound file and reply when the transfer ends.
    Share {
        /// Paired device ID.
        device_id: DeviceId,
        /// Local source path.
        path: PathBuf,
        /// Reply sender. The daemon event loop never awaits the transfer.
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Build a status snapshot and reply.
    Status {
        /// Reply sender.
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Reply, then complete the daemon's shutdown future.
    Shutdown {
        /// Reply sender.
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Send the ring request to one connected device and reply.
    Ring {
        device_id: DeviceId,
        reply: oneshot::Sender<ControlResponse>,
    },
    RemoteVolume {
        device_id: DeviceId,
        sink: String,
        percent: Option<u8>,
        muted: Option<bool>,
        reply: oneshot::Sender<ControlResponse>,
    },
    Media {
        device_id: DeviceId,
        command: RemoteMediaCommand,
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Send a remote command request to one connected device and reply.
    RemoteCommand {
        device_id: DeviceId,
        request: RemoteCommandRequest,
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Send one input action to a connected device and reply.
    RemoteInput {
        device_id: DeviceId,
        action: RemoteInputAction,
        reply: oneshot::Sender<ControlResponse>,
    },
    /// List or refresh one device's contacts and reply.
    Contacts {
        device_id: DeviceId,
        refresh: bool,
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Run one SMS operation and reply.
    Sms {
        device_id: DeviceId,
        action: SmsAction,
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Apply one local command configuration action.
    CommandConfig {
        action: CommandConfigAction,
        reply: oneshot::Sender<ControlResponse>,
    },
    /// Answer one open pairing prompt and reply.
    PairingDecision {
        /// Device ID of the open prompt.
        device_id: DeviceId,
        /// Whether to accept the request.
        accept: bool,
        /// Fingerprint the caller saw, checked against the open prompt.
        fingerprint: String,
        /// Reply sender.
        reply: oneshot::Sender<ControlResponse>,
    },
}

/// Errors from the local control channel.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// A filesystem or stream operation failed.
    #[error("control io: {0}")]
    Io(#[from] io::Error),

    /// The frame is empty, over the cap or truncated.
    #[error("control frame: {0}")]
    Frame(String),

    /// The frame body is not valid JSON for the expected type.
    #[error("control json: {0}")]
    Json(#[from] serde_json::Error),

    /// No daemon is listening on the control endpoint.
    #[error("handfast is not running")]
    NotRunning,

    /// Another daemon already owns the control endpoint.
    #[error("another handfast is already running")]
    AlreadyRunning,

    /// The endpoint path is unsafe or unsupported.
    #[error("control endpoint: {0}")]
    Endpoint(String),

    /// A frame operation exceeded [`CONTROL_IO_TIMEOUT`].
    #[error("control timeout: {0}")]
    Timeout(&'static str),

    /// The daemon replied with an error.
    #[error("{0}")]
    Remote(String),
}

/// Entry point for the local control server.
pub struct ControlServer;

impl ControlServer {
    /// Start the local control server for `data_dir`.
    ///
    /// Unix binds `$XDG_RUNTIME_DIR/handfast/handfast.sock` when
    /// `XDG_RUNTIME_DIR` is set and non-empty, else `<data_dir>/handfast.sock`.
    /// Windows creates `\\.\pipe\handfast-<user>`.
    ///
    /// # Errors
    ///
    /// Fails when the parent is a symlink or not a directory, when the socket
    /// cannot be bound or made private, when another daemon is live on the
    /// endpoint, or when the Windows DACL cannot be built.
    pub async fn start(
        data_dir: &Path,
        commands: mpsc::Sender<DaemonCommand>,
    ) -> Result<ControlServerHandle, ControlError> {
        #[cfg(unix)]
        {
            start_unix(control_path(data_dir), commands).await
        }
        #[cfg(windows)]
        {
            let _ = data_dir;
            windows::start(commands).await
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (data_dir, commands);
            Err(ControlError::Endpoint(
                "the local control channel is not supported on this platform".to_string(),
            ))
        }
    }
}

/// A started control server. Dropping the handle without [`Self::shutdown`]
/// leaves the endpoint until the process exits.
#[derive(Debug)]
pub struct ControlServerHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: oneshot::Sender<()>,
}

impl ControlServerHandle {
    /// Stop accepting connections and wait for the accept task.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "control server task failed");
        }
    }
}

/// The Unix control socket path for `data_dir`.
///
/// `$XDG_RUNTIME_DIR/handfast/handfast.sock` when `XDG_RUNTIME_DIR` is set and
/// non-empty, else `<data_dir>/handfast.sock`.
#[cfg(unix)]
#[must_use]
pub fn control_path(data_dir: &Path) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(runtime) => PathBuf::from(runtime)
            .join("handfast")
            .join("handfast.sock"),
        None => data_dir.join("handfast.sock"),
    }
}

/// Send one request and wait for the framed response.
///
/// There is no reply deadline. A 2 GiB payload can outlive any fixed deadline,
/// so the client waits until the daemon replies or the stream closes.
///
/// # Errors
///
/// Fails with [`ControlError::NotRunning`] when no daemon listens, when the
/// frame is invalid, or when the stream closes before a full response arrives.
pub async fn send_request(
    data_dir: &Path,
    request: &ControlRequest,
) -> Result<ControlResponse, ControlError> {
    #[cfg(unix)]
    {
        send_to_path(&control_path(data_dir), request).await
    }
    #[cfg(windows)]
    {
        let _ = data_dir;
        let mut stream = windows::connect().await?;
        send_on(&mut stream, request).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (data_dir, request);
        Err(ControlError::Endpoint(
            "the local control channel is not supported on this platform".to_string(),
        ))
    }
}

#[cfg(unix)]
async fn send_to_path(
    path: &Path,
    request: &ControlRequest,
) -> Result<ControlResponse, ControlError> {
    let mut stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(ControlError::NotRunning);
        }
        Err(error) => return Err(error.into()),
    };
    send_on(&mut stream, request).await
}

async fn send_on<S>(
    stream: &mut S,
    request: &ControlRequest,
) -> Result<ControlResponse, ControlError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, request).await?;
    let body = read_frame(stream, None).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Serve one connection: read one request, forward it, write one response.
async fn serve<S>(mut stream: S, commands: mpsc::Sender<DaemonCommand>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = match read_frame(&mut stream, Some(CONTROL_IO_TIMEOUT)).await {
        Ok(body) => body,
        Err(error) => {
            tracing::debug!(%error, "control request closed without a response");
            return;
        }
    };
    let request = match serde_json::from_slice::<ControlRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            tracing::debug!(%error, "control request is not valid json");
            return;
        }
    };
    let response = dispatch(request, commands).await;
    if let Err(error) = write_frame(&mut stream, &response).await {
        tracing::debug!(%error, "failed to write the control response");
    }
}

fn validate_media_command(command: &RemoteMediaCommand) -> Result<(), String> {
    let player = match command {
        RemoteMediaCommand::RequestList => return Ok(()),
        RemoteMediaCommand::RequestStatus { player }
        | RemoteMediaCommand::Action { player, .. }
        | RemoteMediaCommand::SetVolume { player, .. }
        | RemoteMediaCommand::Seek { player, .. }
        | RemoteMediaCommand::SetPosition { player, .. } => player,
    };
    if !handfast_plugins::mprisremote::valid_player_name(player) {
        return Err("the player name is not valid".to_string());
    }
    match command {
        RemoteMediaCommand::SetVolume { volume, .. } if *volume > 100 => {
            Err("the volume is over 100".to_string())
        }
        RemoteMediaCommand::SetPosition { position_ms, .. } if *position_ms < 0 => {
            Err("the position is negative".to_string())
        }
        _ => Ok(()),
    }
}

fn validate_input_action(action: &RemoteInputAction) -> Result<(), String> {
    match action {
        RemoteInputAction::Move { dx, dy } | RemoteInputAction::Scroll { dx, dy } => {
            if dx.unsigned_abs() > 10_000 || dy.unsigned_abs() > 10_000 {
                return Err("the pointer delta is out of range".to_string());
            }
        }
        RemoteInputAction::Key { text, .. } => {
            if text.is_empty() || text.len() > 64 || text.chars().any(char::is_control) {
                return Err("the key text is not valid".to_string());
            }
        }
        RemoteInputAction::SpecialKey { code, .. } => {
            if !(1..=handfast_plugins::remotecontrol::MAX_SPECIAL_KEY).contains(code) {
                return Err("the special key code is out of range".to_string());
            }
        }
        RemoteInputAction::Click { .. } => {}
    }
    Ok(())
}

async fn dispatch(
    request: ControlRequest,
    commands: mpsc::Sender<DaemonCommand>,
) -> ControlResponse {
    let (reply, reply_rx) = oneshot::channel();
    let command = match request {
        ControlRequest::Share { device_id, path } => DaemonCommand::Share {
            device_id,
            path,
            reply,
        },
        ControlRequest::Status => DaemonCommand::Status { reply },
        ControlRequest::Shutdown => DaemonCommand::Shutdown { reply },
        ControlRequest::Ring { device_id } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            DaemonCommand::Ring { device_id, reply }
        }
        ControlRequest::RemoteVolume {
            device_id,
            sink,
            percent,
            muted,
        } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            if !handfast_plugins::remotesystemvolume::valid_sink_name(&sink) {
                return ControlResponse::Err {
                    message: "the sink name is not valid".to_string(),
                };
            }
            match (percent, muted) {
                (Some(percent), None) if percent <= 100 => {}
                (None, Some(_)) => {}
                _ => {
                    return ControlResponse::Err {
                        message: "send exactly one of percent and muted".to_string(),
                    };
                }
            }
            DaemonCommand::RemoteVolume {
                device_id,
                sink,
                percent,
                muted,
                reply,
            }
        }
        ControlRequest::Media { device_id, command } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            if let Err(message) = validate_media_command(&command) {
                return ControlResponse::Err { message };
            }
            DaemonCommand::Media {
                device_id,
                command,
                reply,
            }
        }
        ControlRequest::RemoteCommand { device_id, request } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            if let RemoteCommandRequest::Run { key } = &request {
                if !handfast_plugins::remotecommands::valid_command_key(key) {
                    return ControlResponse::Err {
                        message: "the command key is not valid".to_string(),
                    };
                }
            }
            DaemonCommand::RemoteCommand {
                device_id,
                request,
                reply,
            }
        }
        ControlRequest::RemoteInput { device_id, action } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            if let Err(message) = validate_input_action(&action) {
                return ControlResponse::Err { message };
            }
            DaemonCommand::RemoteInput {
                device_id,
                action,
                reply,
            }
        }
        ControlRequest::Contacts { device_id, refresh } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            DaemonCommand::Contacts {
                device_id,
                refresh,
                reply,
            }
        }
        ControlRequest::Sms { device_id, action } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            match &action {
                SmsAction::Thread { thread_id } | SmsAction::RefreshThread { thread_id } => {
                    if *thread_id < 0 {
                        return ControlResponse::Err {
                            message: "the thread ID is negative".to_string(),
                        };
                    }
                }
                SmsAction::Send { address, body } => {
                    if !handfast_plugins::sms::valid_address(address) {
                        return ControlResponse::Err {
                            message: "the SMS address is not valid".to_string(),
                        };
                    }
                    if body.is_empty() || !handfast_plugins::sms::valid_body(body) {
                        return ControlResponse::Err {
                            message: "the SMS body is not valid".to_string(),
                        };
                    }
                }
                SmsAction::Conversations | SmsAction::Refresh => {}
            }
            DaemonCommand::Sms {
                device_id,
                action,
                reply,
            }
        }
        ControlRequest::CommandConfig { action } => {
            match &action {
                CommandConfigAction::List => {}
                CommandConfigAction::Add { key, .. } | CommandConfigAction::Remove { key } => {
                    if !handfast_plugins::runcommand::valid_command_key(key) {
                        return ControlResponse::Err {
                            message: "the command key is not valid".to_string(),
                        };
                    }
                }
            }
            DaemonCommand::CommandConfig { action, reply }
        }
        ControlRequest::PairingDecision {
            device_id,
            accept,
            fingerprint,
        } => {
            let Ok(device_id) = DeviceId::new(&device_id) else {
                return ControlResponse::Err {
                    message: format!("{device_id} is not a device ID"),
                };
            };
            DaemonCommand::PairingDecision {
                device_id,
                accept,
                fingerprint,
                reply,
            }
        }
    };
    if commands.send(command).await.is_err() {
        return ControlResponse::Err {
            message: DAEMON_SHUT_DOWN.to_string(),
        };
    }
    match reply_rx.await {
        Ok(response) => response,
        Err(_) => ControlResponse::Err {
            message: DAEMON_SHUT_DOWN.to_string(),
        },
    }
}

/// Read one framed body. `limit` bounds the whole frame read.
async fn read_frame<S>(stream: &mut S, limit: Option<Duration>) -> Result<Vec<u8>, ControlError>
where
    S: AsyncRead + Unpin,
{
    let read = async {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await?;
        let length = frame_length(header)?;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await?;
        Ok::<Vec<u8>, ControlError>(body)
    };
    match limit {
        Some(limit) => tokio::time::timeout(limit, read)
            .await
            .map_err(|_| ControlError::Timeout("read"))?,
        None => read.await,
    }
}

/// Write one framed body. The whole frame write shares one deadline.
async fn write_frame<S, T>(stream: &mut S, value: &T) -> Result<(), ControlError>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value)?;
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(ControlError::Frame(format!(
            "body length {} is outside 1-{MAX_FRAME}",
            body.len()
        )));
    }
    let header = (body.len() as u32).to_be_bytes();
    tokio::time::timeout(CONTROL_IO_TIMEOUT, async {
        stream.write_all(&header).await?;
        stream.write_all(&body).await?;
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(|_| ControlError::Timeout("write"))??;
    Ok(())
}

fn frame_length(header: [u8; 4]) -> Result<usize, ControlError> {
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 {
        return Err(ControlError::Frame("empty frame".to_string()));
    }
    if length > MAX_FRAME {
        return Err(ControlError::Frame(format!(
            "frame {length} exceeds {MAX_FRAME} bytes"
        )));
    }
    Ok(length)
}

/// Decode one complete control frame from `data`.
///
/// A frame is a 4-byte big-endian length followed by that many body bytes.
/// Bytes after the first frame are ignored, matching the streaming reader,
/// which leaves them for the next frame.
///
/// # Errors
///
/// Fails when the header or body is truncated, the length is zero, or the
/// length exceeds [`MAX_FRAME`].
pub fn decode_frame(data: &[u8]) -> Result<Vec<u8>, ControlError> {
    let (header, rest) = data
        .split_first_chunk::<4>()
        .ok_or_else(|| ControlError::Frame("truncated header".to_string()))?;
    let length = frame_length(*header)?;
    let body = rest.get(..length).ok_or_else(|| {
        ControlError::Frame(format!(
            "body is {} bytes, frame declares {length}",
            rest.len()
        ))
    })?;
    Ok(body.to_vec())
}

#[cfg(unix)]
async fn start_unix(
    path: PathBuf,
    commands: mpsc::Sender<DaemonCommand>,
) -> Result<ControlServerHandle, ControlError> {
    prepare_parent(&path)?;
    let listener = bind_unix(&path).await?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let metadata = std::fs::symlink_metadata(&path)?;
    let owner = metadata.uid();
    let identity = SocketIdentity::of(&metadata);
    let (shutdown, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run_unix(
        listener,
        path,
        owner,
        identity,
        commands,
        shutdown_rx,
    ));
    Ok(ControlServerHandle { task, shutdown })
}

/// The socket file this server bound, so shutdown never unlinks a replacement.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
impl SocketIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    fn matches(self, path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|metadata| Self::of(&metadata) == self)
            .unwrap_or(false)
    }
}

#[cfg(unix)]
async fn bind_unix(path: &Path) -> Result<UnixListener, ControlError> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path).await {
                Ok(_) => Err(ControlError::AlreadyRunning),
                Err(probe)
                    if matches!(
                        probe.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    rebind_stale(path)
                }
                Err(probe) => Err(probe.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Remove a stale socket and bind it again.
///
/// The old socket can disappear between the failed bind and this check, so a
/// missing path is rebound instead of failing startup.
#[cfg(unix)]
fn rebind_stale(path: &Path) -> Result<UnixListener, ControlError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(ControlError::Endpoint(format!(
                    "refusing to unlink a non-socket at {}",
                    path.display()
                )));
            }
            std::fs::remove_file(path)?;
            Ok(UnixListener::bind(path)?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(UnixListener::bind(path)?),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn prepare_parent(path: &Path) -> Result<(), ControlError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            ControlError::Endpoint(format!("socket path {} has no parent", path.display()))
        })?;
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ControlError::Endpoint(format!(
                "control parent is a symlink: {}",
                parent.display()
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(ControlError::Endpoint(format!(
                "control parent is not a directory: {}",
                parent.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent)?;
        }
        Err(error) => return Err(error.into()),
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(unix)]
async fn run_unix(
    listener: UnixListener,
    path: PathBuf,
    owner: u32,
    identity: SocketIdentity,
    commands: mpsc::Sender<DaemonCommand>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let commands = commands.clone();
                        tokio::spawn(async move {
                            if !authorize_stream(&stream, owner) {
                                return;
                            }
                            serve(stream, commands).await;
                        });
                    }
                    Err(error) => tracing::warn!(%error, "control accept failed"),
                }
            }
        }
    }
    drop(listener);
    if identity.matches(&path) {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, path = %path.display(), "failed to remove the control socket");
            }
        }
    }
}

#[cfg(unix)]
fn authorize_stream(stream: &UnixStream, owner: u32) -> bool {
    match stream.peer_cred() {
        Ok(credentials) => {
            if authorize(credentials.uid(), owner) {
                true
            } else {
                tracing::warn!(
                    peer_uid = credentials.uid(),
                    owner_uid = owner,
                    "rejected control connection from another uid"
                );
                false
            }
        }
        Err(error) => {
            tracing::warn!(%error, "failed to read control peer credentials");
            false
        }
    }
}

/// Whether the connecting uid owns the control socket.
#[cfg(unix)]
fn authorize(peer_uid: u32, owner_uid: u32) -> bool {
    peer_uid == owner_uid
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fmt::Debug;
    use std::path::PathBuf;

    #[cfg(unix)]
    use handfast_desktop::{FileLimits, Files, LocalFiles};
    use handfast_proto::DeviceId;
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    #[cfg(unix)]
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    #[cfg(unix)]
    use std::time::Duration;
    #[cfg(unix)]
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    #[cfg(unix)]
    use tokio::net::{UnixListener, UnixStream};
    #[cfg(unix)]
    use tokio::sync::mpsc;

    fn device_id() -> DeviceId {
        DeviceId::new("0123456789abcdef0123456789abcdef").expect("a valid device id")
    }

    fn share_request(path: impl Into<PathBuf>) -> ControlRequest {
        ControlRequest::Share {
            device_id: device_id(),
            path: path.into(),
        }
    }

    fn sample_status() -> DaemonStatus {
        DaemonStatus {
            version: "0.2.0".to_string(),
            pid: 4242,
            uptime_secs: 17,
            identity: IdentitySummary {
                device_id: device_id(),
                device_name: "Laptop".to_string(),
                device_type: "desktop".to_string(),
                fingerprint: "ab".repeat(32),
                tcp_port: 1716,
            },
            incoming_dir: PathBuf::from("/home/me/Downloads"),
            connected: vec![DeviceSummary {
                device_id: DeviceId::new("fedcba9876543210fedcba9876543210").expect("a device id"),
                device_name: "Pixel".to_string(),
                device_type: "phone".to_string(),
                connected: true,
            }],
            paired: vec![PairedDevice {
                device_id: DeviceId::new("fedcba9876543210fedcba9876543210").expect("a device id"),
                device_name: "Pixel".to_string(),
                device_type: "phone".to_string(),
                connected: true,
            }],
            pending_pairings: vec![PairingSummary {
                device_id: DeviceId::new("00112233445566778899aabbccddeeff").expect("a device id"),
                device_name: "Tablet".to_string(),
                fingerprint: "cd".repeat(32),
            }],
            transfers: vec![TransferSummary {
                device_id: DeviceId::new("fedcba9876543210fedcba9876543210").expect("a device id"),
                transfer_id: 7,
                bytes: 1024,
                total: Some(4096),
            }],
            remote_sinks: Vec::new(),
            remote_players: Vec::new(),
            remote_commands: Vec::new(),
            truncated: false,
        }
    }

    async fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + DeserializeOwned + Debug + PartialEq,
    {
        let (mut writer, mut reader) = tokio::io::duplex(MAX_FRAME * 2);
        write_frame(&mut writer, value)
            .await
            .expect("write the frame");
        drop(writer);
        let body = read_frame(&mut reader, Some(CONTROL_IO_TIMEOUT))
            .await
            .expect("read the frame");
        serde_json::from_slice(&body).expect("decode the frame")
    }

    #[tokio::test]
    async fn frame_round_trip_preserves_request_and_response() {
        let request = share_request("/home/me/report.pdf");
        assert_eq!(round_trip(&request).await, request);

        let ok = ControlResponse::Ok { bytes: 7 };
        assert_eq!(round_trip(&ok).await, ok);
        let error = ControlResponse::Err {
            message: "device 0123456789abcdef0123456789abcdef is not connected".to_string(),
        };
        assert_eq!(round_trip(&error).await, error);
    }

    #[tokio::test]
    async fn status_and_shutdown_round_trip() {
        assert_eq!(
            round_trip(&ControlRequest::Status).await,
            ControlRequest::Status
        );
        assert_eq!(
            round_trip(&ControlRequest::Shutdown).await,
            ControlRequest::Shutdown
        );
        assert_eq!(
            round_trip(&ControlRequest::PairingDecision {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                accept: true,
                fingerprint: "ab".repeat(32),
            })
            .await,
            ControlRequest::PairingDecision {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                accept: true,
                fingerprint: "ab".repeat(32),
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::Ring {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
            })
            .await,
            ControlRequest::Ring {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::RemoteVolume {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                sink: "speakers".to_string(),
                percent: Some(40),
                muted: None,
            })
            .await,
            ControlRequest::RemoteVolume {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                sink: "speakers".to_string(),
                percent: Some(40),
                muted: None,
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::Media {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                command: RemoteMediaCommand::Action {
                    player: "mpv".to_string(),
                    action: RemoteMediaAction::Next,
                },
            })
            .await,
            ControlRequest::Media {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                command: RemoteMediaCommand::Action {
                    player: "mpv".to_string(),
                    action: RemoteMediaAction::Next,
                },
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::RemoteCommand {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                request: RemoteCommandRequest::Run {
                    key: "abc".to_string(),
                },
            })
            .await,
            ControlRequest::RemoteCommand {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                request: RemoteCommandRequest::Run {
                    key: "abc".to_string(),
                },
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::RemoteInput {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                action: RemoteInputAction::Key {
                    text: "hello".to_string(),
                    modifiers: RemoteInputModifiers {
                        alt: true,
                        ctrl: false,
                        shift: true,
                        sup: false,
                    },
                },
            })
            .await,
            ControlRequest::RemoteInput {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                action: RemoteInputAction::Key {
                    text: "hello".to_string(),
                    modifiers: RemoteInputModifiers {
                        alt: true,
                        ctrl: false,
                        shift: true,
                        sup: false,
                    },
                },
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::Contacts {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                refresh: true,
            })
            .await,
            ControlRequest::Contacts {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                refresh: true,
            }
        );
        assert_eq!(
            round_trip(&ControlResponse::Contacts {
                contacts: vec![ContactSummary {
                    uid: "uid-1".to_string(),
                    name: "Jane Doe".to_string(),
                    phones: vec!["+123".to_string()],
                    emails: vec!["jane@example.com".to_string()],
                }],
            })
            .await,
            ControlResponse::Contacts {
                contacts: vec![ContactSummary {
                    uid: "uid-1".to_string(),
                    name: "Jane Doe".to_string(),
                    phones: vec!["+123".to_string()],
                    emails: vec!["jane@example.com".to_string()],
                }],
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::Sms {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                action: SmsAction::Send {
                    address: "+123".to_string(),
                    body: "hello".to_string(),
                },
            })
            .await,
            ControlRequest::Sms {
                device_id: "0123456789abcdef0123456789abcdef".to_string(),
                action: SmsAction::Send {
                    address: "+123".to_string(),
                    body: "hello".to_string(),
                },
            }
        );
        assert_eq!(
            round_trip(&ControlResponse::SmsConversations {
                conversations: vec![SmsConversationSummary {
                    thread_id: 7,
                    address: "+123".to_string(),
                    body: "hello".to_string(),
                    date: 10,
                    unread: true,
                }],
            })
            .await,
            ControlResponse::SmsConversations {
                conversations: vec![SmsConversationSummary {
                    thread_id: 7,
                    address: "+123".to_string(),
                    body: "hello".to_string(),
                    date: 10,
                    unread: true,
                }],
            }
        );
        assert_eq!(
            round_trip(&ControlResponse::SmsThread {
                messages: vec![SmsMessageSummary {
                    address: "+123".to_string(),
                    body: "hello".to_string(),
                    date: 10,
                    outgoing: false,
                    read: true,
                }],
            })
            .await,
            ControlResponse::SmsThread {
                messages: vec![SmsMessageSummary {
                    address: "+123".to_string(),
                    body: "hello".to_string(),
                    date: 10,
                    outgoing: false,
                    read: true,
                }],
            }
        );
        assert_eq!(
            round_trip(&ControlRequest::CommandConfig {
                action: CommandConfigAction::Add {
                    key: "abc".to_string(),
                    name: "List files".to_string(),
                    argv: vec!["ls".to_string(), "-l".to_string()],
                },
            })
            .await,
            ControlRequest::CommandConfig {
                action: CommandConfigAction::Add {
                    key: "abc".to_string(),
                    name: "List files".to_string(),
                    argv: vec!["ls".to_string(), "-l".to_string()],
                },
            }
        );
        let response = ControlResponse::Commands {
            commands: vec![CommandSummary {
                key: "abc".to_string(),
                name: "List files".to_string(),
                command: "ls -l".to_string(),
            }],
        };
        assert_eq!(round_trip(&response).await, response);
        assert_eq!(
            round_trip(&ControlResponse::ShuttingDown).await,
            ControlResponse::ShuttingDown
        );
        assert_eq!(
            round_trip(&ControlResponse::Decided).await,
            ControlResponse::Decided
        );

        let response = ControlResponse::Status {
            status: Box::new(sample_status()),
        };
        assert_eq!(round_trip(&response).await, response);
    }

    #[tokio::test]
    async fn a_full_status_snapshot_fits_one_frame() {
        let mut status = sample_status();
        status.connected = (0..MAX_STATUS_DEVICES)
            .map(|index| DeviceSummary {
                device_id: DeviceId::new(format!("{index:032x}")).expect("a device id"),
                device_name: "Handfast test peer".to_string(),
                device_type: "phone".to_string(),
                connected: true,
            })
            .collect();
        status.paired = (0..MAX_STATUS_DEVICES)
            .map(|index| PairedDevice {
                device_id: DeviceId::new(format!("{index:032x}")).expect("a device id"),
                device_name: "Handfast test peer".to_string(),
                device_type: "phone".to_string(),
                connected: index % 2 == 0,
            })
            .collect();
        status.pending_pairings = (0..MAX_STATUS_DEVICES)
            .map(|index| PairingSummary {
                device_id: DeviceId::new(format!("{index:032x}")).expect("a device id"),
                device_name: "Handfast test peer".to_string(),
                fingerprint: "ab".repeat(32),
            })
            .collect();
        // Every remote section is at or above its production cap in the same
        // snapshot, so the frame bound covers the worst case.
        status.remote_sinks = (0..crate::daemon::MAX_STATUS_REMOTE_DEVICES)
            .map(|index| RemoteDeviceSinks {
                device_id: DeviceId::new(format!("{index:032x}")).expect("a device id"),
                sinks: (0..crate::daemon::MAX_STATUS_REMOTE_ENTRIES)
                    .map(|sink| RemoteSink {
                        name: format!("sink-{sink}"),
                        description: "d".repeat(crate::daemon::MAX_STATUS_TEXT_CHARS),
                        volume: 1,
                        muted: false,
                        max_volume: 100,
                        enabled: false,
                    })
                    .collect(),
            })
            .collect();
        status.remote_players = (0..crate::daemon::MAX_STATUS_REMOTE_DEVICES)
            .map(|index| RemoteDevicePlayers {
                device_id: DeviceId::new(format!("{index:032x}")).expect("a device id"),
                players: (0..crate::daemon::MAX_STATUS_REMOTE_ENTRIES)
                    .map(|player| RemotePlayer {
                        player: format!("player-{player}"),
                        title: "t".repeat(crate::daemon::MAX_STATUS_TEXT_CHARS),
                        artist: "a".repeat(crate::daemon::MAX_STATUS_TEXT_CHARS),
                        album: "b".repeat(crate::daemon::MAX_STATUS_TEXT_CHARS),
                        album_art_url: "u".repeat(crate::daemon::MAX_STATUS_TEXT_CHARS),
                        volume: 50,
                        ..RemotePlayer::default()
                    })
                    .collect(),
            })
            .collect();
        status.remote_commands = (0..crate::daemon::MAX_STATUS_REMOTE_DEVICES)
            .map(|index| RemoteDeviceCommands {
                device_id: DeviceId::new(format!("{index:032x}")).expect("a device id"),
                commands: (0..crate::daemon::MAX_STATUS_REMOTE_COMMANDS)
                    .map(|command| RemoteCommand {
                        key: format!("key-{command}"),
                        name: "n".repeat(crate::daemon::MAX_STATUS_TEXT_CHARS),
                        command: "c".repeat(crate::daemon::MAX_STATUS_TEXT_CHARS),
                    })
                    .collect(),
                can_add: false,
            })
            .collect();
        let response = ControlResponse::Status {
            status: Box::new(status),
        };
        assert_eq!(round_trip(&response).await, response);
    }

    #[tokio::test]
    async fn frame_rejects_zero_over_cap_and_truncated_lengths() {
        assert!(frame_length([0, 0, 0, 0]).is_err());
        assert!(frame_length((MAX_FRAME as u32 + 1).to_be_bytes()).is_err());
        assert_eq!(
            frame_length((MAX_FRAME as u32).to_be_bytes()).expect("the cap is accepted"),
            MAX_FRAME
        );

        let (mut writer, mut reader) = tokio::io::duplex(8);
        writer
            .write_all(&[0, 0])
            .await
            .expect("write a truncated header");
        drop(writer);
        assert!(read_frame(&mut reader, Some(CONTROL_IO_TIMEOUT))
            .await
            .is_err());
    }

    #[test]
    fn decode_frame_returns_one_body_and_rejects_short_and_over_cap_frames() {
        let body = b"{\"Share\":{}}";
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(body);
        frame.extend_from_slice(b"next");
        assert_eq!(
            decode_frame(&frame)
                .expect("decode a valid frame")
                .as_slice(),
            body.as_slice(),
            "the first frame is returned and trailing bytes are ignored"
        );

        assert!(decode_frame(&[]).is_err(), "empty input");
        assert!(decode_frame(&[0, 0, 0]).is_err(), "short header");
        assert!(decode_frame(&[0, 0, 0, 0]).is_err(), "zero length");
        assert!(
            decode_frame(&(MAX_FRAME as u32 + 1).to_be_bytes()).is_err(),
            "over the cap"
        );
        let mut truncated = 16u32.to_be_bytes().to_vec();
        truncated.extend_from_slice(b"short");
        assert!(decode_frame(&truncated).is_err(), "short body");
    }

    #[cfg(unix)]
    async fn start_at(
        path: PathBuf,
        commands: mpsc::Sender<DaemonCommand>,
    ) -> Result<ControlServerHandle, ControlError> {
        start_unix(path, commands).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_socket_is_replaced_and_a_live_socket_is_not() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).expect("bind a stale socket");
        drop(stale);
        assert!(std::fs::symlink_metadata(&path)
            .expect("the stale socket exists")
            .file_type()
            .is_socket());

        let (commands, _command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("replace the stale socket");
        assert!(std::fs::symlink_metadata(&path)
            .expect("the new socket exists")
            .file_type()
            .is_socket());

        let (commands, _command_rx) = mpsc::channel(4);
        let error = start_at(path.clone(), commands)
            .await
            .expect_err("a live socket is not replaced");
        assert!(matches!(&error, ControlError::AlreadyRunning), "{error:?}");

        server.shutdown().await;
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "graceful shutdown removes the socket"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_socket_removed_before_metadata_rebinds() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).expect("bind a stale socket");
        drop(stale);

        let bind_error = match UnixListener::bind(&path) {
            Ok(_) => panic!("the stale socket must block the bind"),
            Err(error) => error,
        };
        assert_eq!(bind_error.kind(), std::io::ErrorKind::AddrInUse);
        let probe_error = match UnixStream::connect(&path).await {
            Ok(_) => panic!("the stale socket must refuse the probe"),
            Err(error) => error,
        };
        assert!(matches!(
            probe_error.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ));

        std::fs::remove_file(&path).expect("remove the socket in the recovery window");
        let listener = rebind_stale(&path).expect("rebind after the socket disappears");
        assert!(std::fs::symlink_metadata(&path)
            .expect("the socket exists")
            .file_type()
            .is_socket());
        UnixStream::connect(&path)
            .await
            .expect("the rebound listener accepts");
        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn socket_and_parent_are_private() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir
            .path()
            .join("runtime")
            .join("handfast")
            .join("handfast.sock");
        let (commands, _command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");

        let parent = path.parent().expect("the socket has a parent");
        assert_eq!(
            std::fs::metadata(parent)
                .expect("read the parent")
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "the parent is private"
        );
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .expect("read the socket")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the socket is private"
        );
        server.shutdown().await;
    }

    #[cfg(unix)]
    #[test]
    fn peer_uid_mismatch_is_rejected_before_parsing() {
        assert!(authorize(1000, 1000));
        assert!(!authorize(1001, 1000));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_json_closes_without_a_response() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");
        let drain = tokio::spawn(async move { while command_rx.recv().await.is_some() {} });

        let mut stream = UnixStream::connect(&path).await.expect("connect");
        let body = b"{not json";
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .expect("write the length");
        stream.write_all(body).await.expect("write the body");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("the server closes the connection")
            .expect("read to eof");
        assert!(response.is_empty(), "a malformed request gets no response");

        server.shutdown().await;
        drain.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn directory_path_is_relayed_as_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let files = LocalFiles::new(dir.path().join("incoming"), FileLimits::default())
            .expect("build the files adapter");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");
        let script = tokio::spawn(async move {
            while let Some(DaemonCommand::Share {
                path: source,
                reply,
                ..
            }) = command_rx.recv().await
            {
                let message = match files.open_outgoing(&source).await {
                    Ok(_) => "unexpectedly opened a directory".to_string(),
                    Err(error) => error.to_string(),
                };
                let _ = reply.send(ControlResponse::Err { message });
            }
        });

        let response = send_to_path(&path, &share_request(dir.path().to_path_buf()))
            .await
            .expect("relay the open_outgoing error");
        match response {
            ControlResponse::Err { message } => {
                assert!(message.contains("not a regular file"), "{message}");
            }
            other => panic!("expected an error, got {other:?}"),
        }

        server.shutdown().await;
        script.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn full_session_queue_is_relayed_as_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");
        let script = tokio::spawn(async move {
            if let Some(DaemonCommand::Share { reply, .. }) = command_rx.recv().await {
                let _ = reply.send(ControlResponse::Err {
                    message: "session: session share queue full".to_string(),
                });
            }
        });

        let response = send_to_path(&path, &share_request("/home/me/report.pdf"))
            .await
            .expect("relay the queue error");
        assert!(
            matches!(&response, ControlResponse::Err { message } if message.contains("queue full")),
            "{response:?}"
        );

        server.shutdown().await;
        script.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pairing_decision_is_relayed_to_the_daemon() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");
        let expected = device_id();
        let script = tokio::spawn(async move {
            if let Some(DaemonCommand::PairingDecision {
                device_id,
                accept,
                fingerprint,
                reply,
            }) = command_rx.recv().await
            {
                assert_eq!(device_id, expected);
                assert!(accept);
                assert_eq!(fingerprint, "ab".repeat(32));
                let _ = reply.send(ControlResponse::Decided);
            }
        });

        let request = ControlRequest::PairingDecision {
            device_id: device_id().as_str().to_string(),
            accept: true,
            fingerprint: "ab".repeat(32),
        };
        let response = send_to_path(&path, &request)
            .await
            .expect("relay the pairing decision");
        assert_eq!(response, ControlResponse::Decided);

        server.shutdown().await;
        script.await.expect("the script joins");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_ring_request_is_relayed_to_the_daemon() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");
        let expected = device_id();
        let script = tokio::spawn(async move {
            if let Some(DaemonCommand::Ring { device_id, reply }) = command_rx.recv().await {
                assert_eq!(device_id, expected);
                let _ = reply.send(ControlResponse::Ok { bytes: 0 });
            }
        });

        let request = ControlRequest::Ring {
            device_id: device_id().as_str().to_string(),
        };
        let response = send_to_path(&path, &request)
            .await
            .expect("relay the ring request");
        assert_eq!(response, ControlResponse::Ok { bytes: 0 });

        server.shutdown().await;
        script.await.expect("the script joins");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_malformed_ring_device_id_is_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");

        let request = ControlRequest::Ring {
            device_id: "not-a-device".to_string(),
        };
        let response = send_to_path(&path, &request)
            .await
            .expect("the boundary replies");
        assert!(
            matches!(&response, ControlResponse::Err { message } if message.contains("is not a device ID")),
            "{response:?}"
        );
        assert!(
            command_rx.try_recv().is_err(),
            "a malformed device ID never reaches the daemon"
        );

        server.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_malformed_pairing_device_id_is_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");

        let request = ControlRequest::PairingDecision {
            device_id: "not-a-device".to_string(),
            accept: false,
            fingerprint: "ab".repeat(32),
        };
        let response = send_to_path(&path, &request)
            .await
            .expect("the boundary replies");
        assert!(
            matches!(&response, ControlResponse::Err { message } if message.contains("is not a device ID")),
            "{response:?}"
        );
        assert!(
            command_rx.try_recv().is_err(),
            "a malformed device ID never reaches the daemon"
        );

        server.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_replies_with_an_error_instead_of_hanging() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfast.sock");
        let (commands, mut command_rx) = mpsc::channel(4);
        let server = start_at(path.clone(), commands)
            .await
            .expect("start the control server");
        let script = tokio::spawn(async move {
            let _ = command_rx.recv().await;
        });

        let response = tokio::time::timeout(
            Duration::from_secs(5),
            send_to_path(&path, &share_request("/home/me/report.pdf")),
        )
        .await
        .expect("the server answers instead of hanging")
        .expect("relay the shutdown error");
        assert_eq!(
            response,
            ControlResponse::Err {
                message: DAEMON_SHUT_DOWN.to_string()
            }
        );

        server.shutdown().await;
        script.abort();
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_name_uses_the_current_user_sid() {
        let sid = handfast_desktop::winacl::current_user_sid_string().expect("a SID");
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(
            windows::pipe_name(&sid),
            format!("{}{sid}", windows::PIPE_PREFIX)
        );
    }
}
