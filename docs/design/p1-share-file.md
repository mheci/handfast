# P1 share and file payload transfer

Status: design package, revision 1. No production code in this unit.

Goal: make Handfast a real KDE Connect share target and sender. A paired Android
or desktop peer must be able to send a file, text or URL to `handfastd`, and
`handfastd` must be able to send a file back, with a payload transfer that the
official clients accept. The design also defines the safe-save boundary, the
independent testkit payload peer and the P1 interop exit.

Non-goals:

- Multi-file batch progress UI. The wire metadata is parsed and emitted; the UI
  is later.
- A general IPC or `hfctl` surface. The outbound path is a session API and is
  exercised by tests and the interop harness.
- P2 payload consumers such as SMS attachments. The byte mover is reusable, but
  its second caller is out of scope.
- Clipboard images, SFTP and remote file browsing.
- Windows desktop file backends. The save policy is cross-platform; the Windows
  state-file ACL gap stays open.

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

## Usage (caller's view)

This is the spec. The types below are derived from it.

### Daemon composition

```rust
let incoming_dir = args.incoming_dir.unwrap_or_else(default_incoming_dir);
let files = Arc::new(LocalFiles::new(&incoming_dir, FileLimits::default())?);
files.sweep_stale(Duration::from_secs(24 * 3600)).await?;

let desktop = Desktop {
    files: Some(Arc::clone(&files) as Arc<dyn Files>),
    ..desktop_adapters()
};
let registry = Arc::new(handfast_plugins::registry());
let capabilities = registry.capabilities(&desktop)?; // now advertises share

let payloads = Arc::new(PayloadService::new(
    Arc::clone(&tls),
    PayloadLimits::default(),
));
```

`LocalFiles::new` creates the root with 0700 on Unix. `sweep_stale` removes
`.handfast-tmp-*.part` files older than the threshold before the daemon starts
accepting links. `PayloadService` shares the daemon's long-term `TlsIdentity`; it
does not generate a second certificate.

The `--print-identity` branch keeps the existing `Desktop::default()` behavior
and does not create the incoming directory. The running daemon advertises share
after it composes the Files adapter.

### Inbound file from a phone

The user shares a photo from the Android app. The phone sends
`kdeconnect.share.request` with `filename`, `payloadSize` and
`payloadTransferInfo.port`. The session dispatches the packet to the share
plugin. The plugin returns a transfer intent. The session opens a temp file,
dials the payload port, streams the bytes, checks the size and commits the file
with an atomic no-clobber commit. The user sees `photo.jpg` appear in
`~/Downloads/Handfast`.

### Inbound text and URL

Text is copied to the desktop clipboard and emitted as an event. A URL packet is
opened with the default handler only when the scheme is `http`, `https` or
`mailto`. The event carries the value for a future UI. No payload connection is
involved.

### Outbound file to a phone

```rust
let outcome = session
    .share_file(PathBuf::from("/home/me/report.pdf"))
    .await?;
```

The session opens the source, binds a payload listener in 1739-1764, builds the
packet with the actual port, sends it on the control link, then accepts the
phone's payload connection and streams the file. The phone receives the file.

### Testkit peer

```rust
let sender = PayloadSender::new(PayloadSenderConfig {
    identity: PeerConfig::client(),
    version: TlsVersion::Tls12,
    truncate_after: None,
})?;
let port = sender.port();
// The test sends a share packet with `port` on a control link.
let report = sender.accept_and_send(&bytes).await?;
assert_eq!(report.bytes, bytes.len());
```

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
| `handfast-desktop` | Adapter traits for clipboard, notifications, media, volume, input, screensaver, shell. `DesktopRequirement::Files` is always false. | Gains a `Files` trait and `LocalFiles`: sanitization, temp file, atomic commit, permissions, disk and size limits, duplicate names, crash sweep. Must not depend on protocol. |
| `handfast-daemon` | Composition root, one session per paired device, pairing prompt, shutdown. | Owns the transfer runner, the payload peer descriptor from the authenticated link, the incoming directory, transfer events and cancellation on disconnect. |
| `handfast-testkit` | Independent frame codec, Android-shaped peer, TLS peer with TLS 1.2/1.3 and RSA. Normal build has no proto or net dependency. | Gains an independent payload sender and receiver with failure knobs. The normal build stays independent. |

