//! Minimal same-user local control channel.
//!
//! The channel carries one command, `share`, and one response. Unix uses a
//! Unix domain socket with a peer-credential check; Windows uses a named pipe
//! with a current-user DACL. The frame is a 4-byte big-endian length followed
//! by JSON, capped at 64 KiB.
//!
//! The server never touches the session map. It forwards a
//! [`DaemonCommand::Share`] to the daemon event loop and relays the framed
//! result. The path is validated by `Files::open_outgoing` inside the session,
//! so a directory or special file fails before a packet is sent.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use handfast_proto::DeviceId;
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
}

/// One response on the local control channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResponse {
    /// The transfer completed.
    Ok {
        /// Bytes written to the payload.
        bytes: u64,
    },
    /// The request failed. The message is safe to print.
    Err {
        /// Failure reason.
        message: String,
    },
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
    #[error("handfastd is not running")]
    NotRunning,

    /// Another daemon already owns the control endpoint.
    #[error("another handfastd is already running")]
    AlreadyRunning,

    /// The endpoint path is unsafe or unsupported.
    #[error("control endpoint: {0}")]
    Endpoint(String),

    /// A frame operation exceeded [`CONTROL_IO_TIMEOUT`].
    #[error("control timeout: {0}")]
    Timeout(&'static str),

    /// The daemon replied with an error. `hfctl` prints the message as is.
    #[error("{0}")]
    Remote(String),
}

/// Entry point for the local control server.
pub struct ControlServer;

impl ControlServer {
    /// Start the local control server for `data_dir`.
    ///
    /// Unix binds `$XDG_RUNTIME_DIR/handfast/handfastd.sock` when
    /// `XDG_RUNTIME_DIR` is set, else `<data_dir>/handfastd.sock`. Windows
    /// creates `\\.\pipe\handfastd-<user>`.
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
/// `$XDG_RUNTIME_DIR/handfast/handfastd.sock` when `XDG_RUNTIME_DIR` is set and
/// non-empty, else `<data_dir>/handfastd.sock`.
#[cfg(unix)]
#[must_use]
pub fn control_path(data_dir: &Path) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(runtime) => PathBuf::from(runtime)
            .join("handfast")
            .join("handfastd.sock"),
        None => data_dir.join("handfastd.sock"),
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

async fn dispatch(
    request: ControlRequest,
    commands: mpsc::Sender<DaemonCommand>,
) -> ControlResponse {
    match request {
        ControlRequest::Share { device_id, path } => {
            let (reply, reply_rx) = oneshot::channel();
            if commands
                .send(DaemonCommand::Share {
                    device_id,
                    path,
                    reply,
                })
                .await
                .is_err()
            {
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
        let path = dir.path().join("handfastd.sock");
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
        let path = dir.path().join("handfastd.sock");
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
            .join("handfastd.sock");
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
        let path = dir.path().join("handfastd.sock");
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
        let path = dir.path().join("handfastd.sock");
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
        let path = dir.path().join("handfastd.sock");
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
    async fn shutdown_replies_with_an_error_instead_of_hanging() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("handfastd.sock");
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
