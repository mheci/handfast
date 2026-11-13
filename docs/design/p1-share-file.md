# P1 share and file payload transfer

Status: design package, revision 2. No production code in this unit.

Revision 2 responds to the security and architecture findings at head
`5c1a941`. The accepted shape B and the verified payload TLS facts are
unchanged. The changes are listed in [Revision 2 changes](#revision-2-changes).

Goal: make Handfast a real KDE Connect share target and sender. A paired Android
or desktop peer must be able to send a file, text or URL to `handfastd`, and
`handfastd` must be able to send a file back, with a payload transfer that the
official clients accept. The design also defines the safe-save boundary, the
independent testkit payload peer and the P1 interop exit.

Non-goals:

- Multi-file batch progress UI. The wire metadata is parsed and emitted; the UI
  is later.
- A general IPC or full `hfctl` surface. P1 adds only the minimal `share`
  command on a local control channel; the full IPC is later.
- P2 payload consumers such as SMS attachments. The byte mover is reusable, but
  its second caller is out of scope.
- Clipboard images, SFTP and remote file browsing.
- Windows desktop file backends. The save policy is cross-platform; the Windows
  incoming and state-file ACL gap stays open.

## Problem

The link layer can authenticate a peer and carry framed JSON. It cannot move a
payload. KDE Connect share is not a JSON-only plugin. A file share sends a
`kdeconnect.share.request` packet with a declared size and a port, then the
receiver opens a second connection to that port and streams raw bytes. The
official clients wrap that second connection in TLS with the same device
certificate. A design that treats the payload as plain TCP passes a loopback
test and fails against both real clients.

The shape is non-obvious for five reasons.

1. The payload connection reverses the control link's TLS roles. On the control
   link the TCP dialer is the TLS server and the listener is the TLS client. On
   the payload the sender listens and is the TLS server; the receiver dials and
   is the TLS client. This is the conventional TLS direction, and it is the
   opposite of the control link's convention.
2. The payload port is advertised in the packet body, not in the identity. The
   receiver connects to the control link's peer address at that port. The port
   is not authenticated by the address. The TLS certificate pin is the
   authentication.
3. Empty files have no payload at all. Both official clients define
   `hasPayload()` as size not equal to zero, so an empty file is a metadata-only
   packet with a filename and no `payloadSize` or `payloadTransferInfo`.
4. The receiver's completion rule is EOF on the TLS stream, not a length-prefixed
   frame. The declared size is a check, not a frame boundary. A short transfer is
   incomplete; an over-long transfer is accepted by the desktop client and
   capped by this design.
5. The share plugin cannot own the socket. The architecture says plugins never
   own sockets and the runtime knows nothing about sockets. The byte mover needs
   a home in `handfast-net`, the save policy needs a home in `handfast-desktop`,
   and the session needs an explicit transfer intent so a 2 GiB copy never runs
   inside the 5 s plugin budget.

## Revision 2 changes

The fix round at head `5c1a941` found eight security issues and nine architecture
issues. The resolutions, in order:

1. **No-clobber commit on every path.** The check-then-rename fallback is
   replaced. The commit uses `renameat2(RENAME_NOREPLACE)` on Linux,
   `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on Windows, a hard-link
   fallback, and `create_new` plus copy only as the last resort. No path calls
   `rename` over an existing name.
2. **Dedicated temp directory.** Received temp files live in
   `<root>/.handfast-tmp`, a 0700 directory. The sweep scans only that directory,
   never the user-visible root.
3. **Free-space math.** The check reserves the declared size, the full extra-byte
   cap and the minimum reserve. A 1-byte declaration that sends 64 MiB cannot
   consume the reserve.
4. **Outbound payload timeouts.** The payload TLS handshake and idle timeouts are
   8 s, inside Android's 10 s payload socket timeout. The accept deadline starts
   at bind.
5. **Filename suffix space.** The sanitizer reserves 16 bytes for a duplicate
   suffix, trims trailing dots and spaces after truncation, and retries on
   `ENAMETOOLONG`.
6. **Reliable share delivery.** Share packets use a dedicated bounded channel,
   not the lossy `LinkEvent::Packet` path. A full share or session queue closes
   the link and emits a failure event.
7. **Minimal local control channel.** The one-shot `handfastd share` flag is
   removed. A minimal Unix socket or Windows named pipe carries one `share`
   command, and a small `hfctl` client exercises the outbound P1 exit.
8. **Eager queued connections.** A queued receive dials the payload TCP
   connection immediately and defers the TLS handshake and save until the
   previous file finishes.
9. **Windows and identity defaults.** The incoming directory uses
   `%USERPROFILE%\Downloads\Handfast` on Windows, and `--print-identity` returns
   before `LocalFiles` is created.
10. **Redaction.** Every text-bearing plugin event, including `Clipboard`, is
    logged by type and length, never by body.

The decisions from the findings file are recorded in
[Fix round decisions](#fix-round-decisions).

## Usage (caller's view)

This is the spec. The types below are derived from it.

### Daemon composition

```rust
let limits = ShareLimits::default();
let incoming_dir = args.incoming_dir.unwrap_or_else(default_incoming_dir);
let files = Arc::new(LocalFiles::new(&incoming_dir, limits.files())?);
files.sweep_stale(Duration::from_secs(24 * 3600)).await?;

let desktop = Desktop {
    files: Some(Arc::clone(&files) as Arc<dyn Files>),
    ..desktop_adapters()
};
let registry = Arc::new(handfast_plugins::registry());
let capabilities = registry.capabilities(&desktop)?; // now advertises share

let payloads = Arc::new(PayloadService::new(
    Arc::clone(&tls),
    limits.payloads(),
));
let control = ControlServer::start(control_path(), daemon_handle.clone()).await?;
```

`LocalFiles::new` creates the root with 0700 on Unix and a private
`<root>/.handfast-tmp` directory with 0700. `sweep_stale` scans only that temp
directory for files older than the threshold. `PayloadService` shares the
daemon's long-term `TlsIdentity`; it does not generate a second certificate.
`ShareLimits` is the one owner of the file size, extra-byte, free-space,
filename and queue limits; `limits.files()` and `limits.payloads()` derive the
two adapters from it.

The `--print-identity` branch returns before `LocalFiles::new` and the control
server. It keeps the existing `Desktop::default()` behavior, creates no incoming
directory and opens no control socket. The running daemon advertises share after
it composes the Files adapter.

### Inbound file from a phone

The user shares a photo from the Android app. The phone sends
`kdeconnect.share.request` with `filename`, `payloadSize` and
`payloadTransferInfo.port`. The manager routes the share packet on the reliable
share channel. The session dispatches it to the share plugin. The plugin returns
a transfer intent. The session opens a temp file in `<root>/.handfast-tmp`,
dials the payload port, streams the bytes, checks the size and commits the file
with a no-clobber commit. The user sees `photo.jpg` appear in
`~/Downloads/Handfast`.

### Inbound text and URL

Text is copied to the desktop clipboard and emitted as an event. A URL packet is
opened with the default handler only when the scheme is `http`, `https` or
`mailto`. The event carries the value for a future UI. No payload connection is
involved.

### Outbound file to a phone

```sh
hfctl share --device 0123456789abcdef0123456789abcdef /home/me/report.pdf
```

The client sends one framed JSON request to the daemon's local control socket.
The daemon looks up the paired session, opens the source, binds a payload
listener in 1739-1764, builds the packet with the actual port, sends it on the
control link, then accepts the phone's payload connection and streams the file.
The client waits for the framed result and exits non-zero on failure. A zero-byte
source sends a metadata-only packet and skips the payload listener entirely.

### Local control channel

The P1 control channel is a minimal subset of the planned IPC. It carries one
command, `share`, and one response. The transport is a Unix domain socket at
`$XDG_RUNTIME_DIR/handfast/handfastd.sock` with a peer-credential check on Unix,
and a named pipe `\\.\pipe\handfastd-<user>` with a current-user DACL on Windows.
If `XDG_RUNTIME_DIR` is unset, the socket goes under the state directory with a
0700 parent. The frame is a 4-byte big-endian length followed by JSON, with a
64 KiB cap.

```rust
// handfast-daemon::control
pub enum ControlRequest {
    Share { device_id: DeviceId, path: PathBuf },
}

pub enum ControlResponse {
    Ok { bytes: u64 },
    Err { message: String },
}

pub enum DaemonCommand {
    Share { device_id: DeviceId, path: PathBuf, reply: oneshot::Sender<ControlResponse> },
}

let response = control_client::send(
    control_path(),
    ControlRequest::Share { device_id, path },
).await?;
```

The control channel is not a network boundary. It is a same-user command path.
The peer credential check rejects a different uid on Unix, and the named-pipe
DACL rejects a different user on Windows. The path is validated by
`Files::open_outgoing`, so a directory or special file fails before the packet is
sent. The control server does not touch the session map. It forwards a
`DaemonCommand::Share` to the daemon event loop and returns the framed result;
the daemon event loop remains the only owner of sessions. The Unix transport uses
`tokio::net::UnixListener` and `UnixStream::peer_cred`. The Windows transport uses
the `interprocess` crate's local socket, which maps to a named pipe with a
current-user DACL; if `deny` rejects that crate, a narrow reviewed `windows-sys`
module builds the pipe and DACL. The full `handfast-ipc` replaces this adapter
later; the command and response types are deliberately small.

### Testkit peer

```rust
let sender = PayloadSender::new(PayloadSenderConfig {
    identity: PeerConfig::client(),
    version: TlsVersion::Tls12,
    truncate_after: None,
    pause_second_until_first_completes: false,
    receiver_timeout: Duration::from_secs(10),
})?;
let port = sender.port();
// The test sends a share packet with `port` on a control link.
let report = sender.accept_and_send(&bytes).await?;
assert_eq!(report.bytes, bytes.len());
```

`pause_second_until_first_completes` mimics the desktop sender's batch behavior.
`receiver_timeout` lets a test prove that Handfast's 8 s payload idle and
handshake timeouts fire before a 10 s receiver gives up.

The testkit sender and receiver are independent of `handfast-proto` and
`handfast-net`. They use their own frame codec, certificates and TLS configs.

## Shape

### Phase A grounding: current system

The grounding pass read the seven named crates, the architecture and parity
docs, the CI workflow, the P1 TLS design and the official KDE Connect sources
listed at the end. The table is the constraint set, not a file inventory.

| Crate | Current behavior | Constraint for this design |
| --- | --- | --- |
| `handfast-proto` | `Packet`, 1 MiB framing, identity, capabilities, pairing, `PayloadTransferInfo { port, extra }`. Transport-free by doc and dependency choice. | Owns the share schema, `PayloadSize`, `PayloadOffer`, text/URL/file builders and the update packet. Must not gain TLS, sockets or filesystem paths. |
| `handfast-net` | Discovery, TLS control links, identity re-exchange, manager, trust table, admission policy. Must not depend on store, plugins or desktop. | Owns the payload byte mover: port binding, TLS roles, peer certificate check, streaming, timeouts, cancellation, size caps. No store access. |
| `handfast-runtime` | `Plugin`, `DeviceHost`, 5 s timeout, panic isolation, `PluginOutput { packets, events }`. No sockets. | Gains one transfer intent type and `PluginOutput.transfers`. Stays transport-free. The session executes intents. |
| `handfast-plugins` | Ping, battery, clipboard. Roster has share as `Planned`. | Gains the share plugin. Parses packets, applies text/URL, returns a file intent. Never dials, never writes files. |
| `handfast-desktop` | Adapter traits for clipboard, notifications, media, volume, input, screensaver, shell. `DesktopRequirement::Files` is always false. | Gains a `Files` trait and `LocalFiles`: sanitization, dedicated temp directory, no-clobber commit, permissions, disk and size limits, duplicate names, crash sweep. `MockDesktop` is in the normal library, so `tempfile` is a normal dependency. Must not depend on protocol. |
| `handfast-daemon` | Composition root, one session per paired device, pairing prompt, shutdown. | Owns the transfer runner, the payload peer descriptor from the authenticated link, the incoming directory, the reliable share channel, the minimal control channel, transfer events, redaction and cancellation on disconnect. |
| `handfast-testkit` | Independent frame codec, Android-shaped peer, TLS peer with TLS 1.2/1.3 and RSA. Normal build has no proto or net dependency. | Gains an independent payload sender and receiver with failure knobs. The normal build stays independent. |

The stale details this design must fix:

- `DesktopRequirement::Files` maps to `false` in `desktop_available`, so the
  share plugin cannot be advertised even if a files adapter existed. The runtime
  needs an explicit `Files` mapping and a test.
- `LinkEvent::Connected` drops `LinkedPeer.remote_addr`. The payload receiver
  needs the control link's authenticated peer address; discovery is not a safe
  substitute.
- `docs/ARCHITECTURE.md` says the TCP range is 1714-1764. The real control range
  is 1716-1764 and the payload range is 1739-1764.
- The roster test `exactly_three_plugins_are_implemented_in_p0` will fail when
  share becomes implemented. It must become a count that includes share or a
  roster-derived assertion.
- `LinkEvent::Packet` uses `try_send` and drops on a full channel. A share
  request can disappear with no failure event. The share path needs a reliable
  channel or a failure contract.
- `SessionHandle::deliver` also uses `try_send`. The 65th share packet is
  dropped silently, so the design's queue bound is not enough on its own.
- `--print-identity` runs after the current `Desktop` construction. Adding
  `LocalFiles` before the branch would create and sweep the incoming directory.
- The Windows incoming default cannot use `HOME`; it is empty on Windows.
- `MockDesktop` is in the normal library, so a `tempfile` dev-dependency is not
  available to it.

### Real-client protocol facts

These are re-derived from the official sources at the commits in
[Sources read](#sources-read). No KDE code is copied.

**Packet shape.** `kdeconnect.share.request` is one of three bodies.

- File: `filename` string, optional `creationTime` and `lastModified` in
  milliseconds, optional `open` boolean, optional `numberOfFiles` and
  `totalPayloadSize`. The top-level `payloadSize` and `payloadTransferInfo`
  appear only when the file is non-empty. `payloadTransferInfo` carries `port`.
- Text: `text` string, no payload fields.
- URL: `url` string, no payload fields.
- Update: `kdeconnect.share.request.update` with `numberOfFiles` and
  `totalPayloadSize`. The desktop receiver ignores it. The Android receiver uses
  it to update a running batch. Handfast parses and emits it for both.

**Empty file.** The desktop `NetworkPacket::hasPayload()` returns
`payloadSize != 0`, and Android's `NetworkPacket.hasPayload()` returns
`payload != null && payload.payloadSize != 0`. Both send an empty file as a
metadata-only packet. The desktop receiver still creates the destination file.
Handfast must do the same.

**Payload direction and port.** The sender opens a TCP listener on the first
free port from 1739 through 1764. The desktop `CompositeUploadJob` uses
`MIN_PORT = 1739` and `MAX_PORT = 1764`. Android uses
`PAYLOAD_TRANSFER_MIN_PORT = 1739` and the same 1764 ceiling. The sender binds
all IPv4 interfaces, matching the real clients, and writes the actual port into
`payloadTransferInfo.port` before sending the packet. The receiver connects to
the control link's peer address at that port. The sender does not advertise an
address.

**Payload TLS.** The payload connection is a separate TLS session with the same
long-term device certificate. The sender is the TLS server. The receiver is the
TLS client. Both sides verify the peer certificate against the trusted device
certificate. The desktop `LanDeviceLink::dataReceived` creates a `QSslSocket`,
configures it with the peer device ID and `isDeviceTrusted = true`, and calls
`connectToHostEncrypted`. The desktop `CompositeUploadJob` configures its
accepted socket the same way and calls `startServerEncryption`. Android's
`LanLink.sendPayload` accepts a `ServerSocket`, converts it with
`clientMode = false` and `isDeviceTrusted = true`, and Android's
`LanLink.receivedNetworkPacket` connects a raw socket, converts it with
`clientMode = true`, and attaches the stream to the packet. The Android
`SslHelper` uses `SSLContext("TLSv1.2")`, so the payload also caps at TLS 1.2 for
the official Android client. The desktop uses Qt's TLS stack and can negotiate
TLS 1.3.

**Payload framing.** There is no application framing. The sender writes raw
bytes and closes the TLS stream. The receiver reads until EOF. The desktop
receiver uses a `QNetworkAccessManager` PUT to a file URL with `Content-Length`
set to the declared size when the size is non-negative. Android reads the
socket's `InputStream` into the destination. The declared size is a completion
check.

**Size mismatch.** The desktop receiver deletes the file when fewer bytes than
declared arrive. It keeps bytes beyond the declared size and logs a warning.
Android deletes when fewer bytes arrive and accepts extra bytes. Handfast matches
the short-transfer failure and caps the extra bytes.

**Timeout and cancellation.** The desktop sender waits up to 30 s for the
receiver to connect, then fails. Android waits 10 s. Android also sets a 10 s
payload socket read timeout and deletes the destination on timeout. The desktop
sender pauses accepting between files in a batch; the receiver may connect early
and the connection stays pending. The receiver's cancellation closes the payload
socket and deletes the partial destination. Handfast uses a 30 s accept deadline
that starts at bind, a 5 s connect deadline, and 8 s payload TLS handshake and
idle read/write deadlines so it fails before Android's 10 s socket timeout.

**Filename handling.** The desktop receiver strips everything before the last
`/` and rejects a destination outside the download directory. Android uses a
valid-name helper and a non-existing-name search. Neither is a complete
cross-platform sanitizer. Handfast is stricter and treats the filename as an
untrusted basename.

**Text and URL.** The desktop receiver copies text to the clipboard and offers to
open the last line when it is an `http`, `https` or `mailto` URL. A URL packet is
opened only for those schemes. Android copies text and opens any `Uri` for a URL
packet. Handfast copies text, does not auto-open text, and restricts URL packets
to the safe schemes.

### Layering rationale (why)

The v2 foundation was rebuilt in `da5aa91` with the crate table in
`docs/ARCHITECTURE.md`. Four rules are explicit.

- `handfast-proto` is "deliberately transport-free".
- `handfast-net` "knows nothing about" plugins or desktop adapters and must not
  depend on the store.
- `handfast-runtime` "knows nothing about sockets or concrete plugins".
- "Plugins never own sockets. They receive decoded packets and return outbound
  packets plus typed events."

The P1 TLS design kept TLS in `handfast-net` and rejected a runtime
`PacketTransport` trait. The audit that produced `db6ffed` deleted a certificate
module from `handfast-proto` and an unused identity table because a type without
an owner is dead weight. This design follows those decisions. It adds no
transport trait to the runtime and no socket type to a plugin. The payload byte
mover is a `handfast-net` module; the save policy is a `handfast-desktop`
adapter; the session owns transfer lifecycle; the plugin owns protocol policy.
The local control channel is a daemon adapter over `SessionHandle::share_file`;
it adds peer-credential policy and framing, not a second transfer path. That is
the constraint set used to reject candidate A.

### Wire schema

`PayloadSize` distinguishes a known byte count from the protocol's `-1` stream
marker. `PayloadOffer` is the validated pair of size and port. The parser accepts
only the share packet types and rejects a port outside 1739-1764.

```rust
// handfast-proto::share
pub const PAYLOAD_PORT_MIN: u16 = 1739;
pub const PAYLOAD_PORT_MAX: u16 = 1764;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadSize {
    Known(u64),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadOffer {
    pub size: PayloadSize,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareBatch {
    pub number_of_files: u32,
    pub total_payload_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareFile {
    pub filename: Option<String>,
    pub size: PayloadSize,
    pub created_ms: Option<i64>,
    pub modified_ms: Option<i64>,
    pub open: bool,
    pub batch: Option<ShareBatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareRequest {
    File(ShareFile),
    Text(String),
    Url(String),
}

impl ShareRequest {
    /// Parse a `kdeconnect.share.request`.
    ///
    /// # Errors
    /// Fails for another packet type, a body with no file, text or URL, a
    /// negative size other than -1, or a payload port outside 1739-1764.
    pub fn from_packet(packet: &Packet) -> Result<Self, ShareError>;

    /// Parse a `kdeconnect.share.request.update`.
    pub fn update_from_packet(packet: &Packet) -> Result<ShareBatch, ShareError>;
}

pub fn file_request(file: &ShareFile, port: Option<u16>) -> Result<Packet, ProtoError>;
pub fn text_request(text: &str) -> Result<Packet, ProtoError>;
pub fn url_request(url: &str) -> Result<Packet, ProtoError>;
pub fn update_request(batch: ShareBatch) -> Result<Packet, ProtoError>;

#[derive(Debug, Error)]
pub enum ShareError {
    #[error("not a share request")]
    NotShareRequest,
    #[error("share request has no file, text or url")]
    NothingAttached,
    #[error("invalid payload size {0}")]
    InvalidSize(i64),
    #[error("invalid payload port {0}")]
    InvalidPort(u16),
    #[error("invalid batch metadata")]
    InvalidBatch,
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),
}
```

`ShareFile.filename` is optional because the desktop receiver falls back to a
timestamp when the field is missing. The Files adapter uses `shared-file` when
the field is absent or invalid. A file packet with a filename and no payload is
an empty file. A file packet with a non-zero size and no port is rejected by the
existing `Packet::validate` rule that the two payload fields appear together.
`ShareBatch.number_of_files` must be at least 1 and `total_payload_size` may be
0. The two batch fields appear together or not at all.

`Packet` gains a validated accessor. The existing `payload()` stays for known
sizes and keeps its current error on `-1`.

```rust
impl Packet {
    /// `None` for a metadata-only packet or an empty file. `Some` for a real
    /// payload. Rejects a port outside the payload range.
    pub fn payload_offer(&self) -> Result<Option<PayloadOffer>, ProtoError>;
}
```

The builders set `numberOfFiles` and `totalPayloadSize` only when a batch is
present. `file_request` omits both payload fields for a zero-size file, matching
the official clients. For a non-empty file it calls `with_payload(size, port)`
and returns an error when the port is missing. For `Unknown` it calls
`with_payload(-1, port)`.

### Types

```rust
// handfast-runtime::transfer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    ReceiveFile {
        filename: Option<String>,
        size: PayloadSize,
        offer: Option<PayloadOffer>,
    },
}

impl PluginOutput {
    pub fn with_transfer(mut self, transfer: Transfer) -> Self;
}

// handfast-runtime::event
pub enum PluginEvent {
    ShareText { text: String },
    ShareUrl { url: String },
    ShareBatch { number_of_files: u32, total_payload_size: u64 },
    ShareProgress { transfer_id: u64, bytes: u64, total: Option<u64> },
    ShareCompleted { transfer_id: u64, path: PathBuf, bytes: u64 },
    ShareFailed { transfer_id: u64, reason: String },
}

// handfast-desktop::files
pub struct IncomingRequest {
    pub filename: Option<String>,
    pub size: Option<u64>,
    pub open: bool,
}

pub struct IncomingFile { /* root, temp dir, requested name, temp path, tokio::fs::File */ }

impl IncomingFile {
    pub fn writer(&mut self) -> &mut tokio::fs::File;
    /// Flush, sync, then commit with a no-clobber create. The primary path is
    /// an atomic no-replace rename (`renameat2` on Linux, `MoveFileExW` without
    /// replace on Windows); a hard link is the portable fallback and
    /// `create_new` plus copy is the last resort. Retries a duplicate name with
    /// ` (1)`, ` (2)` and so on.
    pub async fn commit(self) -> Result<PathBuf, FilesError>;
    /// Close and delete the temp file. Idempotent.
    pub async fn abort(self);
}

pub struct OutgoingFile {
    pub filename: String,
    pub size: u64,
    pub created_ms: Option<i64>,
    pub modified_ms: Option<i64>,
    pub reader: tokio::fs::File,
}

#[async_trait]
pub trait Files: Send + Sync {
    async fn open_incoming(&self, request: IncomingRequest) -> Result<IncomingFile, FilesError>;
    async fn open_outgoing(&self, path: &Path) -> Result<OutgoingFile, FilesError>;
    async fn sweep_stale(&self, max_age: Duration) -> Result<SweepReport, FilesError>;
}

pub struct LocalFiles { root: PathBuf, temp_dir: PathBuf, limits: FileLimits }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileLimits {
    pub max_file_size: u64,    // 2 GiB
    pub max_extra: u64,        // 64 MiB
    pub min_free_space: u64,   // 64 MiB
    pub max_name_bytes: usize, // 255
    pub suffix_reserve: usize, // 16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: usize,
}
```

`IncomingFile` is `Send` and owns the temp file. `Drop` removes the temp file
when `commit` did not run. `commit` consumes the value, so a committed file
cannot be aborted afterwards.

```rust
// handfast-net::payload
#[derive(Debug, Clone)]
pub struct PayloadPeer {
    pub device_id: DeviceId,
    pub address: SocketAddr,
    pub certificate: PeerCertificate,
}

#[derive(Debug, Clone)]
pub struct PayloadLimits {
    pub connect_timeout: Duration,    // 5 s
    pub handshake_timeout: Duration,  // 8 s, inside Android's 10 s payload timeout
    pub accept_timeout: Duration,     // 30 s, measured from bind
    pub io_timeout: Duration,         // 8 s, inside Android's 10 s payload timeout
    pub max_attempts: usize,          // 3
    pub max_size: u64,                // 2 GiB
    pub max_extra: u64,               // 64 MiB
    pub buffer_size: usize,           // 64 KiB
    pub max_active: usize,            // 8
    pub max_pending_connects: usize,  // 8, eager queued TCP connections
    pub port_min: u16,                // 1739
    pub port_max: u16,                // 1764
}

pub struct PayloadSource {
    pub size: Option<u64>,
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferOutcome {
    pub bytes: u64,
    pub extra_bytes: u64,
}

#[derive(Debug, Error)]
pub enum PayloadError {
    #[error("no payload port available")]
    NoPort,
    #[error("payload port {0} is outside 1739-1764")]
    PortOutOfRange(u16),
    #[error("payload peer certificate mismatch")]
    PeerCertificate,
    #[error("payload size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: u64, got: u64 },
    #[error("payload exceeds the {0} byte cap")]
    TooLarge(u64),
    #[error("payload timeout: {0}")]
    Timeout(&'static str),
    #[error("payload tls: {0}")]
    Tls(#[from] TlsError),
    #[error("payload io: {0}")]
    Io(#[from] std::io::Error),
}

pub struct PayloadService { /* Arc<TlsIdentity>, PayloadLimits, two semaphores */ }

pub struct PendingPayload { /* TcpStream, peer, offer, connected_at */ }

impl PayloadService {
    pub fn new(tls: Arc<TlsIdentity>, limits: PayloadLimits) -> Self;

    /// Reserve a payload port and hold the source. The caller puts `port()` in
    /// the packet, sends the packet, then calls `start`. The accept deadline is
    /// measured from bind, not from `start`.
    pub async fn prepare_send(
        &self,
        peer: PayloadPeer,
        source: PayloadSource,
    ) -> Result<PreparedSend, PayloadError>;

    /// Dial the peer's payload port and hold the raw TCP connection. The TLS
    /// handshake is deferred until `receive_pending`, so a queued file can
    /// connect before the previous file finishes.
    pub async fn connect(
        &self,
        peer: PayloadPeer,
        offer: PayloadOffer,
    ) -> Result<PendingPayload, PayloadError>;

    /// Run the TLS client handshake on a held connection and stream into `sink`.
    pub async fn receive_pending(
        &self,
        pending: PendingPayload,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError>;

    /// Convenience for the first receive: connect, then receive.
    pub async fn receive(
        &self,
        peer: PayloadPeer,
        offer: PayloadOffer,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError>;
}

pub struct PreparedSend { /* listener, source, peer, limits, bound_at */ }

impl PreparedSend {
    pub fn port(&self) -> u16;
    /// Accept one authenticated receiver and stream the source. Fails if the
    /// bind-time accept deadline has already expired.
    pub async fn start(
        self,
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError>;
}
```

`PayloadService` holds no per-transfer state except two semaphores and the
pending-connect count. One semaphore bounds active transfers at `max_active`;
the other bounds eager raw connections at `max_pending_connects`. A transfer's
state lives in its future. `prepare_send` acquires a permit before binding and
holds it in `PreparedSend`; the accept deadline is computed from `bound_at`.
`connect` acquires a pending-connect permit and holds the raw TCP connection in
`PendingPayload`; `receive_pending` starts the TLS handshake and acquires an
active-transfer permit. A full pool waits up to the accept or connect deadline
and then fails. `PreparedSend` closes its listener on drop, so a failed packet
send cannot leave a port open.

The session types:

```rust
// handfast-daemon::session
pub struct DeviceSession { /* task, handle */ }

impl DeviceSession {
    pub fn start(
        peer: Identity,
        remote_addr: SocketAddr,
        certificate: PeerCertificate,
        link: LinkHandle,
        registry: Arc<Registry>,
        desktop: Desktop,
        payloads: Arc<PayloadService>,
        events: mpsc::Sender<DaemonEvent>,
    ) -> Self;
}

impl SessionHandle {
    /// Queue an outbound file and wait for the payload to finish.
    pub async fn share_file(&self, path: PathBuf) -> Result<TransferOutcome, TransferError>;

    /// Queue one share packet on the reliable path. Waits up to
    /// `share_delivery_timeout` for session queue room. Fails when the queue is
    /// full or closed; the caller closes the link and emits `ShareFailed`.
    pub async fn deliver_share(&self, packet: Packet) -> Result<(), SessionError>;
}

enum SessionInput {
    Packet(Packet),
    ShareFile { path: PathBuf, reply: oneshot::Sender<Result<TransferOutcome, TransferError>> },
    Disconnected(String),
}

// handfast-net::manager
pub struct ShareEvent {
    pub device_id: DeviceId,
    pub packet: Packet,
}

impl LinkManager {
    pub fn start(
        listener: LinkListener,
        local: Arc<LocalEndpoint>,
        trust: TrustTable,
        limits: LinkLimits,
        events: mpsc::Sender<LinkEvent>,
        share_events: mpsc::Sender<ShareEvent>,
    ) -> (LinkManagerHandle, JoinHandle<Result<()>>);
}

impl LinkManagerHandle {
    /// Close the active link for a device. Used when a share packet cannot be
    /// delivered to the session.
    pub async fn close(&self, id: DeviceId) -> Result<(), NetError>;
}

// handfast-daemon::limits
pub struct ShareLimits {
    pub max_file_size: u64,
    pub max_extra: u64,
    pub min_free_space: u64,
    pub max_name_bytes: usize,
    pub suffix_reserve: usize,
    pub max_queued_receives: usize,
    pub share_event_queue: usize,
    pub share_delivery_timeout: Duration,
}

impl ShareLimits {
    pub fn files(&self) -> FileLimits;
    pub fn payloads(&self) -> PayloadLimits;
}
```

`TransferError` is the daemon's error type. It wraps `FilesError`, `PayloadError`,
`ProtoError` and `NetError`; it is distinct from the parser's `ShareError`.
`SessionError` is the delivery error for a full or closed session queue. The
`SessionHandle` carries the 2 s delivery timeout; both `share_file` and
`deliver_share` use it for queue admission, so a full queue returns an error
instead of hanging the control channel.

`TransferRunner` is a private struct in `session.rs`. It owns the peer descriptor,
the desktop adapters, the payload service, the event sender, one active receive,
a bounded queue of eager pending connections and a bounded queue of files whose
connect has not completed. A queued file calls `PayloadService::connect`
immediately and holds the raw TCP connection; the TLS handshake and the temp file
start only when the previous file finishes. The runner has two `JoinSet`s, one
for active receives and one for eager connects. The session loop selects on the
input queue and both sets. On session exit the runner aborts every task; each
`IncomingFile` drop deletes its temp.

### Module map

`handfast-proto`:

- `share.rs`: `PayloadSize`, `PayloadOffer`, `ShareBatch`, `ShareFile`,
  `ShareRequest`, `ShareError`, builders, `Packet::payload_offer`.
- `lib.rs`: re-export the share types and constants.

`handfast-desktop`:

- `files.rs`: `Files`, `IncomingRequest`, `IncomingFile`, `OutgoingFile`,
  `LocalFiles`, `FileLimits`, `sanitize_filename`, `commit_noclobber`,
  `sweep_stale`, `FilesError`. `LocalFiles` owns the private temp directory.
- `lib.rs`: add `files` to `Desktop` and `DesktopCapabilities`.
- `mock.rs`: a `Files` adapter backed by a `tempfile::TempDir`, plus accessors
  for the test root and saved files.
- `Cargo.toml`: add `tokio` with `fs` and `io-util`, `uuid` for temp names,
  `tempfile` as a normal dependency because `MockDesktop` is in the normal
  library, `rustix` for `renameat2` on Linux, and `windows-sys` behind the
  reviewed `MoveFileExW` wrapper on Windows.

`handfast-net`:

- `payload.rs`: `PayloadPeer`, `PayloadLimits`, `PayloadSource`, `PayloadService`,
  `PendingPayload`, `PreparedSend`, `TransferOutcome`, `PayloadError`, port
  binding, TLS server/client, peer certificate check, streaming, timeouts,
  cancellation, size caps. The `connect` and `receive_pending` split lets a
  queued file hold a raw TCP connection before its TLS handshake.
- `link.rs`: extract the `SO_REUSEADDR` and bind loop into a
  `pub(crate) fn bind_tcp_in_range(min, max)` shared by the control and payload
  listeners, and make the TLS peer-certificate extraction helper reusable by the
  payload module.
- `manager.rs`: add `remote_addr` to `LinkEvent::Connected` and `PendingLink`;
  add the `ShareEvent` channel and `LinkManagerHandle::close`.
- `lib.rs`: re-export the payload and share-event types.

`handfast-runtime`:

- `transfer.rs`: `Transfer`.
- `plugin.rs`: `PluginOutput.transfers`, `with_transfer`, and the
  `DesktopRequirement::Files` mapping to `desktop.files.is_some()`.
- `host.rs`: `Dispatch.transfers` and `absorb`.
- `plugin.rs` test: a `Files`-requiring plugin is enabled only when the desktop
  has a files adapter.

`handfast-plugins`:

- `share.rs`: `SharePlugin`, `ShareFactory`, text/URL side effects, file intent,
  update parsing.
- `roster.rs`: `SHARE` becomes `Implemented`; the implemented count test is
  updated.
- `lib.rs`: add the factory to `registry()`.

`handfast-daemon`:

- `session.rs`: `TransferRunner`, `SessionInput::ShareFile`, eager queued
  connects, transfer event forwarding, cancellation on exit,
  `SessionHandle::deliver_share`.
- `main.rs`: `--incoming-dir`, `LocalFiles`, `Desktop.files`, `PayloadService`,
  `ShareLimits`, the reliable share channel, `remote_addr` plumbing,
  `PendingLink.remote_addr`, control server shutdown, shutdown.
- `control.rs`: the minimal local control server, the `share` request, the
  peer-credential check, the named-pipe or Unix-socket transport.
- `src/bin/hfctl.rs`: the minimal client for the `share` command.
- `limits.rs`: `ShareLimits` and its `files()`/`payloads()` derivations.
- `Cargo.toml`: add `interprocess` for the Windows named pipe, or the reviewed
  `windows-sys` wrapper if `deny` rejects it.

`handfast-testkit`:

- `payload_peer.rs`: independent payload sender and receiver, TLS configs,
  failure knobs, `pause_second_until_first_completes`, `receiver_timeout`, and a
  self-test report.
- `main.rs`: `payload-self-test` subcommand.
- `frame.rs`: share packet builders that use the testkit's own `WirePacket`.
- `tests/vectors/share_url.json`, `share_file_empty.json`, `share_update.json`.

Modules stay private and the crate roots re-export the public surface. An agent
that sees only a crate root cannot import the TLS internals or the temp-file
helpers.

### Interface depth

The payload service is deep. `prepare_send` plus `PreparedSend::start`,
`connect` plus `receive_pending`, and the convenience `receive` hide the TLS
roles, port allocation, certificate checks, timeouts, size caps, retries,
cancellation and the eager queued connection. The caller supplies a peer
descriptor and a source or sink; it never sees a socket, a rustls config or a
port scan. The Files adapter is deep in the same way. One `open_incoming` call
hides the sanitizer, disk checks, the private temp directory, permissions and the
no-clobber commit; `commit` and `abort` are the only lifecycle methods. The share
plugin is shallow by design. It parses a packet and maps it to a side effect or a
transfer intent; it holds no policy the other modules need to learn. The session
is the only place that sees the plugin intent, the Files adapter and the payload
service, and it exposes one `share_file` method for the outbound direction. The
control channel is a shallow adapter over `share_file`; it adds peer-credential
policy and framing, not a second transfer implementation.

The complexity that remains exposed is deliberate. The session must choose when
to start a transfer and how to queue it, because that is lifecycle policy. The
Files adapter exposes `IncomingFile` with a writer because the payload service
must stream into it without owning the file. The plugin exposes `Transfer`
because the runtime must carry the intent without knowing about sockets. Every
other decision is hidden behind one of those three surfaces.

### Data flow

Inbound file:

```text
phone
  |  control link: kdeconnect.share.request { filename, payloadSize, port }
  v
link manager (paired gate) -> reliable ShareEvent channel (32)
  -> daemon event loop -> SessionHandle::deliver_share
  -> DeviceHost.dispatch -> share plugin -> Transfer::ReceiveFile
  -> TransferRunner:
       active receive: files.open_incoming -> IncomingFile in <root>/.handfast-tmp
       payloads.connect(peer, offer) -> PendingPayload (raw TCP)
       payloads.receive_pending(pending, incoming.writer)
         TLS client handshake (local cert, peer pin)
         read raw bytes -> write temp, 64 KiB buffer
         EOF -> short/extra size check
       incoming.commit() -> fsync -> hard_link or create_new+copy -> remove temp
  -> DaemonEvent::PluginEvents { ShareProgress | ShareCompleted | ShareFailed }
```

Outbound file:

```text
hfctl share -> local control socket -> ControlServer -> DaemonCommand::Share
  -> daemon event loop -> SessionHandle::share_file
  -> TransferRunner: files.open_outgoing -> OutgoingFile
  -> zero size? proto::file_request(file, None) -> link.send -> reply Ok(0)
  -> payloads.prepare_send(peer, source) -> bind 1739..1764 -> PreparedSend
       bound_at = Instant::now()
  -> proto::file_request(file, Some(prepared.port()))
  -> link.send(packet)
  -> prepared.start()  // accept deadline = bound_at + 30 s
       accept TCP, TLS server handshake (requires client cert)
       check client cert against the control link pin
       read source, write raw bytes, close TLS
  -> reply resolves with TransferOutcome
```

Text and URL:

```text
share.request { text } -> reliable share channel -> share plugin -> Clipboard::write_text -> ShareText event
share.request { url }  -> reliable share channel -> share plugin -> scheme check -> Shell::open_url -> ShareUrl event
share.request.update   -> reliable share channel -> share plugin -> ShareBatch event
```

Empty file:

```text
share.request { filename } -> share plugin -> Transfer::ReceiveFile { size: Known(0), offer: None }
  -> TransferRunner: files.open_incoming -> commit (no payload connection)
```

Queued receive:

```text
first file active, second share.request arrives
  -> TransferRunner: payloads.connect(peer, second_offer) -> PendingPayload
       raw TCP connection held, no TLS, no temp file
  -> first file completes -> files.open_incoming(second) -> payloads.receive_pending(pending, ...)
```

Event delivery is best effort for progress. The transfer task sends at most one
`ShareProgress` event per 500 ms or 4 MiB with `try_send`; a full daemon queue
drops the progress, not the transfer. `ShareCompleted` and `ShareFailed` use a
bounded `send` and are logged if the daemon has stopped. The daemon logs every
text-bearing event by type and length, never by body.

### Share packet delivery contract

Share packets are not sent through the lossy `LinkEvent::Packet` path. The
manager owns a dedicated `ShareEvent` channel with 32 slots. When a paired link
receives `kdeconnect.share.request` or `kdeconnect.share.request.update`, the
manager tries the share channel. On success the packet is delivered to the
daemon event loop. On a full channel the manager closes the link with
`LinkEvent::Disconnected` and a `"share event queue full"` reason. The peer sees
the control link close, so it does not wait for a payload connection that will
never come.

The daemon delivers a `ShareEvent` to the session with
`SessionHandle::deliver_share`. That call waits up to
`ShareLimits.share_delivery_timeout` (2 s) for room in the session's 64-slot
queue. On a full or closed queue the daemon stops the session, closes the link
with `LinkManagerHandle::close`, and emits a `ShareFailed` event with transfer id
0 because no transfer was created. The daemon event loop is the only consumer of
the share channel, so a slow session cannot silently drop a share.

The contract is: a share packet is either queued for the plugin or the control
link closes with an observable failure event within
`ShareLimits.share_delivery_timeout` (2 s). There is no third outcome. Ordinary
plugin packets keep the existing best-effort `try_send` path; they do not need a
payload connection to complete.

### Trust and security model

**The payload is authenticated by TLS, not by peer address.** The official
clients use a separate mutual-TLS session with the same long-term certificate.
The sender is the TLS server and requires a client certificate. The receiver is
the TLS client and presents its certificate. Both sides check the peer
certificate against the trusted device certificate. Handfast does the same.

- The expected peer is the control link's `PeerCertificate`. The payload
  service checks the payload peer certificate with
  `PeerCertificate::check(&peer.device_id, PeerTrust::Pinned(peer.certificate.fingerprint()))`.
  A different certificate with the same common name fails the pin.
- The payload connection uses the same `TlsIdentity` as the control link. No
  second certificate and no second trust store.
- The receiver dials the control link's authenticated `remote_addr` IP at the
  advertised port. The port must be in 1739-1764. A malicious paired peer cannot
  use the payload path to scan arbitrary local ports.
- The peer address is routing, not authentication. A host that knows the port
  can connect, but it cannot complete the TLS handshake without the pinned
  private key. Source-IP filtering is not used as the gate.
- The payload port is open for at most 30 s from bind, not from `start`. A
  delayed packet send cannot extend the listener's exposure. A failed handshake
  is discarded and the listener retries, up to three attempts. A LAN attacker
  can still deny the transfer by exhausting the attempts; this is a denial of
  service, not an impersonation.
- Unpaired peers never reach the share plugin. The manager gate drops every
  packet except `kdeconnect.pair` from an unpaired link, and the daemon creates
  no session. The payload service only receives a `PayloadPeer` from a paired
  session. This is tested at the manager and daemon levels.
- The payload TLS offers TLS 1.2 and TLS 1.3. TLS below 1.2 is rejected. Android
  negotiates 1.2 because its `SSLContext` caps it; the desktop can negotiate
  1.3.
- The outbound payload TLS handshake and idle timeouts are 8 s. Android's payload
  socket timeout is 10 s, so Handfast fails before the receiver deletes the file
  and cannot report success after the receiver has given up.
- The share parser rejects a negative size other than -1, a port outside the
  payload range, a missing body, and a payload field without its pair. It never
  panics on a malformed packet.
- URL packets are opened only for `http`, `https` and `mailto`. `file:`,
  `javascript:`, `data:` and unknown schemes are logged and dropped. Text is
  copied, not executed and not auto-opened.
- The file save boundary is the only writer of a received path. The plugin
  never sees a filesystem path. The session hands the Files adapter the raw
  filename; the adapter sanitizes it and never joins a user-controlled path
  component.
- Every commit path is no-clobber. The primary is an atomic no-replace rename
  (`renameat2` on Linux, `MoveFileExW` without replace on Windows), the fallback
  is a hard link, and the last resort is `create_new` plus copy. No path calls
  `rename` over an existing name. A symlink, directory or existing file at the
  destination causes the next suffix, never a replacement.
- Received temp files live in `<root>/.handfast-tmp`, a 0700 directory. The
  sweep scans only that directory. A pre-existing user file in the incoming root
  is never deleted, even if it matches the temp pattern.
- The free-space check reserves the declared size, the full 64 MiB extra
  allowance and the 64 MiB minimum reserve. A 1-byte declaration that sends
  64 MiB cannot consume the reserve.
- The share packet path is reliable or it fails loudly. A full share or session
  queue closes the link and emits `ShareFailed`; there is no silent drop.
- The local control channel is a same-user boundary. Unix checks the peer uid;
  Windows uses a current-user DACL on the named pipe. The path is validated by
  `open_outgoing` before any packet is sent.
- Logs contain the peer device ID, transfer bytes, final path and failure
  reason. They never contain file contents, clipboard text or private key
  material. The daemon logs every text-bearing event, including `Clipboard`,
  `ShareText` and `ShareUrl`, by type and length, never by body.
- The Windows incoming directory and state-file ACLs remain a P1 release
  blocker. The incoming directory inherits the user profile ACL; the state file
  gap is unchanged.

### Safe file save

`LocalFiles` is the single owner of the received path. It applies these rules.

**Root.** The root is `--incoming-dir`, default `$XDG_DOWNLOAD_DIR/Handfast` or
`~/Downloads/Handfast` on Unix. On Windows the default is
`%USERPROFILE%\Downloads\Handfast`, with `%LOCALAPPDATA%\handfast\incoming` as
the fallback when the profile path is unavailable. The root is created with 0700
on Unix and checked to be a real directory, not a symlink. An existing root keeps
its permissions; the daemon does not chmod a user-chosen directory. A symlinked
root is rejected, so point `--incoming-dir` at the real path.

The private temp directory is `<root>/.handfast-tmp`, created with 0700 and
checked to be a real directory. If that path exists as a file or symlink, the
daemon fails startup with a clear error; it does not delete or replace it. All
received temp files live only in this directory. The sweep scans only this
directory. No user file in the incoming root is ever swept or deleted.

**Filename.** The raw name is treated as an untrusted basename.

1. Take the substring after the last `/` or `\`. This makes an absolute path or
   a traversal sequence a basename. `../../etc/passwd` becomes `passwd`.
2. Replace C0 and C1 control characters, bidi control characters, and the
   Windows-invalid characters `: * ? " < > |` with `_`.
3. Trim trailing dots and spaces. Reject `.` and `..`.
4. If the stem (before the first dot) is a Windows reserved name (`CON`, `PRN`,
   `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, case-insensitive), prefix `_`.
5. Reserve the temp directory name. If the result equals `.handfast-tmp`,
   prefix `_`.
6. Limit the UTF-8 name to `max_name_bytes - suffix_reserve` bytes, for example
   255 - 16 = 239. This leaves room for ` (99)` before the extension. Truncate
   at a character boundary and keep the extension when possible.
7. Trim trailing dots and spaces again after truncation. A truncated name can
   end in a dot or a space.
8. If the result is empty, use `shared-file`. If a fallback is needed and the
   original had an extension, keep it.

The commit loop handles `ENAMETOOLONG` by shortening the stem by the suffix
length and retrying. If no candidate fits, it fails with `NameTooLong`; it never
truncates the final path silently.

The sanitizer is a pure function with a table test. It is not a shell command and
does not use a regex.

**Temp and commit.** `open_incoming` creates a random temp file in
`<root>/.handfast-tmp` named `<uuid>.part` with `create_new` and mode 0600. The
final name is not created yet. On commit:

1. `File::sync_all` flushes the file, then drop the file handle.
2. On Linux, `renameat2(temp, root/name, RENAME_NOREPLACE)` commits atomically
   and fails with `EEXIST` when the name exists. On Windows,
   `MoveFileExW(temp, root/name, 0)` without `MOVEFILE_REPLACE_EXISTING` does the
   same. The temp and the destination are in the same root, so they are on the
   same filesystem.
3. On `Unsupported`, `ENOSYS` or `EINVAL` for the no-replace rename, try
   `hard_link(temp, root/name)`. A hard link is also atomic and no-clobber; remove
   the temp after the link.
4. On `AlreadyExists` or `EEXIST`, try `name (1).ext`, `name (2).ext` and so on,
   up to 99.
5. On `ENAMETOOLONG`, shorten the stem and retry from step 2.
6. Only when the filesystem supports neither no-replace rename nor hard links,
   use `create_new` on the destination and copy the temp bytes into it, then sync
   and close. `create_new` is the no-clobber reservation; a concurrent writer
   that wins the name causes `AlreadyExists` and the next suffix. If the copy
   fails, remove the newly-created destination because this process owns it, then
   abort. This last-resort path is no-clobber but is not crash-atomic: a crash
   during the copy can leave a partial final file.
7. On Unix, sync the root directory after the commit. Remove the temp file if it
   still exists. Return the final path.

The no-replace rename uses `rustix` on Linux and a narrow reviewed `windows-sys`
wrapper on Windows. The workspace's `unsafe_code = deny` needs the same reviewed
exception as the Windows ACL work; the wrapper contains only the
`MoveFileExW` call and its flag conversion.

**Overwrite.** Never overwrite on any path. A duplicate name gets a suffix. A
symlink at the destination fails the no-replace rename, the hard link or
`create_new`, and the next suffix is tried. A directory at the destination also
fails and gets a suffix. Two concurrent commits of the same name are serialized
by the filesystem's no-clobber operation; one gets the base name and the other
gets a suffix.

**Permissions.** The temp directory is 0700, temp files are 0600 and final files
are 0600 on Unix. On Windows the file inherits the user profile ACL; the incoming
and state-file ACL gap is a P1 release blocker and is called out in the open
risks.

**Disk and size.** Before creating the temp, check the declared size against
`max_file_size` (2 GiB default) and check free space against
`declared + max_extra + min_free_space`, for example `size + 64 MiB + 64 MiB`.
The extra allowance is part of the reservation because a lying peer can declare
1 byte and send 64 MiB. An unknown size reserves
`max_file_size + min_free_space`. A write error, including `ENOSPC`, aborts and
deletes the temp. The payload service also caps the bytes it reads at
`max_size`, so the reservation and the read cap agree. Free space comes from a
small platform helper. The recommended dependency is `fs4` because it is safe and
cross-platform; if the workspace rejects it, use `statvfs` and
`GetDiskFreeSpaceEx` behind a reviewed safe wrapper.

**Concurrency and duplicates.** `open_incoming` uses a unique temp name per
transfer. The commit loop is the only name allocator. Two transfers with the
same filename both commit; one gets the base name and the other gets a suffix.
The session runs one active receive per peer and queues up to 8 eager TCP
connections. Different peers can run concurrently under the global payload cap.
The eager connection is a raw TCP socket; the TLS handshake and the temp file
start only when the previous file finishes.

**Crash cleanup.** `sweep_stale` scans only `<root>/.handfast-tmp` at startup,
removes regular `.part` files older than 24 h, and never follows a symlink.
`IncomingFile::Drop` also removes its temp on a normal abort or unwind. A crash
can leave a `.part` file in the temp directory; the next startup removes it. A
crash during the atomic no-replace rename or hard-link path leaves no partial
final. A crash during the last-resort `create_new` copy can leave a partial final;
the sweep cannot safely identify that file as Handfast's, so it is a documented
limit of that fallback.

**Outbound validation.** `open_outgoing` follows the user's path, requires a
regular file, checks readability and size, and rejects directories and special
files. A zero-byte file is valid; it takes the metadata-only packet path and
never binds a payload port. The caller is trusted, but a device file or a
directory is still a mistake worth failing early.

### Failure modes

| Failure | Detection | Behavior |
| --- | --- | --- |
| No payload port free | bind fails across 1739-1764 | outbound share fails with `NoPort`; no packet is sent |
| Packet send delayed past the accept deadline | bind-time deadline | `PreparedSend::start` fails, listener closes, no payload bytes |
| Receiver never connects | 30 s accept deadline from bind | sender closes the listener and reports failure; receiver gets no file |
| Connect refused or timed out | 5 s connect deadline | receive fails, temp deleted, `ShareFailed` event |
| TLS handshake fails | 8 s deadline or rustls error | transfer aborted, no file, security log |
| Idle payload stalls | 8 s read/write deadline | transfer aborted before Android's 10 s receiver timeout |
| Wrong payload certificate | pin or CN check | abort, no file, security log; listener retries up to 3 |
| Client certificate missing | TLS server requires client auth | handshake fails, no file |
| Raw TCP peer | TLS handshake fails | no file; the peer is not an official client |
| Truncated payload | bytes < declared | temp deleted, `SizeMismatch` |
| Extra payload | bytes > declared | keep up to 64 MiB extra with a warning; beyond that abort and delete |
| Unknown size | `payloadSize = -1` | read to EOF or the 2 GiB cap; cap breach aborts |
| Lying size and free space | `declared + max_extra + min_free_space` check | reject before writing; the 64 MiB extra cannot consume the reserve |
| Disk full | write error or free-space check | temp deleted, `Io` |
| Traversal or absolute path | sanitizer | basename only; never outside the root |
| Symlink at destination | no-replace rename, hard link or `create_new` fails | next suffix; never follows the link |
| Duplicate name | no-replace rename, hard link or `create_new` `AlreadyExists` | ` (1)`, ` (2)` suffix |
| Concurrent duplicate commits | filesystem no-clobber operation | one base name, one suffix; no overwrite, no lost file |
| Name at the length limit plus suffix | suffix reserve and `ENAMETOOLONG` retry | shorter stem, suffix fits, or `NameTooLong` |
| Crash during last-resort copy | `create_new` + copy on a filesystem without atomic no-replace | partial final can remain; documented limit, no safe automatic delete |
| Temp directory is a file or symlink | startup check | fatal startup error; the path is not deleted or replaced |
| Crash cleanup | startup sweep | only `<root>/.handfast-tmp` `.part` files older than 24 h removed |
| Concurrent receive | eager connect queue | one active receive, up to 8 raw TCP connections queued, then reject with an event |
| Queued file waits for the first | eager TCP connect | connection held without TLS or temp file; handshake starts when the first finishes |
| Disconnect mid-transfer | session exit | abort tasks, drop `IncomingFile`, delete temp |
| Share event channel full | 32-slot `ShareEvent` channel | close the link with `share event queue full`; peer sees the close |
| Session share queue full | `deliver_share` error | stop the session, close the link, emit `ShareFailed` |
| Control channel wrong uid or user | Unix peer credential or Windows DACL | reject the command, no packet sent |
| Empty file inbound | no payload fields | create and commit an empty file |
| Empty file outbound | zero-byte source | metadata-only packet, no payload listener |
| Text with no clipboard adapter | `desktop.clipboard` is `None` | log warning, still emit `ShareText` |
| URL with no shell adapter | `desktop.shell` is `None` | log warning, still emit `ShareUrl` |
| URL with unsafe scheme | scheme check | log and drop, no shell call |
| Malformed share packet | parser error | log warning, no plugin failure, no panic |
| Unpaired share packet | manager gate | dropped before the session |
| Port outside 1739-1764 | parser or service check | reject, no connect |
| Cancelled by user or session | oneshot or task abort | abort, delete temp, `ShareFailed` |
| Outbound source changed | short read | sender reports failure; receiver sees a short transfer and deletes |
| Log body leak | redaction test | no clipboard text, share text or URL body in the log |

### Limits

The defaults are chosen for a LAN desktop and are configurable.

| Limit | Default | Why |
| --- | --- | --- |
| Payload size | 2 GiB | Bounds a lying peer; real share files are smaller |
| Extra bytes | 64 MiB | Matches the desktop's keep-extra behavior without an unbounded write |
| Connect timeout | 5 s | LAN dial; a dead peer fails fast |
| Payload TLS handshake | 8 s | Inside Android's 10 s payload socket timeout |
| Accept deadline | 30 s from bind | Matches the desktop sender; bind time cannot be extended by a queued packet |
| Payload idle read/write | 8 s | Inside Android's 10 s payload socket timeout |
| Buffer | 64 KiB | Enough for LAN throughput, small enough for memory |
| Active payloads | 8 global | Bounds sockets and disk writers |
| Pending eager connects | 8 global | Bounds raw TCP connections held for queued files |
| Receive queue | 8 per peer | Handles a desktop batch without unbounded memory |
| Share event queue | 32 | Dedicated reliable share path |
| Session queue | 64 | Existing session bound; a full queue closes the link with a failure |
| Share delivery timeout | 2 s | Bounded failure instead of a silent drop |
| File name | 255 bytes | Filesystem limit |
| Suffix reserve | 16 bytes | Leaves room for ` (99)` and the extension |
| Free-space reserve | 64 MiB plus extra cap | A lying size cannot consume the reserve |
| Incoming root | 0700 | Private by default |
| Temp directory | 0700 | Private, swept separately from the root |
| Temp file | 0600 | Private until committed |

`max_size` is the absolute cap including extra bytes. A declared size near the
2 GiB cap plus extra bytes beyond the cap fails, even though the extra allowance
is 64 MiB. The service stops reading at `max_size`. The free-space check reserves
`declared + max_extra + min_free_space`; the extra cap and the reserve are both
part of the same check.

## Synthesis decision

Arena ran three seats: candidate A (link-attached payload streams), candidate B
(session-executed share intents with a `handfast-net` payload service) and
candidate C (a dedicated `handfast-transfer` actor and crate). Model diversity
was not established. Only one model is configured, so the seats ran serially on
the same model. The synthesis is a judgment call, not a cross-model consensus.

Red-flag screen against `references/design-red-flags.md`:

| Candidate | Red flags | Verdict |
| --- | --- | --- |
| A, link-attached streams | Information leakage (stream type in runtime), split ownership (link, plugin, desktop), temporal decomposition, plugin timeout, two ways to handle a payload, shallow stream interface | Reject as base; graft eager start |
| B, session-executed intents | No blocking flag. Watch `PluginOutput` growth, session orchestration scope and the payload service becoming a second stack | Base |
| C, dedicated transfer actor | Split ownership with the link manager, two ways to start a transfer, broad actor interface, plugin layer gains a network dependency | Reject as base; graft registry and state machine |

Base: **candidate B**. It is the only candidate that keeps all four existing
layering rules at once. The plugin stays a pure packet translator, the runtime
gains one intent type, `handfast-net` owns the byte mover, and
`handfast-desktop` owns the save. The public surface is `prepare_send` plus
`PreparedSend::start`, `connect` plus `receive_pending`, and the convenience
`receive`, plus one intent variant. The complexity behind that surface is TLS,
ports, timeouts, certificate checks, size caps, the private temp directory and
the no-clobber commit.

Grafted from candidate A:

- Start the receive task immediately after dispatch. A queued file dials the
  raw payload connection immediately and defers the TLS handshake and save, so
  the sender's 30 s accept window is never the bottleneck.
- Derive the payload peer from the control link's authenticated certificate and
  `remote_addr`, not from discovery.
- Use the exact real-client TLS roles: sender is TLS server, receiver is TLS
  client, mutual auth, pinned certificate.

Grafted from candidate C:

- One transfer registry per session with a bounded queue, a global payload
  semaphore and cancellation on disconnect. This is the single-owner property
  without a new crate.
- An explicit save state machine, Open to Receiving to Committing to Done or
  Aborted, with idempotent abort and a no-clobber commit that uses an atomic
  no-replace rename, a hard-link fallback and a `create_new` plus copy last
  resort.

Rejected from A: a `PayloadStream` in `PluginContext` and long I/O inside
`on_packet`. It leaks transport into the runtime, splits one transfer across
three layers and breaks the 5 s plugin budget.

Rejected from C: a new `handfast-transfer` crate and actor. It duplicates the
TLS configuration, port binding and peer certificate policy that
`handfast-net` already owns, and it gives the plugin layer a network
dependency. Revisit it when a second payload consumer exists.

All three candidates agreed on the real-client payload facts: a separate
connection, port 1739-1764, sender listens, receiver dials, TLS with the pinned
certificate, raw bytes, EOF completion, empty files without a payload. That
agreement is the strongest signal in the arena. The divergence was ownership,
not protocol.

The fix round at `5c1a941` kept candidate B. The commit path, temp directory,
timeouts, delivery contract and outbound trigger changed. The candidate verdicts
did not. The new control channel is an adapter over the accepted `share_file`
surface, not a fourth candidate.

## Fix round decisions

The findings file's decisions are encoded as follows.

- **Outbound trigger.** A minimal local control channel, not a one-shot
  `handfastd share` flag. The channel carries one `share` command. The outbound
  real-client check runs through `hfctl share` against the running daemon.
- **Incoming directory.** `$XDG_DOWNLOAD_DIR/Handfast`, else
  `~/Downloads/Handfast`; on Windows `%USERPROFILE%\Downloads\Handfast` with a
  `%LOCALAPPDATA%` fallback; 0700; `--incoming-dir` override.
- **Extra bytes.** Accept the declared size, cap extra at 64 MiB, fail beyond and
  delete the temp. The free-space check includes the extra cap and the reserve.
- **Batches.** Per-file payload ports.
- **`open` flag.** Parsed but ignored until a shell adapter exists.
- **Text that looks like a URL.** No auto-open.
- **Windows ACLs.** The incoming directory and state-file ACLs remain a P1
  release blocker.

## Tradeoffs accepted

- We accept a new `Transfer` command on `PluginOutput` in exchange for keeping
  the plugin free of sockets and file I/O. The command list is one variant for
  P1; a second variant is a deliberate future change.
- We accept a per-session transfer runner in the daemon in exchange for one
  owner of transfer lifecycle and cancellation. The session grows a module, not
  branches in its packet loop.
- We accept an atomic no-replace rename primary (`renameat2` or `MoveFileExW`
  without replace), a hard-link fallback, and a `create_new` plus copy last
  resort in exchange for a no-clobber commit on every filesystem without unsafe
  code. The last resort is not crash-atomic; a crash during its copy can leave a
  partial final, and the sweep cannot safely delete it.
- We accept a private temp directory and a separate sweep in exchange for never
  deleting a user file in the incoming root. The temp directory is one extra
  path component and one extra 0700 directory.
- We accept a dedicated reliable share channel and link close on a full queue in
  exchange for a share packet that is never silently lost. A slow consumer loses
  the link instead of the file.
- We accept eager raw TCP connections for queued files in exchange for meeting
  the desktop sender's accept window. The raw connection holds no TLS state and
  no temp file until its turn.
- We accept 8 s payload handshake and idle timeouts in exchange for failing
  before Android's 10 s receiver timeout. A very slow but live link may fail
  earlier than a 30 s timeout would.
- We accept a minimal local control channel in exchange for an outbound P1 exit
  that can reach the running daemon. The channel is one command and one
  response; the full IPC replaces it later.
- We accept a 2 GiB size cap and a 64 MiB extra-byte cap in exchange for bounded
  disk use against a lying paired peer. A real client that sends more than
  64 MiB beyond its declaration is treated as hostile.
- We accept a dedicated incoming subdirectory instead of the user's Downloads
  root in exchange for 0700 permissions, easy cleanup and a smaller blast
  radius. The path is configurable.
- We accept best-effort text and URL handling when the host lacks a clipboard or
  shell adapter in exchange for not disabling the whole share plugin. The file
  path still works.
- We accept TLS 1.2 on the payload for Android compatibility even though the
  desktop can do 1.3. The control link already makes the same trade.
- We accept that a LAN attacker can deny a payload transfer by exhausting the
  accept attempts. The pin prevents impersonation; availability is a separate
  problem.

## Alternatives considered

**Link-attached payload streams (candidate A).** The link dials the payload and
attaches a stream to the packet. The plugin copies the stream to a file. This
matches the real clients' eager connection and hides the port from the plugin.
It loses because a transport stream type crosses into `handfast-runtime`, the
plugin does long I/O under a 5 s budget, and one transfer is split across the
link, plugin and desktop layers. The eager-start property is grafted into the
base without the stream type.

**Dedicated transfer actor and crate (candidate C).** A new crate owns ports,
TLS, temp files and the state machine. The plugin calls a handle. This gives the
strongest single owner and is easy to test in isolation. It loses because it
duplicates the TLS and trust policy already in `handfast-net`, adds a second
actor that must coordinate disconnects with the link manager, and gives
`handfast-plugins` a network dependency. The registry and state machine are
grafted into the session.

**Raw TCP payload.** Some third-party protocol summaries describe the payload as
plain TCP. The official desktop and Android implementations both wrap it in TLS
with the trusted device certificate. A raw TCP design would pass a loopback test
and fail against every official client. Rejected by evidence.

**Multiplexing the payload on the control link.** The protocol uses a second
connection and a port in the packet body. The official clients open a new
socket. Multiplexing would be wire-incompatible. Rejected by evidence.

**Plugin-owned payload service.** The plugin would hold a `PayloadService` in
its context and spawn the transfer task. This removes the session runner but
gives the plugin a network dependency and makes cancellation on disconnect the
plugin's job. Rejected by the layering rules.

**One-shot `handfastd share` flag.** A second process cannot open the same redb
store, and a second data directory has a different identity and trust table. The
flag cannot reach the running daemon. Rejected. The minimal local control channel
replaces it.

**Check-then-rename fallback.** `symlink_metadata` followed by `rename` replaces
an existing file on Unix and on Windows with `MOVEFILE_REPLACE_EXISTING`. A file
created between the check and the rename is destroyed. Rejected. The commit uses
`renameat2(RENAME_NOREPLACE)` or `MoveFileExW` without replace, then a hard link,
then `create_new` plus copy as the last resort.

**Temp files in the user-visible root.** A user file that matches the temp
pattern is deleted by the sweep. Rejected. Temp files live in a private
`<root>/.handfast-tmp` directory.

**Lossy share packet delivery.** The existing `LinkEvent::Packet` path drops on
a full channel. A share request then waits for a payload connection that never
comes. Rejected. Share packets use a dedicated reliable channel and close the
link on overflow.

**Queue without eager connect.** A queued file that dials only when the previous
file finishes misses the sender's accept window on a large first file. Rejected.
A queued file holds a raw TCP connection and defers the TLS handshake.

## Test plan

Every test names the behavior a user or a maintainer can observe. The testkit
normal build stays independent of `handfast-proto` and `handfast-net`.

### `handfast-proto`

- File request round trip: filename, size, port, timestamps, `open`, batch.
- Text and URL round trips.
- Empty file has no payload fields and parses as `Known(0)` with no offer.
- `payloadSize = -1` parses as `Unknown`.
- Negative size other than -1 is rejected.
- Port 0 and port 1765 are rejected; 1739 and 1764 are accepted.
- `payloadTransferInfo` without `payloadSize` is rejected by `validate`.
- Unknown body fields are preserved.
- Update packet parses and builds.
- Malformed bodies do not panic.
- Golden vectors: `share_file.json`, `share_text.json`, `share_url.json`,
  `share_file_empty.json`, `share_update.json`.

### `handfast-desktop`

- Sanitizer table: traversal, absolute Unix path, absolute Windows path,
  backslash, reserved name, reserved name with extension, trailing dot, trailing
  space, control characters, bidi controls, empty, `.`, `..`, 300-byte name,
  Unicode name, `.handfast-tmp`.
- A name at the length limit plus a duplicate suffix fits and commits. A name
  that still returns `ENAMETOOLONG` retries with a shorter stem or fails with
  `NameTooLong`.
- Truncation followed by a trailing dot or space is trimmed again.
- `open_incoming` creates a temp in `<root>/.handfast-tmp`, 0600 on Unix, and
  does not create the final name.
- `commit` creates the final file, removes the temp, and leaves no partial file.
- Duplicate commit gets ` (1)` and ` (2)`.
- Two concurrent commits of the same name both succeed; one gets the base name,
  one gets a suffix; no file is overwritten or lost. This is the no-clobber
  concurrent duplicate test.
- The Linux `renameat2(RENAME_NOREPLACE)` path, the Windows `MoveFileExW` path,
  the hard-link fallback and the `create_new` copy last resort each pass the
  duplicate test and never replace an existing file. A test-only commit strategy
  selector exercises each path.
- A symlink or directory at the final name is not followed and gets a suffix.
- Short write or abort deletes the temp.
- The free-space check rejects `declared + max_extra + min_free_space` when free
  space is smaller. A 1-byte declaration that would send 64 MiB is rejected when
  the reserve is not available.
- `max_file_size` rejects a larger declaration.
- `sweep_stale` removes old `.part` files only from `<root>/.handfast-tmp` and
  keeps young files and a pre-existing user file in the root.
- A `.handfast-tmp` file or symlink in the root fails startup; it is not deleted
  or replaced.
- `open_outgoing` rejects a directory, a special file and an unreadable file; a
  zero-byte regular file is accepted.
- The Windows default resolves to `%USERPROFILE%\Downloads\Handfast`, with the
  `%LOCALAPPDATA%` fallback. The Unix default resolves `$XDG_DOWNLOAD_DIR` then
  `~/Downloads`.
- Unix permission test is `cfg(unix)`; the Windows job builds and runs the
  cross-platform tests.
- The normal `handfast-desktop` library build includes `MockDesktop` and its
  `tempfile` dependency; no dev-dependency-only path is needed.

### `handfast-runtime`

- `DesktopRequirement::Files` maps to `desktop.files.is_some()`. A share-style
  plugin is enabled and advertised only when the desktop has a files adapter.
- The existing clipboard mapping test still passes.
- `PluginOutput.transfers` survives `Dispatch::absorb` without touching the
  packet or event lists.

### `handfast-net`

Using the testkit payload peer:

- Receive happy path: 1 byte, 1 MiB and 8 MiB.
- Send happy path: 1 byte, 1 MiB and 8 MiB.
- TLS 1.2 and TLS 1.3 success.
- Wrong certificate, wrong CN, missing client certificate and raw TCP all fail.
- Truncated payload fails and the sink is not committed.
- Extra payload within 64 MiB is kept and reported; beyond the cap fails.
- `Unknown` size reads to EOF; a cap breach fails.
- Connect refused, connect timeout, accept timeout and TLS handshake timeout.
- A receiver that waits 10 s then closes makes Handfast's 8 s idle or handshake
  timeout fire first; Handfast does not report success after the receiver has
  deleted the file.
- Port outside 1739-1764 is rejected before connect.
- The service dials the `PayloadPeer.address` IP and the offer port. A packet
  cannot supply an address.
- Cancellation drops the stream and leaves no bytes.
- Concurrent transfers respect the global semaphore.
- A `PreparedSend` dropped before `start` closes its port.
- A `PreparedSend` whose packet send is delayed past the bind-time accept
  deadline fails in `start` and leaves no listener. This is the backed-up
  control link test.
- `connect` holds a raw TCP connection for a queued file; `receive_pending`
  starts the TLS handshake later. A large first file with a queued second file
  completes both.
- A failed TLS attempt does not consume the whole accept deadline; the next
  valid client succeeds.

### `handfast-plugins`

- Text packet writes the clipboard and emits `ShareText`.
- Text with no clipboard adapter logs and still emits `ShareText`.
- URL packet opens `http`, `https` and `mailto` and emits `ShareUrl`.
- `file:` and `javascript:` URLs are dropped with a log.
- File packet returns `Transfer::ReceiveFile` with the right filename, size and
  offer.
- Empty file returns `Known(0)` and no offer.
- Update packet emits `ShareBatch`.
- Malformed packet logs and does not fail the plugin.
- The plugin is only enabled when the Files adapter is present.
- Roster test reflects four implemented plugins and the share capability.

### `handfast-daemon`

- A paired testkit peer sends a file; the file appears atomically in the
  incoming dir; progress and completion events arrive.
- A paired testkit peer sends text and a URL; clipboard and shell mocks record
  them.
- An unpaired peer's share packet never reaches the plugin.
- A truncated payload leaves no file and emits `ShareFailed`.
- A disconnect mid-transfer cancels the task and removes the temp.
- Two files queued from one peer are received in order. The second raw TCP
  connection is established before the first file finishes.
- A large first file with a queued second file completes both without the
  sender's accept timeout firing.
- A duplicate filename gets a suffix.
- A full share event channel closes the link with a `share event queue full`
  reason and emits a failure.
- A full session share queue stops the session, closes the link and emits
  `ShareFailed`.
- Outbound share to a testkit receiver sends the packet, streams the bytes and
  resolves the reply.
- Outbound empty file sends a metadata-only packet and never binds a payload
  port.
- Outbound with no free payload port fails before sending the packet.
- Shutdown aborts active transfers and leaves no `.part` file.
- `--print-identity` creates no incoming directory and no `.handfast-tmp`
  directory.
- The daemon log for a `Clipboard`, `ShareText` or `ShareUrl` event contains the
  type and length, not the text or URL body. The log-body test asserts the
  secret string is absent.

### `handfast-daemon control`

- `hfctl share --device <id> <path>` round-trips a framed request and response
  over the Unix socket.
- A different uid on Unix is rejected before the command is parsed.
- A wrong or missing device ID returns an error and sends no packet.
- A directory or special file returns an error before the packet is sent.
- The control socket is 0600 and its parent directory is 0700 on Unix.
- The Windows named pipe uses a current-user DACL. The Windows CI job builds the
  control module; the DACL check is manual until a second local user is
  available.

### `handfast-testkit`

- Payload sender and receiver self-test over loopback.
- The self-test uses the testkit's own frame codec and TLS configs.
- Failure knobs: truncate, extra, wrong certificate, missing client certificate,
  raw TCP, accept timeout, slow read, `pause_second_until_first_completes`, and
  `receiver_timeout`.
- A receiver with a 10 s timeout is still waiting when Handfast's 8 s idle or
  handshake timeout fires; Handfast reports failure, and the receiver later
  deletes the file.
- The normal build has no `handfast-proto` or `handfast-net` dependency. The
  conformance test in `tests/` may use the production codec as a consumer.

### CI

The nine required jobs stay the same. The testkit self-test job adds
`cargo run -p handfast-testkit -- payload-self-test`. The fuzz job adds a
`share_request` target over `ShareRequest::from_packet`. The test job runs the
daemon control tests and the log-redaction test. The Windows job builds the
workspace, runs the `handfast-net` and `handfast-desktop` tests, and builds the
control module, including the payload tests and the cross-platform save tests.
No job uses `continue-on-error`.

## Implementation sequence

Each unit ends in a check. A unit is not started until the previous check passes.

1. **Share schema in proto.** `PayloadSize`, `PayloadOffer`, `ShareBatch`,
   `ShareFile`, `ShareRequest`, `ShareError`, builders, `Packet::payload_offer`,
   golden vectors. Check: `cargo test -p handfast-proto` and
   `cargo run -p handfast-testkit -- vectors` lists the new vectors.
2. **Files adapter and safe save.** `Files`, `LocalFiles`, sanitizer with suffix
   reserve and `ENAMETOOLONG`, private temp directory, no-replace rename with a
   hard-link fallback and `create_new` plus copy last resort, permissions,
   free-space math, sweep, `MockDesktop` files, Windows incoming default. Check:
   `cargo test -p handfast-desktop` on Linux and the Windows job.
3. **Independent testkit payload peer.** `payload_peer.rs`, sender and receiver,
   failure knobs, `pause_second_until_first_completes`, `receiver_timeout`,
   self-test, `payload-self-test` command. Check:
   `cargo test -p handfast-testkit` and
   `cargo run -p handfast-testkit -- payload-self-test`.
4. **Payload service in net.** Port binding, TLS server and client, certificate
   check, streaming, 8 s timeouts, bind-time accept deadline, `connect` and
   `receive_pending`, cancellation, size caps, shared bind helper. Check:
   `cargo test -p handfast-net` with the testkit peer for TLS 1.2/1.3, happy,
   truncated, wrong certificate, raw TCP, timeout, cancel, concurrent and the
   backed-up control link.
5. **Transfer intent in runtime.** `Transfer`, `PluginOutput.transfers`,
   `Dispatch.transfers`, share events, and the `DesktopRequirement::Files`
   mapping with its test. Check: `cargo test -p handfast-runtime`.
6. **Share plugin.** Text, URL, file intent, update, registry and roster.
   Check: `cargo test -p handfast-plugins` and the roster count test.
7. **Manager peer address and share channel.** `LinkEvent::Connected.remote_addr`
   and `PendingLink.remote_addr`; the `ShareEvent` channel;
   `LinkManagerHandle::close`. Check: `cargo test -p handfast-net` and
   `cargo check -p handfast-daemon`.
8. **Session transfer runner, inbound.** Open incoming, eager queued connect,
   spawn receive, progress, completion, failure, session delivery, cancellation
   on disconnect, temp cleanup, and redaction of every text-bearing event.
   Check: daemon session tests with the testkit sender, including unpaired gate,
   duplicate name, full share channel and full session queue.
9. **Outbound share path.** `SessionHandle::share_file`, prepare, packet send,
   start, empty file, port unavailable, reply. Check: daemon test with the
   testkit receiver and a large file.
10. **Local control channel.** `ControlServer`, `hfctl share`, Unix peer
    credential check, Windows named pipe, framed JSON, wrong device and invalid
    path errors. Check: daemon control tests and a manual `hfctl` run against a
    live daemon.
11. **Daemon composition and docs.** `--incoming-dir`, `LocalFiles`,
    `Desktop.files`, `PayloadService`, `ShareLimits`, reliable share channel,
    startup sweep, shutdown, INTEROP, ARCHITECTURE, PARITY, CI testkit command.
    Check: full daemon integration test, nine CI checks green, manual interop
    checklist ready.

The sequence is ordered so each layer can be tested without the one above it.
Units 1 to 4 have no daemon dependency. Unit 5 has no network dependency. Units
6 to 10 add one integration point at a time.

## Manual interop checklist update

Add a "Share and file transfer" section to `docs/INTEROP.md`.

Prerequisites:

- A paired Android phone and a paired KDE Connect desktop client.
- Firewall TCP 1716-1764 open on the Handfast host. The control link needs
  1716-1764, and the payload uses 1739-1764 inside that range.
- A fresh `--data-dir` and a disposable `--incoming-dir`.
- `tcpdump -i any -s 0 -w share.pcap 'tcp portrange 1716-1764'`.

Android to Handfast:

1. Share a photo to Handfast. Confirm the log shows `share.request`, the
   filename, the declared size and the payload port. Confirm the file appears in
   the incoming dir with the original name and no `.part` remains.
2. Share text. Confirm the text is on the desktop clipboard and a `ShareText`
   event is logged. Confirm the log line does not contain the text body.
3. Share an `https` URL. Confirm the browser opens it and a `ShareUrl` event is
   logged. Confirm the log line does not contain the URL body.
4. Share a `file://` URL if the app allows it. Confirm Handfast drops it with a
   log and opens nothing.
5. Share an empty file. Confirm a zero-byte file appears.
6. Share a file larger than 100 MiB. Confirm progress and a complete file.
7. Cancel a transfer from the phone. Confirm the `.part` is deleted and a
   `ShareFailed` event is logged.
8. Share two files at once. Confirm both arrive, the second gets a suffix if the
   names collide, and no file is partial.
9. Confirm the payload TLS version in the log is TLS 1.2 for Android. Confirm
   Handfast's payload idle and handshake timeouts are 8 s, inside Android's 10 s
   payload socket timeout.

Desktop to Handfast:

1. Share a file from the KDE Connect desktop client. Confirm the same behavior.
2. Share text and an `https` URL. Confirm the same behavior.
3. Confirm the payload TLS version is TLS 1.3 when the desktop offers it.
4. Send a large first file and a second file queued behind it. Confirm the second
   payload connection is established early, both files arrive in order, and no
   file is partial.

Handfast to a real client:

1. Start `handfastd` with the control socket enabled.
2. Run `hfctl share --device <peer-id> /path/to/file`.
3. Confirm the phone or desktop receives the file and `hfctl` exits zero.
4. Run `hfctl share` with a zero-byte file. Confirm the peer creates an empty
   file and Handfast binds no payload port.
5. Run `hfctl share` with a directory or a special file. Confirm it exits
   non-zero and sends no packet.
6. Run `hfctl share` as a different local user on Unix. Confirm the peer
   credential check rejects it.

Failure capture:

- Full `handfastd` stderr with `RUST_LOG=debug`.
- Android `adb logcat -d | grep -i kdeconnect`.
- Desktop `journalctl --user -u kdeconnectd -b` or a debug terminal.
- The packet capture, the peer device ID and the payload certificate
  fingerprint.
- The Handfast commit SHA, the peer app version and the OS versions.

Do not attach `state.db`. It contains the private key.

Exit criteria:

- Android and desktop complete file, text, empty-file and URL inbound shares.
- A large file completes with no partial file.
- A cancelled or truncated transfer leaves no file.
- An unpaired peer cannot share.
- A payload connection with a wrong certificate is rejected.
- The payload TLS version is 1.2 for Android and 1.3 for the desktop when
  offered.
- `hfctl share` completes the outbound file, empty-file and invalid-path checks
  against both clients.
- A full share channel or session queue closes the link and reports a failure
  rather than hanging the peer.
- The Windows incoming and state-file ACLs are verified or recorded as the
  remaining P1 release blocker.

## Open questions and risks

- **Windows ACLs.** The incoming directory inherits the user profile ACL. The
  incoming and state-file ACLs remain a P1 release blocker. The exact API and
  test are in `docs/ARCHITECTURE.md`.
- **Payload accept DoS.** A LAN attacker can consume the three accept attempts.
  The pin prevents impersonation, not denial of service. Is a longer retry
  window or an IP allowlist worth the compatibility risk?
- **TLS 1.3 on the payload for Android.** Android caps at 1.2. The design offers
  both and tests 1.2. A future Android client that enables 1.3 should negotiate
  it without a change.
- **8 s payload timeout.** The 8 s handshake and idle timeouts fit Android's
  10 s receiver timeout. A very slow but live link may fail earlier than a 30 s
  timeout would. Is a peer-version-aware timeout worth the extra state?
- **Payload size `-1`.** Real share files have a known size. The design accepts
  `-1` with a 2 GiB cap for compatibility. Should P1 reject `-1` for share and
  reserve it for future stream consumers?
- **Eager connection hold time.** A queued file holds a raw TCP connection while
  the previous file transfers. The queue is bounded at 8 and the first file is
  capped at 2 GiB with an 8 s idle timeout. Is that bound enough, or should the
  eager connection have its own total hold deadline?
- **Control channel lifetime.** The minimal Unix socket or named pipe is a
  subset of `handfast-ipc`. The `hfctl` binary is temporary. When does the full
  IPC replace it, and does the control command schema stay stable?
- **Windows DACL test.** The Windows named-pipe DACL needs a second local user
  to prove rejection. The Windows CI job can build the module, but the rejection
  test may stay manual until the runner supports a second user.
- **Temp directory collision.** If a user file or symlink named
  `.handfast-tmp` exists in the incoming root, startup fails. Is a fatal error
  the right behavior, or should the daemon choose another temp directory name?
- **Last-resort copy crash.** On a filesystem with neither no-replace rename nor
  hard links, the `create_new` plus copy fallback can leave a partial final on a
  crash. The sweep cannot safely delete it. Is that acceptable, or should the
  daemon refuse to receive on such filesystems?
- **Session delivery bound.** A 64-slot session queue and a 2 s delivery
  timeout close the link on overflow. A transient burst larger than 64 share
  packets would close the link. Is that the right backpressure policy, or should
  the share path have a larger dedicated session queue?

## Next implementation step

Start with unit 1: add `handfast-proto/src/share.rs` with the schema, builders,
parser and golden vectors, and make `cargo test -p handfast-proto` prove the
empty-file, `-1`, port-range and malformed-body cases.

## Sources read

Official KDE Connect sources, read at the commits below. The design re-derives
behavior; no KDE code is copied.

- Desktop `kdeconnect-kde` at `5adb4e5956128535149c000da9719b96363400a3`:
  `plugins/share/shareplugin.cpp`, `core/networkpacket.cpp`,
  `core/networkpacket.h`, `core/filetransferjob.cpp`,
  `core/backends/lan/landevicelink.cpp`, `core/backends/lan/uploadjob.cpp`,
  `core/backends/lan/compositeuploadjob.cpp`,
  `core/backends/lan/compositeuploadjob.h`,
  `core/backends/lan/lanlinkprovider.cpp`, `core/backends/lan/server.h`.
- Android `kdeconnect-android` at
  `501aee792d983a82dc6bc67f60352f8d4a639dd3`:
  `src/main/java/org/kde/kdeconnect/plugins/share/SharePlugin.kt`,
  `ShareJob.kt`, `CompositeUploadFileJob.kt`, `CompositeReceiveFileJob.kt`,
  `src/main/java/org/kde/kdeconnect/backends/lan/LanLink.kt`,
  `LanLinkProvider.kt`,
  `src/main/java/org/kde/kdeconnect/NetworkPacket.kt`,
  `src/main/java/org/kde/kdeconnect/helpers/security/SslHelper.kt`.

Local sources:

- `docs/ARCHITECTURE.md`, `docs/PARITY.md`, `docs/INTEROP.md`,
  `docs/design/p1-tls-link.md`.
- `crates/handfast-proto`, `handfast-net`, `handfast-runtime`,
  `handfast-plugins`, `handfast-desktop`, `handfast-daemon`,
  `handfast-testkit`.
- `.github/workflows/ci.yml`, `Cargo.toml`, `clippy.toml`, `notes.md`.
- The fix-round findings at
  `C:\Users\mah\AppData\Local\Temp\opencode\share-design-findings.md`, head
  `5c1a941`. Two adversarial reviews verified the findings against the real tree
  and the official sources above.