The stale details this design must fix:

- `DesktopRequirement::Files` maps to `false` in `desktop_available`, so the
  share plugin cannot be advertised even if a files adapter existed.
- `LinkEvent::Connected` drops `LinkedPeer.remote_addr`. The payload receiver
  needs the control link's authenticated peer address; discovery is not a safe
  substitute.
- `docs/ARCHITECTURE.md` says the TCP range is 1714-1764. The real control range
  is 1716-1764 and the payload range is 1739-1764.
- The roster test `exactly_three_plugins_are_implemented_in_p0` will fail when
  share becomes implemented. It must become a count that includes share or a
  roster-derived assertion.

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
receiver to connect, then fails. Android waits 10 s. The desktop sender pauses
accepting between files in a batch; the receiver may connect early and the
connection stays pending. The receiver's cancellation closes the payload socket
and deletes the partial destination. Handfast uses a 30 s accept deadline, a
10 s TLS handshake deadline, a 5 s connect deadline and a 30 s idle read/write
deadline.

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
That is the constraint set used to reject candidate A.

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

pub struct IncomingFile { /* root, requested name, temp path, tokio::fs::File */ }

impl IncomingFile {
    pub fn writer(&mut self) -> &mut tokio::fs::File;
    /// Flush, sync, then commit with a no-clobber hard link. Retries a
    /// duplicate name with ` (1)`, ` (2)` and so on.
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

pub struct LocalFiles { root: PathBuf, limits: FileLimits }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileLimits {
    pub max_file_size: u64,    // 2 GiB
    pub min_free_space: u64,   // 64 MiB
    pub max_name_bytes: usize, // 255
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
    pub handshake_timeout: Duration,  // 10 s
    pub accept_timeout: Duration,     // 30 s
    pub io_timeout: Duration,         // 30 s
    pub max_attempts: usize,          // 3
    pub max_size: u64,                // 2 GiB
    pub max_extra: u64,               // 64 MiB
    pub buffer_size: usize,           // 64 KiB
    pub max_active: usize,            // 8
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

pub struct PayloadService { /* Arc<TlsIdentity>, PayloadLimits, Arc<Semaphore> */ }

impl PayloadService {
    pub fn new(tls: Arc<TlsIdentity>, limits: PayloadLimits) -> Self;

    /// Reserve a payload port and hold the source. The caller puts `port()` in
    /// the packet, sends the packet, then calls `start`.
    pub async fn prepare_send(
        &self,
        peer: PayloadPeer,
        source: PayloadSource,
    ) -> Result<PreparedSend, PayloadError>;

