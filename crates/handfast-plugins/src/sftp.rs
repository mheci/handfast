//! SFTP plugin.
//!
//! The phone runs the SFTP server. The desktop asks it to start with
//! `kdeconnect.sftp.request`, receives the port, user, password and root in
//! `kdeconnect.sftp`, and browses the phone with a `russh` client. The phone's
//! server decides which paths it exposes.

use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use handfast_proto::{types, Packet, ProtoError};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput, SftpEntry, SftpInfo,
};
use russh::keys::PublicKeyOrCertificate;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::roster;

const MAX_USER_BYTES: usize = 128;
const MAX_PASSWORD_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4096;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const COPY_BUFFER: usize = 64 * 1024;

/// Errors from the SFTP client.
#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    /// The request or the peer's reply is invalid.
    #[error("sftp protocol: {0}")]
    Protocol(String),
    /// A local or remote file operation failed.
    #[error("sftp io: {0}")]
    Io(#[from] io::Error),
    /// The SSH connection or authentication failed.
    #[error("sftp connection: {0}")]
    Connection(String),
    /// The SFTP client failed.
    #[error("sftp client: {0}")]
    Client(String),
    /// The file is larger than the client limit.
    #[error("the file is larger than 512 MiB")]
    TooLarge,
}

/// Asks the phone to start its SFTP server.
///
/// # Errors
///
/// Fails when the packet cannot be built.
pub fn start_request() -> Result<Packet, ProtoError> {
    Packet::new(types::SFTP_REQUEST, json!({ "startBrowsing": true }))
}

fn parse_port(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .or_else(|| value.as_str().and_then(|port| port.parse().ok()))
        .filter(|port| *port > 0)
}

fn parse_text(value: &Value, max: usize) -> Option<String> {
    let text = value.as_str()?;
    (!text.is_empty() && text.len() <= max && !text.chars().any(char::is_control))
        .then(|| text.to_string())
}

fn parse_info(body: &serde_json::Map<String, Value>) -> Option<SftpInfo> {
    if body.get("startBrowsing").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    Some(SftpInfo {
        port: parse_port(body.get("port")?)?,
        user: parse_text(body.get("user")?, MAX_USER_BYTES)?,
        password: parse_text(body.get("password")?, MAX_PASSWORD_BYTES)?,
        path: parse_text(body.get("path")?, MAX_PATH_BYTES)?,
        ip: body
            .get("ip")
            .and_then(Value::as_str)
            .and_then(|ip| ip.parse().ok()),
    })
}

struct Client {
    expected: ExpectedKey,
}

enum ExpectedKey {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ecdsa { point: Vec<u8> },
}

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // The phone reuses its TLS key as its SSH host key, so the paired
        // certificate is the only host-key authority.
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        match &self.expected {
            ExpectedKey::Rsa { n, e } => {
                let Some(rsa) = key.key_data().rsa() else {
                    return Ok(false);
                };
                Ok(strip_der_integer_padding(rsa.n().as_bytes()) == *n
                    && strip_der_integer_padding(rsa.e().as_bytes()) == *e)
            }
            ExpectedKey::Ecdsa { point } => {
                let Some(ecdsa) = key.key_data().ecdsa() else {
                    return Ok(false);
                };
                Ok(ecdsa.as_sec1_bytes() == point.as_slice())
            }
        }
    }
}

fn strip_der_integer_padding(bytes: &[u8]) -> Vec<u8> {
    let mut index = 0;
    while index + 1 < bytes.len() && bytes[index] == 0 {
        index += 1;
    }
    bytes[index..].to_vec()
}

fn expected_key(certificate: &[u8]) -> Result<ExpectedKey, SftpError> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate)
        .map_err(|error| SftpError::Protocol(format!("malformed peer certificate: {error:?}")))?;
    match certificate.public_key().parsed() {
        Ok(x509_parser::public_key::PublicKey::RSA(rsa)) => Ok(ExpectedKey::Rsa {
            n: strip_der_integer_padding(rsa.modulus),
            e: strip_der_integer_padding(rsa.exponent),
        }),
        Ok(x509_parser::public_key::PublicKey::EC(point)) => Ok(ExpectedKey::Ecdsa {
            point: point.data().to_vec(),
        }),
        _ => Err(SftpError::Protocol(
            "the peer certificate does not carry an RSA or ECDSA key".to_string(),
        )),
    }
}

/// One SFTP session with a phone.
pub struct SftpClient {
    _session: russh::client::Handle<Client>,
    sftp: SftpSession,
}

impl SftpClient {
    /// Connect to the phone's SFTP server.
    ///
    /// # Errors
    ///
    /// Fails when the TCP connection, password authentication, subsystem
    /// request or SFTP handshake fails.
    pub async fn connect(
        host: IpAddr,
        info: &SftpInfo,
        certificate: &[u8],
    ) -> Result<Self, SftpError> {
        let expected = expected_key(certificate)?;
        let config = Arc::new(russh::client::Config::default());
        let mut session = russh::client::connect(config, (host, info.port), Client { expected })
            .await
            .map_err(|error| SftpError::Connection(error.to_string()))?;
        let auth = session
            .authenticate_password(info.user.clone(), info.password.clone())
            .await
            .map_err(|error| SftpError::Connection(error.to_string()))?;
        if !auth.success() {
            return Err(SftpError::Protocol(
                "the phone rejected the SFTP password".to_string(),
            ));
        }
        let channel = session
            .channel_open_session()
            .await
            .map_err(|error| SftpError::Connection(error.to_string()))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|error| SftpError::Connection(error.to_string()))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|error| SftpError::Client(error.to_string()))?;
        Ok(Self {
            _session: session,
            sftp,
        })
    }

    /// List one remote directory.
    ///
    /// # Errors
    ///
    /// Fails when the phone rejects the path or the SFTP request fails.
    pub async fn list(&mut self, path: &str) -> Result<Vec<SftpEntry>, SftpError> {
        let entries = self
            .sftp
            .read_dir(path)
            .await
            .map_err(|error| SftpError::Client(error.to_string()))?;
        let mut list = Vec::new();
        for entry in entries {
            let metadata = entry.metadata();
            list.push(SftpEntry {
                name: entry.file_name(),
                directory: metadata.is_dir(),
                size: metadata.len(),
            });
        }
        list.sort_by(|left, right| {
            right
                .directory
                .cmp(&left.directory)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(list)
    }

    /// Copy one remote file to a local path.
    ///
    /// # Errors
    ///
    /// Fails when the remote file is a directory or too large, or when either
    /// side of the copy fails.
    pub async fn download(&mut self, remote: &str, local: &Path) -> Result<u64, SftpError> {
        let metadata = self
            .sftp
            .metadata(remote)
            .await
            .map_err(|error| SftpError::Client(error.to_string()))?;
        if metadata.is_dir() {
            return Err(SftpError::Protocol(
                "the remote path is a directory".to_string(),
            ));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(SftpError::TooLarge);
        }
        let mut remote_file = self
            .sftp
            .open(remote)
            .await
            .map_err(|error| SftpError::Client(error.to_string()))?;
        let mut local_file = tokio::fs::File::create(local).await?;
        let copied = match copy_limited(&mut remote_file, &mut local_file).await {
            Ok(copied) => copied,
            Err(error) => {
                let _ = tokio::fs::remove_file(local).await;
                return Err(error);
            }
        };
        local_file.flush().await?;
        Ok(copied)
    }

    /// Copy one local file to a remote path.
    ///
    /// # Errors
    ///
    /// Fails when the local path is not a file or is too large, or when either
    /// side of the copy fails.
    pub async fn upload(&mut self, local: &Path, remote: &str) -> Result<u64, SftpError> {
        let metadata = tokio::fs::metadata(local).await?;
        if !metadata.is_file() {
            return Err(SftpError::Protocol(
                "the local path is not a file".to_string(),
            ));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(SftpError::TooLarge);
        }
        let mut local_file = tokio::fs::File::open(local).await?;
        let mut remote_file = self
            .sftp
            .open_with_flags(
                remote,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|error| SftpError::Client(error.to_string()))?;
        let copied = match copy_limited(&mut local_file, &mut remote_file).await {
            Ok(copied) => copied,
            Err(error) => {
                let _ = self.sftp.remove_file(remote).await;
                return Err(error);
            }
        };
        remote_file
            .flush()
            .await
            .map_err(|error| SftpError::Client(error.to_string()))?;
        remote_file
            .shutdown()
            .await
            .map_err(|error| SftpError::Client(error.to_string()))?;
        Ok(copied)
    }
}

async fn copy_limited<R, W>(reader: &mut R, writer: &mut W) -> Result<u64, SftpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(total);
        }
        total = total.saturating_add(read as u64);
        if total > MAX_FILE_BYTES {
            return Err(SftpError::TooLarge);
        }
        writer.write_all(&buffer[..read]).await?;
    }
}