    /// Dial the peer's payload port and stream into `sink`.
    pub async fn receive(
        &self,
        peer: PayloadPeer,
        offer: PayloadOffer,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError>;
}

pub struct PreparedSend { /* listener, source, peer, limits */ }

impl PreparedSend {
    pub fn port(&self) -> u16;
    /// Accept one authenticated receiver and stream the source.
    pub async fn start(
        self,
        progress: Option<mpsc::Sender<u64>>,
    ) -> Result<TransferOutcome, PayloadError>;
}
```

`PayloadService` holds no per-transfer state except the global semaphore. A
transfer's state lives in its future. `prepare_send` acquires a permit before
binding and holds it in `PreparedSend`; `receive` acquires a permit before
dialing. A full pool waits up to the accept or connect deadline and then fails.
`PreparedSend` closes its listener on drop, so a failed packet send cannot leave
a port open.

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
}

enum SessionInput {
    Packet(Packet),
    ShareFile { path: PathBuf, reply: oneshot::Sender<Result<TransferOutcome, TransferError>> },
    Disconnected(String),
}
```

`TransferError` is the daemon's error type. It wraps `FilesError`, `PayloadError`,
`ProtoError` and `NetError`; it is distinct from the parser's `ShareError`.

`TransferRunner` is a private struct in `session.rs`. It owns the peer descriptor,
the desktop adapters, the payload service, the event sender, a `JoinSet` of
transfer tasks, one active receive and a bounded queue of pending receives. It
starts the next queued receive when a task finishes. The session loop selects on
the input queue and the transfer `JoinSet`, so a completed receive starts the
next queued one without waiting for another packet. On session exit the runner
aborts every task; each `IncomingFile` drop deletes its temp.

### Module map

`handfast-proto`:

- `share.rs`: `PayloadSize`, `PayloadOffer`, `ShareBatch`, `ShareFile`,
  `ShareRequest`, `ShareError`, builders, `Packet::payload_offer`.
- `lib.rs`: re-export the share types and constants.

`handfast-desktop`:

- `files.rs`: `Files`, `IncomingRequest`, `IncomingFile`, `OutgoingFile`,
  `LocalFiles`, `FileLimits`, `sanitize_filename`, `commit_noclobber`,
  `sweep_stale`, `FilesError`.
- `lib.rs`: add `files` to `Desktop` and `DesktopCapabilities`.
- `mock.rs`: a `Files` adapter backed by a `tempfile::TempDir`, plus accessors
  for the test root and saved files.
- `Cargo.toml`: add `tokio` with `fs` and `io-util`, `uuid` for temp names, and
  `tempfile` as a dev-dependency for the mock.

`handfast-net`:

- `payload.rs`: `PayloadPeer`, `PayloadLimits`, `PayloadSource`,
  `PayloadService`, `PreparedSend`, `TransferOutcome`, `PayloadError`, port
  binding, TLS server/client, peer certificate check, streaming, timeouts,
  cancellation, size caps.
- `link.rs`: extract the `SO_REUSEADDR` and bind loop into a
  `pub(crate) fn bind_tcp_in_range(min, max)` shared by the control and payload
  listeners, and make the TLS peer-certificate extraction helper reusable by the
  payload module.
- `manager.rs`: add `remote_addr` to `LinkEvent::Connected` and `PendingLink`.
- `lib.rs`: re-export the payload types.

`handfast-runtime`:

- `transfer.rs`: `Transfer`.
- `plugin.rs`: `PluginOutput.transfers` and `with_transfer`.
- `host.rs`: `Dispatch.transfers` and `absorb`.

`handfast-plugins`:

- `share.rs`: `SharePlugin`, `ShareFactory`, text/URL side effects, file intent,
  update parsing.
- `roster.rs`: `SHARE` becomes `Implemented`; the implemented count test is
  updated.
- `lib.rs`: add the factory to `registry()`.

`handfast-daemon`:

- `session.rs`: `TransferRunner`, `SessionInput::ShareFile`, transfer event
  forwarding, cancellation on exit.
- `main.rs`: `--incoming-dir`, `LocalFiles`, `Desktop.files`, `PayloadService`,
  `remote_addr` plumbing, `PendingLink.remote_addr`, shutdown.

`handfast-testkit`:

- `payload_peer.rs`: independent payload sender and receiver, TLS configs,
  failure knobs, self-test report.
- `main.rs`: `payload-self-test` subcommand.
- `frame.rs`: share packet builders that use the testkit's own `WirePacket`.
- `tests/vectors/share_url.json`, `share_file_empty.json`, `share_update.json`.

Modules stay private and the crate roots re-export the public surface. An agent
that sees only a crate root cannot import the TLS internals or the temp-file
helpers.

### Interface depth

The payload service is deep. `prepare_send` plus `PreparedSend::start` and
`receive` hide the TLS roles, port allocation, certificate checks, timeouts,
size caps, retries and cancellation. The caller supplies a peer descriptor and a
source or sink; it never sees a socket, a rustls config or a port scan. The
Files adapter is deep in the same way. One `open_incoming` call hides the
sanitizer, disk checks, temp naming, permissions and the no-clobber commit;
`commit` and `abort` are the only lifecycle methods. The share plugin is shallow
by design. It parses a packet and maps it to a side effect or a transfer intent;
it holds no policy the other modules need to learn. The session is the only
place that sees the plugin intent, the Files adapter and the payload service, and
it exposes one `share_file` method for the outbound direction.

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
link manager (paired gate) -> daemon session -> DeviceHost.dispatch
  -> share plugin parses, returns Transfer::ReceiveFile
  -> TransferRunner: files.open_incoming -> IncomingFile (temp, 0600)
  -> payloads.receive(peer, offer, incoming.writer)
       TCP connect peer.address:port
       TLS client handshake (local cert, peer pin)
       read raw bytes -> write temp, 64 KiB buffer
       EOF -> short/extra size check
  -> incoming.commit() -> fsync -> hard_link(temp, final) -> remove temp
  -> DaemonEvent::PluginEvents { ShareProgress | ShareCompleted | ShareFailed }
```

Outbound file:

```text
caller -> SessionHandle::share_file(path)
  -> TransferRunner: files.open_outgoing -> OutgoingFile
  -> payloads.prepare_send(peer, source) -> bind 1739..1764 -> PreparedSend
  -> proto::file_request(file, Some(prepared.port()))
  -> link.send(packet)
  -> prepared.start()
       accept TCP, TLS server handshake (requires client cert)
       check client cert against the control link pin
       read source, write raw bytes, close TLS
  -> reply resolves with TransferOutcome
```

Text and URL:

```text
share.request { text } -> share plugin -> Clipboard::write_text -> ShareText event
share.request { url }  -> share plugin -> scheme check -> Shell::open_url -> ShareUrl event
share.request.update   -> share plugin -> ShareBatch event
```

Empty file:

```text
share.request { filename } -> share plugin -> Transfer::ReceiveFile { size: Known(0), offer: None }
  -> TransferRunner: files.open_incoming -> commit (no payload connection)
```

Event delivery is best effort for progress. The transfer task sends at most one
`ShareProgress` event per 500 ms or 4 MiB with `try_send`; a full daemon queue
drops the progress, not the transfer. `ShareCompleted` and `ShareFailed` use a
bounded `send` and are logged if the daemon has stopped.

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
- The payload port is open for at most 30 s before the first authenticated
  connection. A failed handshake is discarded and the listener retries, up to
  three attempts. A LAN attacker can still deny the transfer by exhausting the
  attempts; this is a denial of service, not an impersonation.
- Unpaired peers never reach the share plugin. The manager gate drops every
  packet except `kdeconnect.pair` from an unpaired link, and the daemon creates
  no session. The payload service only receives a `PayloadPeer` from a paired
  session. This is tested at the manager and daemon levels.
- The payload TLS offers TLS 1.2 and TLS 1.3. TLS below 1.2 is rejected. Android
  negotiates 1.2 because its `SSLContext` caps it; the desktop can negotiate
  1.3.
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
- Logs contain the peer device ID, transfer bytes, final path and failure
  reason. They never contain file contents, clipboard text or private key
  material. The daemon logs `ShareText` and `ShareUrl` events by type and
  length, not by their text or URL body.

### Safe file save

`LocalFiles` is the single owner of the received path. It applies these rules.

**Root.** The root is `--incoming-dir`, default `$XDG_DOWNLOAD_DIR/Handfast` or
`~/Downloads/Handfast`. It is created with 0700 on Unix and checked to be a real
directory, not a symlink. An existing root keeps its permissions; the daemon does
not chmod a user-chosen directory. A symlinked root is rejected, so point
`--incoming-dir` at the real path. All received files are direct children of
this root. No user-controlled path component is ever joined to the root.

**Filename.** The raw name is treated as an untrusted basename.

1. Take the substring after the last `/` or `\`. This makes an absolute path or
   a traversal sequence a basename. `../../etc/passwd` becomes `passwd`.
2. Replace C0 and C1 control characters, bidi control characters, and the
   Windows-invalid characters `: * ? " < > |` with `_`.
3. Trim trailing dots and spaces. Reject `.` and `..`.
4. If the stem (before the first dot) is a Windows reserved name (`CON`, `PRN`,
   `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, case-insensitive), prefix `_`.
5. If the name starts with `.handfast-tmp-` and ends with `.part`, prefix `_` so
   the crash sweep cannot remove a user file.
6. Limit the UTF-8 name to 255 bytes. Truncate at a character boundary and keep
   the extension when possible.
7. If the result is empty, use `shared-file`. If a fallback is needed and the
   original had an extension, keep it.

The sanitizer is a pure function with a table test. It is not a shell command and
does not use a regex.

**Temp and commit.** `open_incoming` creates a random temp file in the root
named `.handfast-tmp-<uuid>.part` with `create_new` and mode 0600. The final name
is not created yet. On commit:

1. `File::sync_all` flushes the file, then drop the file handle. On Unix,
   `File::open(root).sync_all()` flushes the directory entry after the link.
2. Try `hard_link(temp, root/name)`. A hard link fails if the destination
   exists, so it is a no-clobber commit. It also fails on a symlink at the
   destination. The temp and the destination are in the same directory, so they
   are on the same filesystem.
3. On `AlreadyExists`, try `name (1).ext`, `name (2).ext` and so on, up to 100.
4. On `Unsupported` (a filesystem without hard links), fall back to
   `symlink_metadata(destination)` followed by `rename`. The fallback has a
   small check-then-rename race and is documented as such.
5. Remove the temp file. Return the final path.

**Overwrite.** Never overwrite. A duplicate name gets a suffix. A symlink at the
destination fails the hard link and the next suffix is tried. A directory at the
destination also fails and gets a suffix.

**Permissions.** Temp and final files are 0600 on Unix. The root is 0700. On
Windows the file inherits the user profile ACL; the state-file ACL gap is
unchanged and is called out in the open risks.

**Disk and size.** Before creating the temp, check the declared size against
`max_file_size` (2 GiB default) and check free space against
`size + min_free_space` (64 MiB reserve). An unknown size uses the 2 GiB cap.
A write error, including `ENOSPC`, aborts and deletes the temp. The payload
service also caps the bytes it reads, so a lying size cannot fill the disk. Free
space comes from a small platform helper. The recommended dependency is `fs4`
because it is safe and cross-platform; if the workspace rejects it, use
`statvfs` and `GetDiskFreeSpaceEx` behind a reviewed safe wrapper.

**Concurrency and duplicates.** `open_incoming` uses a unique temp name per
transfer. The commit loop is the only name allocator. Two transfers with the
same filename both commit; one gets the base name and the other gets a suffix.
The session runs one receive at a time per peer and queues up to 8; different
peers can run concurrently under the global payload cap.

**Crash cleanup.** `sweep_stale` scans the root at startup for
`.handfast-tmp-*.part`, removes regular files older than 24 h, and never follows
a symlink. `IncomingFile::Drop` also removes its temp on a normal abort or
unwind.

**Outbound validation.** `open_outgoing` follows the user's path, requires a
regular file, checks readability and size, and rejects directories and special
files. The caller is trusted, but a device file or a directory is still a
mistake worth failing early.

### Failure modes

| Failure | Detection | Behavior |
| --- | --- | --- |
| No payload port free | bind fails across 1739-1764 | outbound share fails with `NoPort`; no packet is sent |
| Receiver never connects | 30 s accept deadline | sender closes the listener and reports failure; receiver gets no file |
| Connect refused or timed out | 5 s connect deadline | receive fails, temp deleted, `ShareFailed` event |
| TLS handshake fails | 10 s deadline or rustls error | transfer aborted, no file, security log |
| Wrong payload certificate | pin or CN check | abort, no file, security log; listener retries up to 3 |
| Client certificate missing | TLS server requires client auth | handshake fails, no file |
| Raw TCP peer | TLS handshake fails | no file; the peer is not an official client |
| Truncated payload | bytes < declared | temp deleted, `SizeMismatch` |
| Extra payload | bytes > declared | keep up to 64 MiB extra with a warning; beyond that abort and delete |
| Unknown size | `payloadSize = -1` | read to EOF or the 2 GiB cap; cap breach aborts |
| Disk full | write error or free-space check | temp deleted, `Io` |
| Traversal or absolute path | sanitizer | basename only; never outside the root |
| Symlink at destination | hard link fails | next suffix; never follows the link |
| Duplicate name | hard link `AlreadyExists` | ` (1)`, ` (2)` suffix |
| Concurrent receive | runner queue | one active per peer, 8 queued, then reject with an event |
| Disconnect mid-transfer | session exit | abort tasks, drop `IncomingFile`, delete temp |
| Daemon crash | startup sweep | stale `.part` files older than 24 h removed |
| Empty file | no payload fields | create and commit an empty file |
| Text with no clipboard adapter | `desktop.clipboard` is `None` | log warning, still emit `ShareText` |
| URL with no shell adapter | `desktop.shell` is `None` | log warning, still emit `ShareUrl` |
| URL with unsafe scheme | scheme check | log and drop, no shell call |
| Malformed share packet | parser error | log warning, no plugin failure, no panic |
| Unpaired share packet | manager gate | dropped before the session |
| Port outside 1739-1764 | parser or service check | reject, no connect |
| Cancelled by user or session | oneshot or task abort | abort, delete temp, `ShareFailed` |
| Outbound source changed | short read | sender reports failure; receiver sees a short transfer and deletes |

### Limits

The defaults are chosen for a LAN desktop and are configurable.

| Limit | Default | Why |
| --- | --- | --- |
| Payload size | 2 GiB | Bounds a lying peer; real share files are smaller |
| Extra bytes | 64 MiB | Matches the desktop's keep-extra behavior without an unbounded write |
| Connect timeout | 5 s | LAN dial; a dead peer fails fast |
| TLS handshake | 10 s | Matches the control link |
| Accept deadline | 30 s | Matches the desktop sender |
| Idle read/write | 30 s | Detects a stalled peer without killing a slow transfer |
| Buffer | 64 KiB | Enough for LAN throughput, small enough for memory |
| Active payloads | 8 global | Bounds sockets and disk writers |
| Receive queue | 8 per peer | Handles a desktop batch without unbounded memory |
| File name | 255 bytes | Filesystem limit |
| Incoming root | 0700 | Private by default |
| Temp file | 0600 | Private until committed |

`max_size` is the absolute cap including extra bytes. A declared size near the
2 GiB cap plus extra bytes beyond the cap fails, even though the extra allowance
is 64 MiB. The service stops reading at `max_size`.

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
`handfast-desktop` owns the save. The public surface is two payload operations
(`prepare_send` and `receive`) and one intent variant. The complexity behind
that surface is TLS, ports, timeouts, certificate checks, size caps, temp files
and atomic commit.

Grafted from candidate A:

- Start the receive task immediately after dispatch. The payload connection is
  established within milliseconds of the packet, so the sender's 30 s accept
  window is never the bottleneck.
- Derive the payload peer from the control link's authenticated certificate and
  `remote_addr`, not from discovery.
- Use the exact real-client TLS roles: sender is TLS server, receiver is TLS
  client, mutual auth, pinned certificate.

Grafted from candidate C:

- One transfer registry per session with a bounded queue, a global payload
  semaphore and cancellation on disconnect. This is the single-owner property
  without a new crate.
- An explicit save state machine, Open to Receiving to Committing to Done or
  Aborted, with idempotent abort and a no-clobber commit.

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

## Tradeoffs accepted

- We accept a new `Transfer` command on `PluginOutput` in exchange for keeping
  the plugin free of sockets and file I/O. The command list is one variant for
  P1; a second variant is a deliberate future change.
- We accept a per-session transfer runner in the daemon in exchange for one
  owner of transfer lifecycle and cancellation. The session grows a module, not
  branches in its packet loop.
- We accept a hard-link commit with a rename fallback in exchange for
  cross-platform no-clobber behavior without unsafe code or a new dependency.
  The fallback race is documented and only applies to filesystems without hard
  links.
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
  Unicode name, `.handfast-tmp-x.part`.
- `open_incoming` creates a temp in the root, 0600 on Unix, and does not create
  the final name.
- `commit` creates the final file, removes the temp, and leaves no partial file.
- Duplicate commit gets ` (1)` and ` (2)`.
- Symlink at the final name is not followed and gets a suffix.
- Short write or abort deletes the temp.
- Disk-space check rejects a size larger than free space plus reserve.
- `max_file_size` rejects a larger declaration.
- `sweep_stale` removes old temp files and keeps young ones and user files.
- `open_outgoing` rejects a directory, a special file and an unreadable file.
- Unix permission test is `cfg(unix)`; the Windows job builds and runs the
  cross-platform tests.

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
- Port outside 1739-1764 is rejected before connect.
- The service dials the `PayloadPeer.address` IP and the offer port. A packet
  cannot supply an address.
- Cancellation drops the stream and leaves no bytes.
- Concurrent transfers respect the global semaphore.
- A `PreparedSend` dropped before `start` closes its port.
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
- Two files queued from one peer are received in order.
- A duplicate filename gets a suffix.
- Outbound share to a testkit receiver sends the packet, streams the bytes and
  resolves the reply.
- Outbound empty file sends a metadata-only packet.
- Outbound with no free payload port fails before sending the packet.
- Shutdown aborts active transfers and leaves no `.part` file.

### `handfast-testkit`

- Payload sender and receiver self-test over loopback.
- The self-test uses the testkit's own frame codec and TLS configs.
- Failure knobs: truncate, extra, wrong certificate, missing client certificate,
  raw TCP, accept timeout, slow read.
- The normal build has no `handfast-proto` or `handfast-net` dependency. The
  conformance test in `tests/` may use the production codec as a consumer.

### CI

The nine required jobs stay the same. The testkit self-test job adds
`cargo run -p handfast-testkit -- payload-self-test`. The fuzz job adds a
`share_request` target over `ShareRequest::from_packet`. The Windows job builds
the workspace and runs the `handfast-net` and `handfast-desktop` tests,
including the payload tests and the cross-platform save tests. No job uses
`continue-on-error`.

## Implementation sequence

Each unit ends in a check. A unit is not started until the previous check passes.

1. **Share schema in proto.** `PayloadSize`, `PayloadOffer`, `ShareBatch`,
   `ShareFile`, `ShareRequest`, `ShareError`, builders, `Packet::payload_offer`,
   golden vectors. Check: `cargo test -p handfast-proto` and
   `cargo run -p handfast-testkit -- vectors` lists the new vectors.
2. **Files adapter and safe save.** `Files`, `LocalFiles`, sanitizer, temp,
   no-clobber commit, permissions, disk and size checks, sweep, `MockDesktop`
   files. Check: `cargo test -p handfast-desktop` on Linux and the Windows job.
3. **Independent testkit payload peer.** `payload_peer.rs`, sender and receiver,
   failure knobs, self-test, `payload-self-test` command. Check:
   `cargo test -p handfast-testkit` and
   `cargo run -p handfast-testkit -- payload-self-test`.
4. **Payload service in net.** Port binding, TLS server and client, certificate
   check, streaming, timeouts, cancellation, size caps, shared bind helper.
   Check: `cargo test -p handfast-net` with the testkit peer for TLS 1.2/1.3,
   happy, truncated, wrong certificate, raw TCP, timeout, cancel and concurrent.
5. **Transfer intent in runtime.** `Transfer`, `PluginOutput.transfers`,
   `Dispatch.transfers`, share events. Check: `cargo test -p handfast-runtime`;
   the existing plugin tests are unchanged.
6. **Share plugin.** Text, URL, file intent, update, registry and roster.
   Check: `cargo test -p handfast-plugins` and the roster count test.
7. **Manager peer address.** `LinkEvent::Connected.remote_addr` and
   `PendingLink.remote_addr`; session signature. Check:
   `cargo test -p handfast-net` and `cargo check -p handfast-daemon`.
8. **Session transfer runner, inbound.** Open incoming, spawn receive, progress,
   completion, failure, queue, cancellation on disconnect, temp cleanup, and
   share event logging that redacts text and URL bodies. Check: daemon session
   tests with the testkit sender, including unpaired gate and duplicate name.
9. **Outbound share path.** `SessionHandle::share_file`, prepare, packet send,
   start, empty file, port unavailable, reply. Check: daemon test with the
   testkit receiver and a large file.
10. **Daemon composition and docs.** `--incoming-dir`, `LocalFiles`,
    `Desktop.files`, `PayloadService`, startup sweep, shutdown, INTEROP,
    ARCHITECTURE, PARITY, CI testkit command. Check: full daemon integration
    test, nine CI checks green, manual interop checklist ready.

The sequence is ordered so each layer can be tested without the one above it.
Units 1 to 4 have no daemon dependency. Unit 5 has no network dependency. Units
6 to 9 add one integration point at a time.

## Manual interop checklist update

Add a "Share and file transfer" section to `docs/INTEROP.md`.

Prerequisites:

- A paired Android phone and a paired KDE Connect desktop client.
- Firewall TCP 1739-1764 open on the Handfast host.
- A fresh `--data-dir` and a disposable `--incoming-dir`.
- `tcpdump -i any -s 0 -w share.pcap 'tcp portrange 1716-1764'`.

Android to Handfast:

1. Share a photo to Handfast. Confirm the log shows `share.request`, the
   filename, the declared size and the payload port. Confirm the file appears in
   the incoming dir with the original name and no `.part` remains.
2. Share text. Confirm the text is on the desktop clipboard and a `ShareText`
   event is logged.
3. Share an `https` URL. Confirm the browser opens it and a `ShareUrl` event is
   logged.
4. Share a `file://` URL if the app allows it. Confirm Handfast drops it with a
   log and opens nothing.
5. Share an empty file. Confirm a zero-byte file appears.
6. Share a file larger than 100 MiB. Confirm progress and a complete file.
7. Cancel a transfer from the phone. Confirm the `.part` is deleted and a
   `ShareFailed` event is logged.
8. Share two files at once. Confirm both arrive, the second gets a suffix if the
   names collide, and no file is partial.
9. Confirm the payload TLS version in the log is TLS 1.2 for Android.

Desktop to Handfast:

1. Share a file from the KDE Connect desktop client. Confirm the same behavior.
2. Share text and an `https` URL. Confirm the same behavior.
3. Confirm the payload TLS version is TLS 1.3 when the desktop offers it.
4. Send a file while a second file is queued. Confirm the order and no partial
   files.

Handfast to a real client:

- The outbound path is a session API in P1. If the interop build exposes
  `handfastd share --device <id> <path>`, run it against both clients. Otherwise
  record the outbound real-client check as blocked on the IPC unit and use the
  testkit receiver as the automated proof.

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
- The outbound check passes if the interop trigger is present; otherwise it is
  recorded as an open P1 item.

## Open questions and risks

- **Outbound trigger.** P1 has no IPC or `hfctl`. Should the interop build add a
  one-shot `handfastd share --device <id> <path>` flag so the real-client
  outbound check can run, or should that check wait for `handfast-ipc`? The
  design recommends the one-shot flag for the P1 exit and its removal when IPC
  lands. The flag is a test surface, not a product UX.
- **Incoming directory.** Is `~/Downloads/Handfast` the right default, or should
  files land directly in the user's Downloads directory like the desktop client?
  The design recommends the subdirectory for 0700 permissions and cleanup. This
  is a product call.
- **Extra bytes.** The desktop client keeps bytes beyond the declared size. The
  design keeps up to 64 MiB and fails beyond. Is that the right compatibility
  line, or should Handfast match the desktop exactly and keep everything?
- **Multi-file batches.** P1 sends one file per `share_file` call and receives a
  batch sequentially. The wire metadata supports a batch. Is per-file payload
  ports acceptable, or should the sender reuse one port for the whole batch like
  the desktop client? The receiver reads the port from each packet, so per-file
  is wire-compatible, but a client that caches the first port would break.
- **`open` flag.** The packet has an `open` boolean. Handfast parses it but has
  no file-opening adapter. Should P1 add `Shell::open_path`, or ignore the flag
  until the UI exists?
- **Text that looks like a URL.** The desktop client offers to open the last
  line of shared text when it is a URL. Handfast does not auto-open. Is that the
  right security/usability trade?
- **Windows ACLs.** The incoming directory inherits the user profile ACL. The
  state-file ACL gap remains a P1 release blocker.
- **Payload accept DoS.** A LAN attacker can consume the three accept attempts.
  The pin prevents impersonation, not denial of service. Is a longer retry
  window or an IP allowlist worth the compatibility risk?
- **TLS 1.3 on the payload for Android.** Android caps at 1.2. The design offers
  both and tests 1.2. A future Android client that enables 1.3 should negotiate
  it without a change.
- **Hard-link fallback.** A filesystem without hard links uses a check-then-
  rename fallback with a small race. Is that acceptable for P1, or should the
  design require `renameat2`/`MoveFileEx` with a narrow unsafe module?
- **Payload size `-1`.** Real share files have a known size. The design accepts
  `-1` with a 2 GiB cap for compatibility. Should P1 reject `-1` for share and
  reserve it for future stream consumers?

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