fn warn(message: impl Into<String>) -> PluginOutput {
    PluginOutput::new().with_event(PluginEvent::Log {
        level: LogLevel::Warn,
        message: message.into(),
    })
}

/// Turns `kdeconnect.sftp` replies into events.
#[derive(Debug, Default)]
pub struct SftpPlugin;

#[async_trait]
impl Plugin for SftpPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SFTP
    }

    async fn on_packet(
        &mut self,
        _ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::SFTP {
            return Ok(PluginOutput::new());
        }
        if let Some(message) = packet.body.get("errorMessage").and_then(Value::as_str) {
            return Ok(warn(format!("the phone refused SFTP: {message}")));
        }
        let start = packet
            .body
            .get("startBrowsing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !start {
            return Ok(PluginOutput::new().with_event(PluginEvent::SftpStopped));
        }
        match parse_info(&packet.body) {
            Some(info) => Ok(PluginOutput::new().with_event(PluginEvent::Sftp { info })),
            None => Ok(warn("the phone sent malformed SFTP details")),
        }
    }
}

/// Creates one SFTP plugin per device session.
#[derive(Debug, Clone, Copy, Default)]
pub struct SftpFactory;

impl PluginFactory for SftpFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SFTP
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(SftpPlugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;
    use russh::server::Server as _;
    use std::net::Ipv4Addr;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    #[test]
    fn sftp_longname_preserves_utc_timestamp_format() {
        for (timestamp, expected) in [
            (0, "Jan 01 1970 00:00"),
            (1_582_934_400, "Feb 29 2020 00:00"),
            (u32::MAX, "Feb 07 2106 06:28"),
        ] {
            let attrs = russh_sftp::protocol::FileAttributes {
                mtime: Some(timestamp),
                ..Default::default()
            };
            let entry = russh_sftp::protocol::File::new("sample.txt", attrs);
            assert!(entry.longname.contains(expected), "{}", entry.longname);
        }
    }

    #[test]
    fn sftp_metadata_preserves_unix_timestamp() {
        let file = tempfile::tempfile().unwrap();
        let timestamp = std::time::UNIX_EPOCH + Duration::from_secs(1_582_934_400);
        file.set_times(std::fs::FileTimes::new().set_modified(timestamp))
            .unwrap();
        let attrs = russh_sftp::protocol::FileAttributes::from(&file.metadata().unwrap());
        assert_eq!(attrs.mtime, Some(1_582_934_400));
    }

    #[cfg(unix)]
    #[test]
    fn sftp_pre_epoch_fractional_timestamp_rounds_down() {
        let file = tempfile::tempfile().unwrap();
        let timestamp = std::time::UNIX_EPOCH - Duration::from_millis(250);
        file.set_times(std::fs::FileTimes::new().set_modified(timestamp))
            .unwrap();
        let attrs = russh_sftp::protocol::FileAttributes::from(&file.metadata().unwrap());
        assert_eq!(attrs.mtime, Some(u32::MAX));
    }

    struct TestServer {
        info: SftpInfo,
        certificate: Vec<u8>,
        shutdown: russh::server::RunningServerHandle,
        task: JoinHandle<()>,
    }

    impl TestServer {
        async fn start(root: PathBuf) -> Self {
            let root = root.canonicalize().unwrap();
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let password = "test-password".to_string();
            let key = russh::keys::PrivateKey::random(
                &mut rand::rng(),
                russh::keys::Algorithm::Ecdsa {
                    curve: russh::keys::EcdsaCurve::NistP256,
                },
            )
            .unwrap();
            let mut pem = Vec::new();
            russh::keys::encode_pkcs8_pem(&key, &mut pem).unwrap();
            let pem = String::from_utf8(pem).unwrap();
            let body: String = pem
                .lines()
                .filter(|line| !line.starts_with("-----"))
                .collect();
            let der = data_encoding::BASE64.decode(body.as_bytes()).unwrap();
            let cert_key = rcgen::KeyPair::try_from(der.as_slice()).unwrap();
            let params = rcgen::CertificateParams::new(vec!["test".to_string()]).unwrap();
            let certificate = params.self_signed(&cert_key).unwrap().der().to_vec();
            let config = Arc::new(russh::server::Config {
                auth_rejection_time: Duration::from_millis(10),
                auth_rejection_time_initial: Some(Duration::ZERO),
                keys: vec![key],
                ..Default::default()
            });
            let server = TestSshServer {
                root: root.clone(),
                user: "kdeconnect".to_string(),
                password: password.clone(),
            };
            let (handle_tx, handle_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let mut server = server;
                let running = server.run_on_socket(config, &listener);
                let _ = handle_tx.send(running.handle());
                let _ = running.await;
            });
            let shutdown = handle_rx.await.unwrap();
            Self {
                info: SftpInfo {
                    port,
                    user: "kdeconnect".to_string(),
                    password,
                    path: root.to_string_lossy().into_owned(),
                    ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                },
                certificate,
                shutdown,
                task,
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.shutdown.shutdown("test done".to_string());
            self.task.abort();
        }
    }

    struct TestSshServer {
        root: PathBuf,
        user: String,
        password: String,
    }

    impl russh::server::Server for TestSshServer {
        type Handler = TestSshSession;

        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            TestSshSession {
                root: self.root.clone(),
                user: self.user.clone(),
                password: self.password.clone(),
                channels: std::collections::HashMap::new(),
            }
        }
    }

    struct TestSshSession {
        root: PathBuf,
        user: String,
        password: String,
        channels: std::collections::HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>,
    }

    impl russh::server::Handler for TestSshSession {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            user: &str,
            password: &str,
        ) -> Result<russh::server::Auth, Self::Error> {
            if user == self.user && password == self.password {
                Ok(russh::server::Auth::Accept)
            } else {
                Ok(russh::server::Auth::reject())
            }
        }

        async fn channel_open_session(
            &mut self,
            channel: russh::Channel<russh::server::Msg>,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            channel_id: russh::ChannelId,
            name: &str,
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            if name != "sftp" {
                session.channel_failure(channel_id)?;
                return Ok(());
            }
            let Some(channel) = self.channels.remove(&channel_id) else {
                session.channel_failure(channel_id)?;
                return Ok(());
            };
            session.channel_success(channel_id)?;
            russh_sftp::server::run(
                channel.into_stream(),
                TestSftpSession::new(self.root.clone()),
            )
            .await;
            Ok(())
        }
    }

    struct TestSftpSession {
        sandbox: TestSandbox,
        handles: std::collections::HashMap<String, TestHandle>,
        next_handle: u64,
    }

    enum TestHandle {
        File(std::fs::File),
        Dir {
            files: Vec<russh_sftp::protocol::File>,
            index: usize,
        },
    }

    impl TestSftpSession {
        fn new(root: PathBuf) -> Self {
            Self {
                sandbox: TestSandbox { root },
                handles: std::collections::HashMap::new(),
                next_handle: 0,
            }
        }

        fn insert(
            &mut self,
            handle: TestHandle,
        ) -> Result<String, russh_sftp::protocol::StatusCode> {
            use russh_sftp::protocol::StatusCode;
            if self.handles.len() >= 64 {
                return Err(StatusCode::Failure);
            }
            let key = format!("h{}", self.next_handle);
            self.next_handle = self.next_handle.wrapping_add(1);
            self.handles.insert(key.clone(), handle);
            Ok(key)
        }

        fn file_mut(
            &mut self,
            handle: &str,
        ) -> Result<&mut std::fs::File, russh_sftp::protocol::StatusCode> {
            use russh_sftp::protocol::StatusCode;
            match self.handles.get_mut(handle) {
                Some(TestHandle::File(file)) => Ok(file),
                _ => Err(StatusCode::BadMessage),
            }
        }
    }

    impl russh_sftp::server::Handler for TestSftpSession {
        type Error = russh_sftp::protocol::StatusCode;

        fn unimplemented(&self) -> Self::Error {
            russh_sftp::protocol::StatusCode::OpUnsupported
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            pflags: russh_sftp::protocol::OpenFlags,
            _attrs: russh_sftp::protocol::FileAttributes,
        ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
            let path = self.sandbox.resolve(
                &filename,
                pflags.contains(russh_sftp::protocol::OpenFlags::CREATE),
            )?;
            let options = std::fs::OpenOptions::from(pflags);
            let file = options.open(&path).map_err(status)?;
            let handle = self.insert(TestHandle::File(file))?;
            Ok(russh_sftp::protocol::Handle { id, handle })
        }

        async fn close(
            &mut self,
            id: u32,
            handle: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            self.handles.remove(&handle);
            Ok(ok_status(id))
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<russh_sftp::protocol::Data, Self::Error> {
            use std::io::{Read, Seek, SeekFrom};
            let file = self.file_mut(&handle)?;
            file.seek(SeekFrom::Start(offset)).map_err(status)?;
            let mut buffer = vec![0u8; len.min(64 * 1024) as usize];
            let read = file.read(&mut buffer).map_err(status)?;
            if read == 0 {
                return Err(russh_sftp::protocol::StatusCode::Eof);
            }
            buffer.truncate(read);
            Ok(russh_sftp::protocol::Data { id, data: buffer })
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            use std::io::{Seek, SeekFrom, Write};
            let file = self.file_mut(&handle)?;
            file.seek(SeekFrom::Start(offset)).map_err(status)?;
            file.write_all(&data).map_err(status)?;
            Ok(ok_status(id))
        }

        async fn lstat(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            let path = self.sandbox.resolve_nofollow(&path)?;
            let metadata = std::fs::symlink_metadata(&path).map_err(status)?;
            Ok(russh_sftp::protocol::Attrs {
                id,
                attrs: attributes(&metadata),
            })
        }

        async fn stat(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            let path = self.sandbox.resolve(&path, false)?;
            let metadata = std::fs::metadata(&path).map_err(status)?;
            Ok(russh_sftp::protocol::Attrs {
                id,
                attrs: attributes(&metadata),
            })
        }

        async fn fstat(
            &mut self,
            id: u32,
            handle: String,
        ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            let file = self.file_mut(&handle)?;
            let metadata = file.metadata().map_err(status)?;
            Ok(russh_sftp::protocol::Attrs {
                id,
                attrs: attributes(&metadata),
            })
        }

        async fn setstat(
            &mut self,
            id: u32,
            path: String,
            attrs: russh_sftp::protocol::FileAttributes,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let path = self.sandbox.resolve(&path, false)?;
            if let Some(size) = attrs.size {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(status)?;
                file.set_len(size).map_err(status)?;
            }
            if let Some(mode) = attrs.permissions {
                set_permissions(&path, mode)?;
            }
            Ok(ok_status(id))
        }

        async fn fsetstat(
            &mut self,
            id: u32,
            handle: String,
            attrs: russh_sftp::protocol::FileAttributes,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let file = self.file_mut(&handle)?;
            if let Some(size) = attrs.size {
                file.set_len(size).map_err(status)?;
            }
            if let Some(mode) = attrs.permissions {
                set_file_permissions(file, mode)?;
            }
            Ok(ok_status(id))
        }

        async fn opendir(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
            let path = self.sandbox.resolve(&path, false)?;
            let metadata = std::fs::metadata(&path).map_err(status)?;
            if !metadata.is_dir() {
                return Err(russh_sftp::protocol::StatusCode::Failure);
            }
            let mut files = vec![russh_sftp::protocol::File::new(".", attributes(&metadata))];
            let parent = path
                .parent()
                .filter(|parent| parent.starts_with(&self.sandbox.root))
                .unwrap_or(&self.sandbox.root);
            if let Ok(parent_metadata) = std::fs::metadata(parent) {
                files.push(russh_sftp::protocol::File::new(
                    "..",
                    attributes(&parent_metadata),
                ));
            }
            for entry in std::fs::read_dir(&path).map_err(status)? {
                if files.len() >= 4096 {
                    break;
                }
                let entry = entry.map_err(status)?;
                let name = entry.file_name().to_string_lossy().to_string();
                if let Ok(metadata) = entry.metadata() {
                    files.push(russh_sftp::protocol::File::new(name, attributes(&metadata)));
                }
            }
            let handle = self.insert(TestHandle::Dir { files, index: 0 })?;
            Ok(russh_sftp::protocol::Handle { id, handle })
        }

        async fn readdir(
            &mut self,
            id: u32,
            handle: String,
        ) -> Result<russh_sftp::protocol::Name, Self::Error> {
            use russh_sftp::protocol::StatusCode;
            let Some(TestHandle::Dir { files, index }) = self.handles.get_mut(&handle) else {
                return Err(StatusCode::BadMessage);
            };
            if *index >= files.len() {
                return Err(StatusCode::Eof);
            }
            let batch = files[*index..].to_vec();
            *index = files.len();
            Ok(russh_sftp::protocol::Name { id, files: batch })
        }

        async fn remove(
            &mut self,
            id: u32,
            filename: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let path = self.sandbox.resolve(&filename, false)?;
            std::fs::remove_file(&path).map_err(status)?;
            Ok(ok_status(id))
        }

        async fn mkdir(
            &mut self,
            id: u32,
            path: String,
            _attrs: russh_sftp::protocol::FileAttributes,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let path = self.sandbox.resolve(&path, true)?;
            std::fs::create_dir(&path).map_err(status)?;
            Ok(ok_status(id))
        }

        async fn rmdir(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let path = self.sandbox.resolve(&path, false)?;
            std::fs::remove_dir(&path).map_err(status)?;
            Ok(ok_status(id))
        }

        async fn realpath(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Name, Self::Error> {
            let path = self.sandbox.resolve(&path, false)?;
            Ok(russh_sftp::protocol::Name {
                id,
                files: vec![russh_sftp::protocol::File::dummy(
                    self.sandbox.relative_display(&path),
                )],
            })
        }

        async fn rename(
            &mut self,
            id: u32,
            oldpath: String,
            newpath: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let old = self.sandbox.resolve(&oldpath, false)?;
            let new = self.sandbox.resolve(&newpath, true)?;
            std::fs::rename(&old, &new).map_err(status)?;
            Ok(ok_status(id))
        }

        async fn readlink(
            &mut self,
            _id: u32,
            _path: String,
        ) -> Result<russh_sftp::protocol::Name, Self::Error> {
            Err(russh_sftp::protocol::StatusCode::OpUnsupported)
        }

        async fn symlink(
            &mut self,
            _id: u32,
            _linkpath: String,
            _targetpath: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            Err(russh_sftp::protocol::StatusCode::OpUnsupported)
        }
    }

    struct TestSandbox {
        root: PathBuf,
    }

    impl TestSandbox {
        fn relative(path: &str) -> Result<PathBuf, russh_sftp::protocol::StatusCode> {
            use russh_sftp::protocol::StatusCode;
            if path.contains('\0') {
                return Err(StatusCode::BadMessage);
            }
            let mut parts: Vec<String> = Vec::new();
            for part in path.split(['/', '\\']) {
                match part {
                    "" | "." => {}
                    ".." => {
                        parts.pop();
                    }
                    _ if part.contains(':') => return Err(StatusCode::PermissionDenied),
                    _ => parts.push(part.to_string()),
                }
            }
            Ok(parts.into_iter().collect())
        }

        fn resolve(
            &self,
            path: &str,
            allow_missing: bool,
        ) -> Result<PathBuf, russh_sftp::protocol::StatusCode> {
            use russh_sftp::protocol::StatusCode;
            let joined = self.root.join(Self::relative(path)?);
            match joined.canonicalize() {
                Ok(canonical) if canonical.starts_with(&self.root) => Ok(canonical),
                Ok(_) => Err(StatusCode::PermissionDenied),
                Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => {
                    let Some(parent) = joined.parent() else {
                        return Err(StatusCode::NoSuchFile);
                    };
                    let Some(name) = joined.file_name() else {
                        return Err(StatusCode::BadMessage);
                    };
                    let parent = parent.canonicalize().map_err(status)?;
                    if !parent.starts_with(&self.root) {
                        return Err(StatusCode::PermissionDenied);
                    }
                    Ok(parent.join(name))
                }
                Err(error) => Err(status(error)),
            }
        }

        fn resolve_nofollow(
            &self,
            path: &str,
        ) -> Result<PathBuf, russh_sftp::protocol::StatusCode> {
            use russh_sftp::protocol::StatusCode;
            let joined = self.root.join(Self::relative(path)?);
            let Some(parent) = joined.parent() else {
                return Err(StatusCode::NoSuchFile);
            };
            let Some(name) = joined.file_name() else {
                return Err(StatusCode::BadMessage);
            };
            let parent = parent.canonicalize().map_err(status)?;
            if !parent.starts_with(&self.root) {
                return Err(StatusCode::PermissionDenied);
            }
            Ok(parent.join(name))
        }

        fn relative_display(&self, canonical: &Path) -> String {
            let relative = canonical.strip_prefix(&self.root).unwrap_or(Path::new(""));
            let text = relative.to_string_lossy().replace('\\', "/");
            if text.is_empty() {
                "/".to_string()
            } else {
                format!("/{text}")
            }
        }
    }

    fn status(error: io::Error) -> russh_sftp::protocol::StatusCode {
        use russh_sftp::protocol::StatusCode;
        match error.kind() {
            io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            io::ErrorKind::AlreadyExists => StatusCode::Failure,
            _ => StatusCode::Failure,
        }
    }

    fn ok_status(id: u32) -> russh_sftp::protocol::Status {
        russh_sftp::protocol::Status {
            id,
            status_code: russh_sftp::protocol::StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        }
    }

    fn attributes(metadata: &std::fs::Metadata) -> russh_sftp::protocol::FileAttributes {
        let mut attrs = russh_sftp::protocol::FileAttributes::from(metadata);
        if metadata.file_type().is_symlink() {
            attrs.set_regular(false);
            attrs.set_symlink(true);
        }
        attrs
    }

    #[cfg(unix)]
    fn set_permissions(path: &Path, mode: u32) -> Result<(), russh_sftp::protocol::StatusCode> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(status)
    }

    #[cfg(windows)]
    fn set_permissions(path: &Path, mode: u32) -> Result<(), russh_sftp::protocol::StatusCode> {
        let mut permissions = std::fs::metadata(path).map_err(status)?.permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        std::fs::set_permissions(path, permissions).map_err(status)
    }

    #[cfg(unix)]
    fn set_file_permissions(
        file: &std::fs::File,
        mode: u32,
    ) -> Result<(), russh_sftp::protocol::StatusCode> {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(status)
    }

    #[cfg(windows)]
    fn set_file_permissions(
        file: &std::fs::File,
        mode: u32,
    ) -> Result<(), russh_sftp::protocol::StatusCode> {
        let mut permissions = file.metadata().map_err(status)?.permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        file.set_permissions(permissions).map_err(status)
    }

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    async fn plugin_output(plugin: &mut SftpPlugin, body: Value) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &handfast_desktop::Desktop::default(),
        };
        let packet = Packet::from_body(types::SFTP, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    #[test]
    fn relative_paths_stay_inside_the_root() {
        use russh_sftp::protocol::StatusCode;
        assert_eq!(
            TestSandbox::relative("..").unwrap(),
            PathBuf::new(),
            "a parent step at the root stays at the root"
        );
        assert_eq!(
            TestSandbox::relative("../../etc/passwd").unwrap(),
            PathBuf::from("etc").join("passwd"),
            "parent steps are clamped at the root"
        );
        assert_eq!(
            TestSandbox::relative("/a/b/../c").unwrap(),
            PathBuf::from("a").join("c")
        );
        assert_eq!(
            TestSandbox::relative("C:\\Windows").unwrap_err(),
            StatusCode::PermissionDenied
        );
        assert_eq!(
            TestSandbox::relative("bad\0path").unwrap_err(),
            StatusCode::BadMessage
        );
    }

    #[tokio::test]
    async fn a_real_client_lists_downloads_and_uploads() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("hello.txt"), b"hello").unwrap();
        std::fs::create_dir(directory.path().join("sub")).unwrap();
        let server = TestServer::start(directory.path().to_path_buf()).await;
        let mut client = SftpClient::connect(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &server.info,
            &server.certificate,
        )
        .await
        .expect("connect to the test server");

        let names: Vec<String> = client
            .list("/")
            .await
            .expect("list the root")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert!(names.contains(&"hello.txt".to_string()), "{names:?}");
        assert!(names.contains(&"sub".to_string()), "{names:?}");

        let local = tempfile::tempdir().unwrap();
        let downloaded = local.path().join("hello.txt");
        let bytes = client
            .download("/hello.txt", &downloaded)
            .await
            .expect("download the file");
        assert_eq!(bytes, 5);
        assert_eq!(std::fs::read(&downloaded).unwrap(), b"hello");

        let upload = local.path().join("new.txt");
        std::fs::write(&upload, b"new").unwrap();
        let bytes = client
            .upload(&upload, "/new.txt")
            .await
            .expect("upload the file");
        assert_eq!(bytes, 3);
        assert_eq!(
            std::fs::read(directory.path().join("new.txt")).unwrap(),
            b"new"
        );

        assert!(client
            .download("/sub", &local.path().join("sub"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_wrong_host_key_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let server = TestServer::start(directory.path().to_path_buf()).await;
        let other = TestServer::start(directory.path().to_path_buf()).await;

        let result = SftpClient::connect(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            &server.info,
            &other.certificate,
        )
        .await;

        assert!(result.is_err(), "a certificate for another key is refused");
    }

    #[tokio::test]
    async fn a_reply_carries_the_server_details() {
        let mut plugin = SftpPlugin;
        let output = plugin_output(
            &mut plugin,
            json!({
                "startBrowsing": true,
                "port": 1739,
                "user": "kdeconnect",
                "password": "secret",
                "path": "/storage/emulated/0"
            }),
        )
        .await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Sftp { info }]
                if info.port == 1739
                    && info.user == "kdeconnect"
                    && info.password == "secret"
                    && info.path == "/storage/emulated/0"
        ));
    }

    #[tokio::test]
    async fn a_string_port_is_accepted() {
        let mut plugin = SftpPlugin;
        let output = plugin_output(
            &mut plugin,
            json!({
                "startBrowsing": true,
                "port": "1739",
                "user": "kdeconnect",
                "password": "secret",
                "path": "/"
            }),
        )
        .await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Sftp { info }] if info.port == 1739
        ));
    }

    #[tokio::test]
    async fn a_malformed_reply_is_a_warning() {
        let mut plugin = SftpPlugin;
        let output = plugin_output(
            &mut plugin,
            json!({ "startBrowsing": true, "port": 0, "user": "kdeconnect" }),
        )
        .await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn an_error_reply_is_a_warning() {
        let mut plugin = SftpPlugin;
        let output = plugin_output(
            &mut plugin,
            json!({ "errorMessage": "storage permission missing" }),
        )
        .await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::Log {
                level: LogLevel::Warn,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_server_running_false_reply_clears_the_server() {
        let mut plugin = SftpPlugin;
        let output = plugin_output(&mut plugin, json!({ "serverRunning": false })).await;

        assert!(matches!(
            output.events.as_slice(),
            [PluginEvent::SftpStopped]
        ));
    }

    #[test]
    fn the_request_builders_match_the_protocol() {
        let start = start_request().unwrap();
        assert_eq!(start.ptype, types::SFTP_REQUEST);
        assert_eq!(start.body["startBrowsing"], true);
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(&roster::SFTP, SftpPlugin.meta());
        assert_eq!(&roster::SFTP, SftpFactory.meta());
        assert_eq!(
            roster::SFTP.status,
            handfast_runtime::PluginStatus::Implemented
        );
    }
}
