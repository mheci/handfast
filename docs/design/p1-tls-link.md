# P1 step 2: TLS link, certificate persistence and identity re-exchange

Status: design package. No production code in this unit.

Goal: make the Handfast link layer real. A `handfastd` process must bind its TCP port, present a stable per-device certificate, authenticate a KDE Connect peer over TLS 1.3, and enforce the protocol-8 identity re-exchange before any plugin sees a packet. This unit also defines the boundary the next unit uses for pairing prompts and persistent trust.

Non-goals:

- Pairing prompts, pairing codes and the user acceptance flow. Designed at the boundary only.
- Payload and file transfer over a second connection.
- Protocol version 7 compatibility.
- Windows service installation, release packaging and version changes.
- Any discovery behavior unrelated to TLS identity security. The existing discovery follow-ups for multi-interface broadcast, socket supervision and `last_seen` write throttling stay out of scope.

## Problem

`handfastd` currently announces `tcpPort` 1716 over UDP discovery and binds no TCP listener. The daemon can discover a phone, persist its name and show it, but it cannot accept a connection, cannot prove which device is on the other end, and cannot persist the certificate material that KDE Connect peers pin.

The audit deleted two pieces that looked related but were dead. `handfast-proto::cert` put certificate bytes in a crate whose contract is transport-free. `identity_v1` in the store was initialized and had load/save methods, but no daemon path ever called them. Neither deletion was a decision that Handfast does not need identity; it was a decision that no owner existed. This design gives the material an owner and keeps the transport-free crate clean.

The shape is non-obvious for three reasons.

1. The protocol reverses the usual TLS roles. The device that dials sends a plaintext identity and then acts as the TLS **server**. The device that accepts acts as the TLS **client**. The listener does not know the expected peer ID until it reads the plaintext hello, so a per-peer verifier would require a per-connection TLS config and a second copy of the identity policy.
2. Protocol 8 sends identity twice. The first identity is plaintext and selects the pairing state. The second is encrypted and identifies the device. If the two device IDs are not correlated, an attacker can claim an unpaired device in the first packet and a paired device in the second (CVE-2025-66270). The design must make that correlation impossible to omit.
3. The daemon advertises a port before it owns one. The fix is an ordering constraint, not a new feature: open the store, load or generate the certificate, bind the listener, learn the actual port, then build the discovery identity and announce.

## Current system (Phase A grounding)

The grounding pass read `handfast-net`, `handfast-store`, `handfast-daemon`, `handfast-runtime`, `handfast-testkit`, `handfast-proto`, the architecture and parity docs, the CI workflow, `deny.toml`, and the git history of the deleted certificate module and identity table. The external protocol reference and the two 2025 KDE security advisories were read as well. Sources are listed at the end.

| Crate | Current behavior | Constraint for this design |
| --- | --- | --- |
| `handfast-proto` | Packet model, 1 MiB framing, `Identity`, `PairingRequest`, capability sets. Transport-free by doc and by dependency choice. | May gain the plaintext identity hello body. Must not gain TLS, certificate bytes or fingerprints. |
| `handfast-net` | UDP discovery on 1716, plain TCP `Link`, pure pairing state machine. | Owns the TCP listener, TLS roles, identity re-exchange and link lifecycle. Must not depend on `handfast-store`. |
| `handfast-store` | redb tables for devices, plugin state and settings. Device ID lives in `settings_v1`. No identity table. | Owns opaque DER and fingerprint persistence. No crypto, no TLS policy. |
| `handfast-runtime` | `Device`, `DeviceHost`, plugin dispatch, panic isolation, 5 s plugin timeout. No sockets. | Stays transport-free. The daemon builds one host per paired connected device. |
| `handfast-daemon` | Opens store, loads device ID, builds identity with `--tcp-port`, binds discovery, announces every 5 s, receives peers, persists sightings, waits for Ctrl+C. | Becomes the composition root. Binds the TCP listener before announcing, wires discovery to the link manager, runs per-device sessions, and supplies the trust input the manager's packet gate enforces. |
| `handfast-testkit` | Independent wire framing and a plain TCP peer. Does not depend on the production codec in its normal build. | Gains an independent real TLS peer. Must not depend on `handfast-proto` or `handfast-net` in its normal build. |

Two stale details matter. `handfast-proto`'s crate doc still claims it owns "TLS certificate material" even though the module was deleted. `handfastd --print-identity` persists a device ID but does not bind a port, so it cannot print the actual port when `--tcp-port 0` is used.

The existing store has no trust pin for a peer. `DeviceRecord.paired` is a boolean that no transport has ever been able to verify. The next unit owns the pairing prompt. This unit must not let the old boolean imply cryptographic trust.

### Protocol sequence

The KDE Connect protocol reference defines the identity fields, the device ID format and the requirement that the device ID is the TLS certificate common name. The connection sequence is documented by the protocol reference and the 2025 advisories. It is:

```
A (discovered B)                              B (listener)
  TCP connect  ------------------------------>
  plaintext identity hello {A, target B} ----->  validate targetDeviceId and targetProtocolVersion
  TLS handshake, A = server, B = client  <----->
  encrypted identity {A full} ---------------->  compare A deviceId/version to the hello
  <--------------- encrypted identity {B full}  compare B deviceId/version to discovery/hello
  framed packets, gate: unpaired -> pair only
```

The plaintext hello is not the UDP discovery packet. It is a new identity packet sent on the fresh TCP connection. It carries the connecting device's ID, its protocol version, the target device ID and the target protocol version. The listener rejects it before TLS if the target does not match.

CVE-2025-66270 is the reason the encrypted identity must be compared to the plaintext one. The advisory says protocol 8 "does not correlate device IDs across two packets" and describes an attacker who first claims an unpaired device, then a paired device. The fix is a three-way check: plaintext ID, certificate CN and encrypted ID must all be the same device.

CVE-2025-32900 is the reason the encrypted identity is authoritative for name, type and capabilities. UDP discovery is unauthenticated. A spoofed discovery packet must not rename a connected or paired device.

## Usage (caller's view)

The daemon is the only production caller. The testkit is an independent peer, not a caller. This section is the spec. The type sketch below is derived from it.

### Startup

```rust
std::fs::create_dir_all(&data_dir)?;
secure_state_dir(&data_dir)?;
let store = Arc::new(Store::open(data_dir.join("state.db"))?);
secure_state_file(&data_dir.join("state.db"))?;
let device_id = load_or_create_device_id(&store)?;

let local = match store.identity()? {
    Some(record) => LocalIdentity::from_parts(
        DeviceId::new(record.device_id)?,
        record.cert_der,
        record.key_der,
    )?,
    None => {
        let local = LocalIdentity::generate(device_id.clone())?;
        store.set_identity(&IdentityRecord {
            device_id: local.device_id().as_str().to_string(),
            cert_der: local.cert_der().to_vec(),
            key_der: local.key_der().to_vec(),
        })?;
        local
    }
};
let local = Arc::new(local);

// The listener must exist before discovery announces the port.
let listener = LinkListener::bind(
    SocketAddr::from((Ipv4Addr::UNSPECIFIED, args.tcp_port)),
    Arc::clone(&local),
    LinkLimits::default(),
).await?;
let tcp_port = listener.local_addr()?.port();
if tcp_port == 0 {
    return Err("TCP listener reported port 0".into());
}
let discovery_identity = Identity::new(
    device_id.clone(),
    DeviceName::new(&device_name()),
    DeviceType::Desktop,
    capabilities,
    Some(tcp_port),
);

let (events_tx, mut events_rx) = mpsc::channel(256);
let mut trust = TrustTable::default();
for (id, pin) in store.trust_table()? {
    trust.set(DeviceId::new(id)?, Fingerprint::from_bytes(pin));
}
let (links, link_task) = LinkManager::start(listener, local, trust, events_tx);

// The manager exists before discovery can produce a sighting.
let discovery = Discovery::bind(discovery_identity, args.discovery_port, tcp_port).await?;
```

### Discovery sighting

```rust
if let Some(port) = peer.tcp_port {
    links.consider_peer(
        peer.device_id.clone(),
        SocketAddr::new(address.ip(), port),
    );
}
```

Discovery remains responsible for `last_seen` and for inserting a placeholder record. It does not decide trust and it does not overwrite the name or type of an existing device. The authenticated identity does that after the TLS link is up.

### Daemon event loop

```rust
let mut sessions: HashMap<DeviceId, DeviceSession> = HashMap::new();
let mut pending: HashMap<DeviceId, PendingLink> = HashMap::new();
while let Some(event) = events_rx.recv().await {
    match event {
        LinkEvent::Connected { peer, certificate, link, paired } => {
            // The encrypted identity is authoritative for name and type.
            if let Err(error) = store.record_authenticated_identity(
                peer.device_id.as_str(),
                peer.device_name.as_str(),
                peer.device_type.as_str(),
            ) {
                tracing::warn!(%error, device_id = %peer.device_id, "failed to persist authenticated identity");
            }

            if paired {
                let id = peer.device_id.clone();
                pending.remove(&id);
                if let Some(previous) = sessions.remove(&id) {
                    previous.handle().stop("replaced".to_string());
                }
                let session = DeviceSession::start(
                    peer,
                    link,
                    registry.clone(),
                    desktop.clone(),
                    daemon_events.clone(),
                );
                tracing::info!(device_id = %id, fingerprint = %certificate.fingerprint().to_hex(), "paired link connected");
                sessions.insert(id, session);
            } else {
                // Keep the authenticated link so the next unit can initiate pairing too.
                pending.insert(
                    peer.device_id.clone(),
                    PendingLink { peer, certificate, link },
                );
                tracing::info!(device_id = %peer.device_id, "unpaired link authenticated; waiting for a pairing request");
            }
        }
        LinkEvent::Packet { device_id, packet } => {
            if let Some(session) = sessions.get(&device_id) {
                session.deliver(packet);
            } else if let Some(link) = pending.get_mut(&device_id) {
                if link.queued.len() < 32 {
                    link.queued.push_back(packet);
                }
            }
        }
        LinkEvent::PairingRequest { peer, request, certificate, link, paired } => {
            // Next unit: prompt. This unit logs and leaves the link open.
            tracing::info!(
                device_id = %peer.device_id,
                fingerprint = %certificate.fingerprint().to_hex(),
                paired,
                request = ?request,
                "pairing request received; prompt lands in P1 step 3"
            );
        }
        LinkEvent::Disconnected { device_id, reason } => {
            sessions.remove(&device_id);
            pending.remove(&device_id);
            tracing::info!(%device_id, %reason, "device link closed");
        }
    }
}
```

### Pairing unit plug-in

The next unit implements the prompt behind the same event. One function is the only writer of peer trust, which keeps the store and the in-memory manager from diverging.

```rust
// after the user accepts, inside the pairing unit
async fn accept_pairing(
    store: &Store,
    links: &LinkManagerHandle,
    peer: Identity,
    certificate: PeerCertificate,
    link: LinkHandle,
    registry: Registry,
    desktop: Desktop,
    daemon_events: mpsc::Sender<DaemonEvent>,
) -> Result<DeviceSession, Box<dyn Error>> {
    let pin = *certificate.fingerprint().as_bytes();
    if !store.set_trusted(peer.device_id.as_str(), Some(pin))? {
        return Err("cannot pin an unknown device".into());
    }
    links.set_trust(peer.device_id.clone(), Some(certificate.fingerprint())).await?;
    link.send(PairingRequest::accept().to_packet()?).await?;
    Ok(DeviceSession::start(peer, link, registry, desktop, daemon_events))
}
```

Rejecting writes no pin and sends `PairingRequest::reject()`. Unpairing clears the pin through the same `store.set_trusted(id, None)` and `links.set_trust(id, None)` calls, then sends `PairingRequest::reject()`. The manager's trust table and gate update, and the daemon stops the session. The `DeviceHost` is created only after acceptance. The daemon inserts the new session and drains the `PendingLink.queued` packets into it. If the manager rejects a pin because the live certificate changed, the pairing unit rolls back the stored pin and reports an error. A pin is never written for a certificate the manager will not accept.

## Shape

### Module map

`handfast-proto`:

- `identity.rs` gains `IdentityHello`.
- `lib.rs` loses the stale claim that the crate owns TLS certificate material.

`handfast-net`:

- `identity.rs`: pure identity types and checks. `Fingerprint`, `LocalIdentity`, `PeerCertificate`, `PeerTrust`, `TrustTable`, `TlsError`. No rustls, no sockets.
- `tls.rs`: rustls 1.3 configuration, the accept-any verifiers, certificate/key conversion, and the small amount of provider setup. It calls `identity.rs`; it does not own trust policy.
- `link.rs`: plaintext hello, both TLS roles, identity re-exchange, framed reader and writer, keepalive, timeouts. One public `LinkListener::accept`, one public `connect`, and `Link`.
- `manager.rs`: `LinkManager` actor, duplicate rule, trust snapshot, gate, backoff, pending limit, events, shutdown.
- `discovery.rs`, `pairing.rs`, `error.rs`: existing, with `NetError` extended.

Modules are private. The crate root re-exports the public types. This makes `handfast_net::tls::AcceptAnyClientCert` unreachable from outside; the verifier is an implementation detail. An agent that sees only the root cannot import an internal stage.

`handfast-store`:

- `identity_v1` table, singleton key `1`, JSON `IdentityRecord`.
- `DeviceRecord.certificate_fingerprint: Option<[u8; 32]>` with serde default.
- `Store::{identity, set_identity, set_trusted, trust_table, record_authenticated_identity}`.
- `record_discovery` preserves trust and only updates `last_seen` for an existing record.

`handfast-daemon`:

- `main.rs`: startup ordering, task supervision, event routing.
- `identity.rs`: device ID and certificate load or generate, Unix permissions (`secure_state_dir`, `secure_state_file`).
- `session.rs`: `DeviceSession`, `SessionHandle`, `SessionInput`, `DaemonEvent`, `PendingLink`. Owns one `DeviceHost` per paired connected peer.

`handfast-runtime` is unchanged. The daemon drives it through the existing `DeviceHost` API.

`handfast-testkit`:

- `tls_peer.rs`: an independent TLS peer with its own rcgen certificates, rustls config, framing and failure modes.
- `tls_self_test`: two testkit peers complete the identity exchange over loopback without the production codec.

### Types and signatures

```rust
// handfast-proto
pub struct IdentityHello {
    pub device_id: DeviceId,
    pub protocol_version: u8,
    pub target_device_id: DeviceId,
    pub target_protocol_version: u8,
}

impl IdentityHello {
    pub fn new(device_id: DeviceId, target_device_id: DeviceId) -> Self;
    pub fn to_packet(&self) -> Result<Packet>;
    pub fn from_packet(packet: &Packet) -> Result<Self>;
}
```

`from_packet` requires `kdeconnect.identity`, protocol version 8 for both fields, valid device IDs, and `device_id != target_device_id`. A hello for protocol 7 is not accepted. The packet body is exactly the four fields in the protocol reference.

```rust
// handfast-net::identity
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of_certificate(der: &[u8]) -> Self;
    pub fn from_bytes(bytes: [u8; 32]) -> Self;
    pub fn as_bytes(&self) -> &[u8; 32];
    pub fn to_hex(&self) -> String; // lowercase, 64 characters
}

pub struct LocalIdentity {
    device_id: DeviceId,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    fingerprint: Fingerprint,
}

impl LocalIdentity {
    pub fn generate(device_id: DeviceId) -> Result<Self, TlsError>;
    pub fn from_parts(
        device_id: DeviceId,
        cert_der: Vec<u8>,
        key_der: Vec<u8>,
    ) -> Result<Self, TlsError>;
    pub fn device_id(&self) -> &DeviceId;
    pub fn fingerprint(&self) -> Fingerprint;
    pub fn cert_der(&self) -> &[u8];
    pub fn key_der(&self) -> &[u8];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCertificate {
    device_id: DeviceId,
    fingerprint: Fingerprint,
    der: Vec<u8>,
}

impl PeerCertificate {
    pub fn parse(der: &[u8]) -> Result<Self, TlsError>;
    pub fn device_id(&self) -> &DeviceId;
    pub fn fingerprint(&self) -> Fingerprint;
    pub fn der(&self) -> &[u8];
    pub fn check(&self, expected: &DeviceId, trust: PeerTrust) -> Result<(), TlsError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTrust {
    Unpaired,
    Pinned(Fingerprint),
}

#[derive(Debug, Clone, Default)]
pub struct TrustTable(Arc<HashMap<DeviceId, Fingerprint>>);

impl TrustTable {
    pub fn trust(&self, id: &DeviceId) -> PeerTrust;
    pub fn set(&mut self, id: DeviceId, pin: Fingerprint);
    pub fn remove(&mut self, id: &DeviceId);
}
```

`LocalIdentity` has no `Debug` implementation. A derived `Debug` would print the private key. If debugging is needed, write a redacted implementation that prints the device ID and fingerprint only. The key bytes are not zeroized on drop; this is a known follow-up risk, not a feature hidden in this design.

```rust
// handfast-net::error
pub enum TlsError {
    Certificate(String),
    CertificateDeviceId { expected: DeviceId, got: DeviceId },
    PinMismatch { expected: Fingerprint, got: Fingerprint },
    IdentityDeviceId { plaintext: DeviceId, encrypted: DeviceId },
    ProtocolVersion { expected: u8, got: u8 },
}
```

These are typed because the daemon logs them as security events and the tests assert on the exact failure. A generic string would make a wrong-certificate test pass for the wrong reason. `NetError` gains `Tls(TlsError)`, `Timeout(&'static str)` and `LinkClosed` variants.

```rust
// handfast-net::link
pub struct LinkLimits {
    pub connect_timeout: Duration,   // 10 s
    pub handshake_timeout: Duration, // 10 s
    pub identity_timeout: Duration,  // 5 s
    pub write_timeout: Duration,     // 10 s
    pub max_pending: usize,          // 64
    pub max_known_peers: usize,      // 256
}

impl Default for LinkLimits { /* the values above */ }

pub struct LinkListener { /* TcpListener, Arc<LocalIdentity>, LinkLimits */ }

impl LinkListener {
    pub async fn bind(
        addr: SocketAddr,
        local: Arc<LocalIdentity>,
        limits: LinkLimits,
    ) -> Result<Self, NetError>;
    pub fn local_addr(&self) -> Result<SocketAddr, NetError>;
    pub async fn accept(&self, trust: &TrustTable) -> Result<Link, NetError>;
}

pub async fn connect(
    addr: SocketAddr,
    local: &LocalIdentity,
    expected: &DeviceId,
    trust: PeerTrust,
    limits: &LinkLimits,
) -> Result<Link, NetError>;

pub struct Link { /* peer, reader, writer */ }

impl Link {
    pub fn peer(&self) -> &LinkedPeer;
    pub fn into_parts(self) -> (LinkedPeer, LinkReader, LinkWriter);
}

pub struct LinkedPeer {
    pub identity: Identity,
    pub certificate: PeerCertificate,
    pub remote_addr: SocketAddr,
    pub initiator: DeviceId,
}

impl LinkedPeer {
    pub fn device_id(&self) -> &DeviceId {
        &self.identity.device_id
    }
}

pub struct LinkReader { /* ReadHalf<TlsStream> */ }
impl LinkReader {
    pub async fn recv(&mut self) -> Result<Option<Packet>, NetError>;
}

pub struct LinkWriter { /* WriteHalf<TlsStream> */ }
impl LinkWriter {
    pub async fn send(&mut self, packet: &Packet) -> Result<(), NetError>;
    pub async fn close(self);
}
```

`initiator` is the device ID that dialed. Both sides can compute it. The dialer sets it to the local ID; the listener sets it to the peer ID. It is the tie-breaker for duplicate connections.

```rust
// handfast-net::manager
#[derive(Clone)]
pub struct LinkHandle {
    device_id: DeviceId,
    tx: mpsc::Sender<Packet>,
}

impl LinkHandle {
    pub fn device_id(&self) -> &DeviceId;
    pub async fn send(&self, packet: Packet) -> Result<(), NetError>;
    pub fn try_send(&self, packet: Packet) -> Result<(), NetError>;
}

pub enum LinkEvent {
    Connected {
        peer: Identity,
        certificate: PeerCertificate,
        link: LinkHandle,
        paired: bool,
    },
    Packet {
        device_id: DeviceId,
        packet: Packet,
    },
    PairingRequest {
        peer: Identity,
        request: PairingRequest,
        certificate: PeerCertificate,
        link: LinkHandle,
        paired: bool,
    },
    Disconnected {
        device_id: DeviceId,
        reason: String,
    },
}

pub struct LinkManagerHandle { /* command sender, shutdown signal */ }

impl LinkManager {
    pub fn start(
        listener: LinkListener,
        local: Arc<LocalIdentity>,
        trust: TrustTable,
        events: mpsc::Sender<LinkEvent>,
    ) -> (LinkManagerHandle, tokio::task::JoinHandle<Result<(), NetError>>);
}

impl LinkManagerHandle {
    pub fn consider_peer(&self, id: DeviceId, addr: SocketAddr);
    pub async fn set_trust(
        &self,
        id: DeviceId,
        pin: Option<Fingerprint>,
    ) -> Result<(), NetError>;
    pub fn shutdown(&self);
}
```

```rust
// handfast-store
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityRecord {
    pub device_id: String,
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub paired: bool, // legacy display flag; trust is the pin
    pub last_seen: Option<i64>,
    #[serde(default)]
    pub certificate_fingerprint: Option<[u8; 32]>,
}

impl Store {
    pub fn identity(&self) -> Result<Option<IdentityRecord>>;
    pub fn set_identity(&self, record: &IdentityRecord) -> Result<()>;
    pub fn set_trusted(&self, id: &str, pin: Option<[u8; 32]>) -> Result<bool>;
    pub fn trust_table(&self) -> Result<Vec<(String, [u8; 32])>>;
    pub fn record_authenticated_identity(
        &self,
        id: &str,
        name: &str,
        device_type: &str,
    ) -> Result<()>;
}
```

`set_trusted` writes `certificate_fingerprint` and `paired = pin.is_some()` in one redb transaction. `trust_table` returns only pins. A record with `paired = true` and no fingerprint is not trusted. This is the key migration rule. `record_authenticated_identity` upserts the name and type for an authenticated device and preserves the pin and `last_seen`. `IdentityRecord` must not derive `Debug`; the key field is private material. A redacted debug implementation may print the device ID and certificate length.

```rust
// handfast-daemon::session
pub struct DeviceSession { /* task, handle */ }

impl DeviceSession {
    pub fn start(
        peer: Identity,
        link: LinkHandle,
        registry: Registry,
        desktop: Desktop,
        events: mpsc::Sender<DaemonEvent>,
    ) -> Self;
    pub fn deliver(&self, packet: Packet);
    pub fn handle(&self) -> SessionHandle;
}

/// An authenticated but unpaired link. No DeviceHost exists yet.
pub struct PendingLink {
    pub peer: Identity,
    pub certificate: PeerCertificate,
    pub link: LinkHandle,
    pub queued: VecDeque<Packet>, // bounded at 32
}

#[derive(Clone)]
pub struct SessionHandle /* bounded sender */;

impl SessionHandle {
    pub fn deliver(&self, packet: Packet);
    pub fn stop(&self, reason: String);
}

pub enum SessionInput {
    Packet(Packet),
    Disconnected(String),
}

pub enum DaemonEvent {
    PluginEvents { device_id: DeviceId, events: Vec<PluginEvent> },
}
```

The daemon `DeviceSession` is not a transport. It owns the `DeviceHost` and a bounded receiver. The manager owns the socket and the gate. The session never reads from a `TcpStream`. The session constructs `Device { paired: true, connected: true, .. }`; `connected` is true only while the session runs.

### TLS configuration and verifier policy

Both roles use TLS 1.3 only. The rustls provider is the ring provider, built per configuration with `builder_with_provider` rather than a process-global install. The configuration omits the `tls12` feature.

The dialer builds a `ServerConfig`:

- `with_client_cert_verifier(AcceptAnyClientCert)`, with `offer_client_auth = true` and `client_auth_mandatory = true`.
- `with_single_cert(local cert chain, local PKCS#8 key)`.
- `with_protocol_versions(&[&rustls::version::TLS13])`.

The listener builds a `ClientConfig`:

- `dangerous().with_custom_certificate_verifier(AcceptAnyServerCert)`.
- `with_client_auth_cert(local cert chain, local key)`.
- `with_protocol_versions(&[&rustls::version::TLS13])`.
- The server name is the peer IP as a `ServerName::IpAddress`. The custom verifier ignores the name.

Both accept-any verifiers still call the provider's `verify_tls13_signature`. They never skip proof that the peer owns the private key for its certificate. They only defer the identity decision. That decision is `PeerCertificate::check`, a pure function called once after the handshake and before any frame is read:

1. Parse exactly one certificate from `peer_certificates()`.
2. `x509_parser::parse_x509_certificate` must consume the whole DER input.
3. The subject must contain exactly one common name. Zero, multiple, an empty value or a value that is not a valid `DeviceId` is an error.
4. The CN device ID must equal the expected device ID.
5. For `PeerTrust::Pinned`, the SHA-256 of the full DER must equal the pin. For `Unpaired`, no pin is compared.

Why not check the pin inside the rustls verifier? For the listener, the expected peer ID is not known until after the plaintext hello, and the listener's `ClientConfig` is shared across connections. A per-connection config is possible but duplicates the policy. The post-handshake check has one code path for both roles and is easier to test without a socket. The tradeoff is that the TLS handshake completes for a peer that is rejected immediately afterward. No application data is exchanged before the check.

Why not validate the X.509 chain? KDE Connect uses self-signed device certificates. The security property is the pinned fingerprint and the CN-to-device-ID binding, not a public CA. Chain validation against a system trust store would reject every peer. The design deliberately ignores issuer and validity and relies on the pin, the CN and the TLS proof of possession. The certificate validity is rcgen's default long window.

### Handshake

A private `HandshakeState` enum in `link.rs` keeps the phases explicit without exposing a typestate API:

```rust
enum HandshakeState {
    Connecting,
    HelloSent,
    TlsEstablished,
    Reauthenticated,
    Active,
}
```

The public API has one `connect` and one `accept`. Callers do not coordinate stages. Each transition has its own timeout and typed error.

`connect`:

1. `TcpStream::connect` with `connect_timeout`; set `TCP_NODELAY`; enable keepalive.
2. Reject `expected == local.device_id`.
3. Send `IdentityHello` for the local ID targeting `expected`.
4. Start the TLS server handshake with the local certificate.
5. Read the peer certificate and run `PeerCertificate::check(expected, trust)`.
6. Send the local full `Identity` over TLS.
7. Read the peer `Identity` with `identity_timeout`.
8. Require the encrypted device ID to equal `expected` and the certificate CN, and the protocol version to be 8.
9. Return `Link` with the peer identity, certificate, address and initiator.

`accept`:

1. Accept TCP; set `TCP_NODELAY` and keepalive.
2. Read the plaintext `IdentityHello` with `handshake_timeout`.
3. Require `target_device_id == local.device_id`, `target_protocol_version == 8`, and `hello.device_id != local.device_id`.
4. Resolve `trust = trust_table.trust(&hello.device_id)`.
5. Start the TLS client handshake with the local certificate.
6. Read the peer certificate and run `PeerCertificate::check(&hello.device_id, trust)`.
7. Send the local full `Identity`; read the peer `Identity`.
8. Require the encrypted device ID to equal the hello device ID and the certificate CN, and the protocol version to be 8.
9. Return `Link`.

`LinkListener::bind` uses `socket2` to set `SO_REUSEADDR` on Unix before binding, so a daemon restart is not blocked by a socket in `TIME_WAIT`. It does not set `SO_REUSEPORT`, which would allow two daemons to bind the same port. On Windows it uses the plain bind.

The listener never sends a plaintext identity. The protocol reference describes one hello from the dialer. The listener's identity is the encrypted identity after the handshake. This is also why the certificate check for the listener uses the hello ID and not a discovery record.

### Link manager

`LinkManager` is a single-owner actor. It owns the listener, the trust snapshot, the peer table and the active links. There is no mutex around the socket map. It is also the single gate: the link task reads frames and forwards them to the manager, and the manager decides whether a packet is allowed. This keeps trust and packet policy in one place.

Commands and messages:

```rust
enum ManagerCommand {
    ConsiderPeer { id: DeviceId, addr: SocketAddr },
    SetTrust { id: DeviceId, pin: Option<Fingerprint>, ack: oneshot::Sender<()> },
    Shutdown,
}

enum ManagerMessage {
    HandshakeFinished { expected: Option<DeviceId>, result: Result<Link, NetError> },
    Inbound { device_id: DeviceId, generation: u64, packet: Packet },
    LinkClosed { device_id: DeviceId, generation: u64, reason: String },
}
```

State:

```rust
struct PeerState {
    addr: SocketAddr,
    failures: u32,
    next_attempt: Instant,
}

struct ActiveLink {
    generation: u64,
    initiator: DeviceId,
    peer: Identity,
    certificate: PeerCertificate,
    link: LinkHandle,
    control: watch::Sender<LinkControl>,
}

enum LinkControl {
    Close,
}
```

Rules:

- `ConsiderPeer` updates the address and dials only if the peer is not active, not pending and not in backoff. Backoff is 5 s, 10 s, 20 s, 40 s, capped at 60 s. A success resets the failure count.
- Incoming handshakes run in their own tasks, accepted up to `max_pending`. Beyond that, the connection is closed immediately.
- The peer table is capped at `max_known_peers` (256). New sightings beyond the cap are ignored with a warning. This bounds memory from a hostile discovery flood.
- A completed handshake enters the duplicate rule.
- Duplicate rule: the winner is the link whose `initiator` is `min(local_id, peer_id)`. Both sides compute the same value.
  - No active link: install the new link.
  - New and active have the same initiator: replace the active link. This is a reconnect.
  - Different initiators: keep the winner, close the loser.
- Closing a link sends `LinkControl::Close` through its watch. The link task exits cleanly. The manager ignores stale messages by generation.
- When the duplicate rule replaces an active link, the manager emits the old link's `Disconnected` before the new link's `Connected`. The daemon processes one ordered event stream, so a stale disconnect cannot remove the new session.
- When a link is installed, the manager emits `Connected` before it starts the link reader task. The daemon session therefore exists before any `Packet` event from that link.
- Each established link gets a `LinkHandle` backed by a bounded channel. The link task selects between inbound frames, outbound packets and the close signal. Inbound frames become `ManagerMessage::Inbound`; the task does not gate.
- The gate runs in the manager:
  - `kdeconnect.pair` from any authenticated link is decoded. A malformed pair body closes the link. A valid request becomes `LinkEvent::PairingRequest` with `paired` set from the trust table.
  - `kdeconnect.identity` after establishment is a protocol violation. Close.
  - Any other packet from an unpaired peer is dropped with a debug log.
  - Any other packet from a paired peer becomes `LinkEvent::Packet`.
  - The unpaired allow list is one helper, `is_unpaired_allowed(ptype)`, used by the gate and the tests. It is not repeated as a match arm in two places.
- `SetTrust { pin: Some }` is accepted only if the active link's certificate fingerprint equals the pin. A stale prompt from a replaced link cannot pin a different certificate. `SetTrust { pin: None }` clears trust on unpair. The command acknowledges after the table update. The next unit awaits the acknowledgement before sending the pairing acceptance, so the gate is already open when the peer's first paired packet arrives. A failed pin never clears trust.
- Connection and pairing events use `send().await` because they are rare and must not be lost. Packet events use `try_send`; if the daemon's bounded channel is full, the packet is dropped with a warning. Plugin packets are best effort, and dropping under overload is better than blocking the manager.
- Shutdown stops the accept loop, sends close to every active link, waits for link tasks with a 5 s timeout, then aborts stragglers.

The manager does not know `DeviceHost`. It emits `LinkEvent::Packet`; the daemon routes it to a session with a channel send.

### Daemon composition and gate

- The daemon owns one `DeviceSession` per **paired** connected device. The session owns `DeviceHost`, the device `Device` value and a bounded inbound channel.
- On `Connected`, the daemon first updates the device record from the authenticated identity (`record_authenticated_identity`), not from UDP. Then:
  - If `paired` is true, create the session, call `DeviceHost::on_connect` inside the session task, and send any output packets through the `LinkHandle`.
  - If `paired` is false, store a `PendingLink { peer, certificate, link }`. Do not create a `DeviceHost` and do not call `on_connect`. No plugin sees an unpaired peer. The pending link lets the next unit answer a peer request or start a local pairing request.
- On `Packet`, the daemon calls `session.deliver`. The session dispatches through `DeviceHost::dispatch`, sends `Dispatch.packets` through the link, and forwards `Dispatch.events` as `DaemonEvent::PluginEvents`. Existing plugin failures are already isolated by the host. If a packet arrives before the session exists, it is queued on the `PendingLink` (bounded at 32) and drained when the session is created. This closes the accept/ping race.
- On `Disconnected`, remove the session and any pending link for that device. The session calls `on_disconnect` and exits.
- The gate is in the manager, before the daemon. An unpaired ping never enters the daemon's channel. This matches the architecture requirement that unpaired packets are dropped before the plugin layer.
- The pairing prompt unit plugs in at `LinkEvent::PairingRequest`. It receives the peer, the request, the peer certificate, the `paired` state and a `LinkHandle`. It prompts, and on accept calls the one trust writer (`store.set_trusted` plus `links.set_trust`), sends the accept packet, and creates the `DeviceSession` for the now-paired link. The session starts with `Device.paired = true`.
- This unit logs pairing requests and does not answer them. That is deliberate. A reject sent before a prompt exists would teach peers a state the next unit cannot reverse without a protocol edge case.

### Startup and shutdown ordering

Run mode:

1. Parse args and initialize tracing.
2. Create the data directory. On Unix set it to 0700.
3. Open the store and set the state file to 0600 on Unix.
4. Load or create the device ID.
5. Load or generate the local TLS identity and persist it.
6. Bind `LinkListener` on `0.0.0.0:args.tcp_port`. If the port is 0, the OS assigns one.
7. Read the actual port and build the discovery `Identity`.
8. Build the trust table from the store and start the link manager.
9. Bind discovery and start the announce/receive task. The manager exists before the first sighting.
10. Run the event loop until Ctrl+C.

`--print-identity` mode:

- It prints the configured identity and exits. It does not generate a certificate, bind a port or announce.
- `--print-identity --tcp-port 0` is rejected with a clear error. There is no bound port to print.
- The existing behavior of persisting a stable device ID is preserved.

Shutdown:

- Ctrl+C sends a shutdown signal to the discovery task and stops the event loop.
- The daemon calls `links.shutdown()` and then stops every session and pending link with `SessionHandle::stop` / dropping the handle. This ensures `on_disconnect` runs even though the event loop is no longer reading `LinkEvent`s.
- Discovery stops announcing and receiving.
- The manager stops accepting, closes every link, and waits for link tasks.
- The daemon awaits the discovery task, the manager task and the session tasks with a 5 s timeout, then aborts. The listener is dropped.

### What this design deliberately does not do

- It does not put certificate bytes or rustls in `handfast-proto`.
- It does not make `handfast-net` depend on `handfast-store`.
- It does not make `handfast-runtime` own sockets or TLS.
- It does not add a second trust table. The peer pin lives on the device record.
- It does not add an application-level idle timeout. KDE Connect links can be quiet; TCP keepalive detects dead peers.
- It does not auto-unpair on a pin mismatch. An attacker must not be able to clear trust by reconnecting.
- It does not implement the pairing prompt, pairing code or persistent user trust UI.

### Interface depth

The public surface is small relative to the behavior behind it. `LinkListener::accept`, `connect` and `LinkManager::start` hide the plaintext hello, both TLS roles, certificate parsing, pin policy, identity re-exchange, framing, keepalive, timeouts, duplicate resolution and shutdown. Callers do not see a TLS type, a certificate DER type or a board state. The daemon sees domain types: a peer, an identity, a fingerprint, a packet handle.

The trust policy has one home. `PeerCertificate::check` is the only function that decides whether a certificate belongs to a peer. The gate has one home in the manager. The per-device host has one owner in the session. The store is the only writer of durable bytes and pins.

The complexity that remains exposed is deliberate. The daemon must still route events and own one session per device. That is the composition root's job. The manager does not create plugin hosts. The runtime does not know TLS.

## Data flow

1. The process opens the store and loads the stable device ID from `settings_v1`.
2. It loads `identity_v1`. If missing, it generates an ECDSA P-256 key and a self-signed certificate with CN equal to the device ID, then writes both in one transaction.
3. It binds the TCP listener. The bound address supplies the actual `tcpPort`.
4. It builds the discovery identity with that port, binds UDP discovery, and starts announcing. The announce cannot advertise a port the process has not bound.
5. Discovery receives a peer identity. It records a sighting and tells the manager the peer ID and the announced TCP port.
6. The manager either dials or accepts. A dial sends the plaintext hello, then acts as TLS server. An accept reads the hello, then acts as TLS client.
7. After the TLS handshake, each side parses the peer certificate and checks CN and pin. Then both exchange the full identity over TLS.
8. The manager checks the three-way ID correlation and the protocol version. It installs the link if it wins the duplicate rule and emits `LinkEvent::Connected` with the trust state.
9. The link task forwards inbound frames to the manager. The manager gates them. Pair frames become events. Unpaired non-pair frames are dropped. Paired frames become `LinkEvent::Packet`.
10. The daemon updates the device record from the authenticated identity. It creates a `DeviceSession` only for a paired link. The session calls `on_connect`, dispatches packets, sends plugin output and calls `on_disconnect` on close.
11. Shutdown closes discovery, the listener and every link, then drains the sessions.

## Trust model

### Local identity

- One ECDSA P-256 key pair and one self-signed X.509 certificate per device.
- The certificate common name is the device ID. The device ID is generated as a random UUIDv4 with hyphens removed, so it is 32 lowercase hex characters and satisfies the 32-38 character protocol rule. The generated certificate has exactly one subject CN; rcgen's default subject is replaced, not appended.
- The private key is stored as PKCS#8 DER in `identity_v1`. The certificate is stored as DER. Both live in the redb state file.
- On Unix the data directory is 0700 and `state.db` is 0600. On Windows the file inherits the user's profile ACLs; this is a known gap and is listed in the risks.
- A stored identity is never silently regenerated. A corrupt record, a CN mismatch, a key mismatch or a device ID mismatch with `settings_v1` is a fatal startup error. Regenerating would silently change this device's fingerprint and break every existing paired peer.
- The fingerprint is SHA-256 over the full DER certificate, lowercase hex. It is public and safe to log.

### Unpaired peer

- The TLS handshake still requires a client certificate from the dialer and presents the local certificate from the listener.
- The peer certificate must parse, contain exactly one valid device ID as CN, and match the expected peer ID. For the listener the expected ID is the plaintext hello; for the dialer it is the discovery identity that triggered the dial.
- No pin is compared.
- The gate allows only `kdeconnect.pair` after the identity exchange. Every other packet is dropped before the daemon.
- The daemon does not create a `DeviceHost` and does not call `on_connect` for an unpaired link. No plugin sees the peer.
- The pairing prompt unit can read the certificate and fingerprint from `LinkEvent::PairingRequest`, show it to the user, and pin it on acceptance.

### Paired peer

- The device record has a `certificate_fingerprint`.
- The peer certificate CN must equal the expected device ID.
- The SHA-256 of the full DER certificate must equal the stored pin.
- The plaintext hello device ID, the certificate CN and the encrypted identity device ID must all be equal.
- The protocol version must be 8 in the hello, the encrypted identity and the target fields.
- Any mismatch closes the link before a packet is emitted. The daemon logs a security event with the peer ID, the address and the reason.
- A pin mismatch does not clear the pin. A stolen device ID cannot force an unpair by reconnecting with a new certificate.
- A `pair=false` from the authenticated peer is the unpair signal. The next unit handles it.

### Migration trust rule

`DeviceRecord.paired` is a legacy display flag. The pin is the only cryptographic trust. A record with `paired = true` and no fingerprint is treated as unpaired. This can happen in a state file written by v0.2.0, where pairing was never transport-backed. The next unit's first successful pairing writes the pin and sets `paired` from it. `set_trusted` is the only writer that keeps the two fields in sync.

### Version rule

Only protocol 8 is accepted. The identity parser already rejects other versions. The plaintext hello adds an explicit target version check. There is no fallback to version 7 and no reduced-security path.

## Failure modes

| Failure | Behavior | Evidence |
| --- | --- | --- |
| TCP port in use | Exit nonzero before discovery announces. | Daemon startup test. |
| `--tcp-port 0` in run mode | Bind an ephemeral port; announce the actual port. | Daemon integration test reads the encrypted identity. |
| `--tcp-port 0` with `--print-identity` | Reject with a clear error. No port exists to print. | CLI test. |
| Discovery has no `tcpPort` | Persist the sighting; do not dial. | Manager unit test. |
| Connect refused or timed out | Log, back off per peer, retry on later discovery. | Manager test with a closed port. |
| Plaintext hello malformed | Close before TLS. | Link test. |
| Hello target ID or version mismatch | Close before TLS. | Link test. |
| Hello claims our own ID | Close. | Link test. |
| TLS handshake failure | Close; log at debug. | Link test. |
| Certificate CN missing, duplicated or invalid | Close; security event. | Identity unit test and link test. |
| Paired peer presents a different certificate | Close; keep the pin; security event. | Link test. |
| Encrypted identity ID differs from hello or CN | Close; security event. This is the CVE-2025-66270 regression. | Link test. |
| Encrypted identity version is not 8 | Close; security event. | Link test. |
| Malformed pair body | Close; it is a protocol violation. | Manager test. |
| Frame over 1 MiB, malformed JSON or deeply nested JSON | Proto or serde error; close. The 1 MiB cap bounds the allocation. | Link test. |
| Duplicate connection | One winner by initiator ID; loser closed; stale close ignored by generation. | Manager test. |
| Silent peer after TCP accept | Handshake timeout; pending permit released. | Link test with a silent peer. |
| Peer disappears | TCP keepalive detects; manager removes the link. | Manager test with a dropped stream. |
| Corrupt identity record | Fatal startup error; no regeneration. | Store and daemon tests. |
| Old pre-audit identity table | Load if valid; fail on device ID mismatch. | Store migration test. |
| v0.2.0 state with paired devices and no pin | Treated as unpaired. | Store migration test. |
| Shutdown with links open | Close listener, signal links, wait 5 s, abort leftovers. | Manager shutdown test. |
| Plugin panic or timeout | Existing `DeviceHost` isolation; link stays open. | Existing runtime tests. |

## Test plan

The test plan is written as the set of checks the implementation unit must pass. Each check names the artifact, not the internal function.

### Identity and certificate unit tests

- A generated identity has CN equal to the device ID and a fingerprint equal to SHA-256 of the DER certificate.
- The fingerprint hex form is 64 lowercase characters.
- `LocalIdentity::from_parts` rejects an empty certificate, an empty key, a malformed certificate, a CN that is not the device ID, a malformed key, and a key that does not match the certificate public key.
- `PeerCertificate::parse` rejects no CN, more than one CN, an empty CN, an invalid device ID and trailing DER after the certificate.
- `PeerCertificate::check` accepts an unpaired peer with a matching CN.
- It rejects an unpaired peer with a different CN.
- It accepts a pinned peer with a matching fingerprint.
- It rejects a pinned peer whose certificate changed.
- It never treats a missing pin as trust.

### Proto tests

- `IdentityHello` round-trips.
- It rejects a missing or malformed device ID.
- It rejects a target device ID that is not a valid device ID.
- It rejects a protocol version other than 8 in either field.
- It rejects a hello whose device ID equals the target.
- A version 7 identity is still rejected by `Identity::from_packet`.

### Link integration tests

These use the independent testkit TLS peer, not a second copy of the production code.

- A dialer and listener complete the hello, TLS handshake, certificate check and identity exchange, then exchange a ping.
- A listener presents a certificate whose CN is a different device ID. The dialer closes before identity.
- A dialer presents a certificate whose CN is a different device ID. The listener closes.
- A testkit peer sends a plaintext hello for ID A and an encrypted identity for ID B. The link closes with an identity mismatch. This is the CVE-2025-66270 regression.
- A testkit peer sends protocol version 7 in the hello, or in the encrypted identity. The link closes with a version error.
- A pinned peer is accepted when the certificate matches.
- A pinned peer is rejected when the certificate changes, and the pin remains in the trust table.
- A peer that connects and sends nothing hits the handshake timeout.
- A peer that sends a partial identity frame hits the identity timeout. After establishment there is no idle read timeout; the 1 MiB cap and keepalive bound the damage.
- A peer that declares a frame larger than 1 MiB is closed before a larger allocation.
- An unpaired peer can complete the link, but a ping is dropped and a pair request is emitted.
- An unpaired link creates no `DeviceHost` and receives no `on_connect` callback.
- Two simultaneous connections for one device ID leave exactly one active link. The winner is the link initiated by the smaller device ID.
- A second connection from the same initiator replaces the old link.

### Testkit tests

- A testkit TLS self-test runs two independent peers over loopback. They generate their own certificates, complete the hello, TLS and identity exchange, and exchange a ping.
- The testkit peer can be configured to present a wrong CN, a certificate that does not match a seeded pin, an identity mismatch, a protocol mismatch, silence and an oversized frame.
- The testkit does not import `handfast-proto` or `handfast-net` in its normal build. The conformance tests may use production types as consumers.

### Daemon integration tests

- Occupy the configured TCP port before starting `handfastd`. The daemon exits nonzero and does not announce.
- Start the daemon with `--tcp-port 0` and a free discovery port. A testkit peer announces itself. The daemon dials, completes TLS, and sends its encrypted identity. The testkit peer asserts the identity's `tcpPort` is `Some(nonzero)`. This proves the daemon announces the actual bound port, not 0.
- Start the daemon with a free fixed TCP port and a testkit peer that announces its `tcpPort`. The daemon dials, completes TLS, sends its encrypted identity and receives the peer identity. This proves bind-before-announce and the discovery-to-manager wiring.
- An unpaired testkit peer completes the link. The daemon holds a `PendingLink`, creates no `DeviceHost`, and logs the pair request. The test can assert the peer receives no plugin packet.
- Create a v0.2.0 state file with `settings_v1.device_id` and a discovered device. Start the daemon. Assert that `identity_v1` is created, the certificate CN matches the device ID, a restart reuses the same certificate and fingerprint, and the device record survives.
- Create a state file with `identity_v1` whose device ID disagrees with `settings_v1`. The daemon exits with a clear error and does not announce.
- Start the daemon with `--print-identity` twice. The device ID and `tcpPort` are stable.
- The existing `announced_devices_are_persisted` test uses a free TCP port, not 1716, because the daemon now binds the TCP port before discovery.

### Security regression tests

- Plaintext hello ID, certificate CN and encrypted identity ID all differ pairwise. Each mismatch must fail. This is the CVE-2025-66270 matrix.
- A paired peer with a changed certificate is rejected and the pin is not removed.
- A `SetTrust` with a fingerprint that does not match the active link's certificate is rejected, so a stale prompt cannot pin a replacement link.
- An unpaired peer sends a `kdeconnect.ping` and a plugin packet. Neither reaches the `DeviceHost`. A pair packet does reach the pairing event.
- A spoofed UDP identity with a different name does not overwrite the authenticated name of a connected device.
- A peer connecting to the TLS server without a client certificate is rejected by the mandatory client-auth setting.

### CI

All nine existing jobs must stay green:

1. `fmt`.
2. `clippy` with `-D warnings`.
3. `test` for the workspace and doc tests.
4. `msrv` on Rust 1.90.0 with `--locked`.
5. `deny` for advisories, licenses, bans and sources.
6. `docs` with `RUSTDOCFLAGS="-D warnings"`.
7. `testkit self-test`, extended with the TLS self-test.
8. `fuzz`, existing frame and identity targets.
9. `windows` build with the static CRT.

The Windows job is a build, not a test. The TLS certificate generation and the ring build must compile there. The Linux test job owns the runtime TLS tests.

## Dependency plan

| Crate | Version | Used by | License | MSRV | Why |
| --- | --- | --- | --- | --- | --- |
| `rustls` | 0.23.46 | `handfast-net`, `handfast-testkit` | Apache-2.0 OR ISC OR MIT | 1.71 | TLS 1.3, custom verifiers, ring provider. |
| `tokio-rustls` | 0.26.6 | `handfast-net`, `handfast-testkit` | MIT OR Apache-2.0 | 1.71 | Async TLS over Tokio streams. |
| `rcgen` | 0.14.10 | `handfast-net`, `handfast-testkit` | MIT OR Apache-2.0 | 1.88 | Self-signed ECDSA P-256 certificate generation. |
| `x509-parser` | 0.18.1 | `handfast-net` | MIT OR Apache-2.0 | 1.67 | Parse the peer certificate and extract the CN. |
| `sha2` | 0.11.1 | `handfast-net` | MIT OR Apache-2.0 | 1.88 | SHA-256 certificate fingerprint, independent of the TLS provider. |
| `rustls-pki-types` | 1.16.0 (transitive) | via rustls | MIT OR Apache-2.0 | 1.60 | Certificate and key DER types. Kept out of the public API. |
| `ring` | 0.17.14 (transitive) | via rustls and rcgen | Apache-2.0 AND ISC | 1.66 | TLS crypto provider. Not a direct dependency. |

Feature choices:

```toml
rustls = { version = "0.23.46", default-features = false, features = ["ring", "std"] }
tokio-rustls = { version = "0.26.6", default-features = false, features = ["ring"] }
rcgen = { version = "0.14.10", default-features = false, features = ["crypto", "ring"] }
x509-parser = "0.18.1"
sha2 = "0.11"
```

`default-features = false` removes `aws-lc-rs`, `tls12` and the post-quantum preference. TLS 1.2 is not used because the compatibility contract is TLS 1.3 and the configuration sets `TLS13` explicitly. `rcgen`'s default `pem` feature is also removed; the store uses DER only. `tokio-rustls`'s default `aws_lc_rs` feature is replaced by `ring`.

`handfast-net` uses rustls, tokio-rustls, rcgen, x509-parser and sha2. `handfast-testkit` uses rustls, tokio-rustls and rcgen. `handfast-daemon` adds `handfast-runtime` and `handfast-desktop` path dependencies so it can name `DeviceHost`, `Registry` and `Desktop`; it already depends on `handfast-plugins`. `handfast-net` and `handfast-daemon` add `handfast-testkit` as a dev-dependency for the integration tests. `handfast-daemon` adds no TLS dependency. `rustls-pki-types` stays transitive so no rustls type appears in the public API.

### ring versus aws-lc-rs

`rustls` 0.23 defaults to `aws-lc-rs`. That pulls `aws-lc-sys` 0.47.0, whose license expression includes `MIT-0` in an OR branch. The expression is acceptable under the current allow list because the other branches are allowed, but it adds a license family that `deny.toml` does not name and a CMake/NASM build. `ring` is `Apache-2.0 AND ISC`, both named, and needs only the C toolchain already used by the Windows runner. `rcgen` with the ring feature uses the same provider as rustls. One crypto implementation covers key generation, TLS and signatures. The tradeoff is that ring is C and assembly rather than pure Rust. The workspace's `unsafe_code = "deny"` applies to Handfast code, not to audited upstream crates. The smaller crypto surface and the simpler build favor ring. The license expression is not by itself a failure; the tooling and the one-provider argument are the decisive reasons.

### sha2 versus ring::digest

`sha2` is a direct dependency so the fingerprint function does not depend on the TLS provider. If rustls later changes its ring major or a different provider is selected, the certificate fingerprint stays stable. `sha2` 0.11.1 is pure Rust and has an allowed license. The cost is a small transitive digest tree. `ring::digest` would be one dependency fewer, but it couples a security-relevant hash to the TLS backend. The design chooses the decoupling.

### X.509 parsing

`x509-parser` is the only new parser. It is used once, on the peer certificate DER, to extract the subject CN. It has no crypto feature enabled. The alternatives are `rustls-webpki`, which does not expose the subject CN as a convenience, and hand-rolled DER parsing, which would add unsafe-adjacent complexity for no benefit. The parser consumes untrusted input, so the parser version must be kept current and a fuzz target is a sensible follow-up when the link unit lands.

### MSRV and deny

The highest new MSRV is 1.88 (`rcgen`, `sha2`, `time` transitively). The workspace MSRV is 1.90. All licenses are in `deny.toml`'s allow list. No wildcard versions, no git sources. `multiple-versions` remains a warning, not a failure. The Windows build uses the static CRT; ring supports it.

## Migration

### From a v0.2.0 state file

A v0.2.0 state file has `settings_v1` with `device_id`, `devices_v1` records and `plugin_state_v1`. It has no `identity_v1`.

On the first TLS-aware start:

1. Open the existing database without clearing tables.
2. Read `device_id` from `settings_v1`. If it is absent, generate and store it.
3. `identity_v1` is absent. Generate a self-signed certificate for the existing device ID and persist it in one write transaction. Do this before binding the listener and before discovery announces.
4. Device records deserialize with `certificate_fingerprint = None`. A record with `paired = true` and no pin is treated as unpaired. No pin is invented.
5. The link manager trusts only pins. The first successful pairing after the next unit writes one.

If the process crashes between generation and persistence, the next start generates a new certificate. No peer can have seen the old one because the listener has not been bound and discovery has not announced. This ordering is the crash-safety argument.

### From a pre-audit state file with `identity_v1`

The pre-audit table used key `1` and JSON `{ device_id, cert_der, key_der }`. The new store uses the same table name and shape.

- If the record is present and its device ID equals `settings_v1.device_id`, load it. Parse the certificate and key. Require CN equal to the device ID and the key to match the certificate.
- If the device ID differs, fail closed. Two stable identities in one state file is corruption, and choosing one silently could break a peer's pin.
- If the record is present but invalid, fail closed. Do not regenerate. A paired peer may have pinned this certificate.

### File permissions

On Unix, the data directory is created with 0700 and the redb file is set to 0600 after creation. The private key is in that file. On Windows, the file inherits the user profile ACL. A future unit can add an explicit DACL through a Windows-specific crate; this unit does not add an unsafe or platform-specific dependency for that.

### `--print-identity`

It remains a read-only diagnostic. It loads or creates the device ID, builds the identity from the configured port and prints it. It does not bind, generate a certificate or announce. With `--tcp-port 0` it returns an error because there is no actual port to print. The existing CLI test that asserts a stable identity and `tcpPort` 1716 continues to pass.

### Read compatibility

`DeviceRecord` gains one optional field with `#[serde(default)]`. Old records deserialize. New records are readable by old code because serde ignores the unknown field by default. The `identity_v1` table is invisible to old code. A downgrade is not safe: an old binary that rewrites a device record drops `certificate_fingerprint` and silently loses the pin. Treat a downgrade as a re-pair. There is no downgrade path that preserves the certificate.

## Implementation sequence

Each unit ends in a check. Do not start the next unit until the check passes.

1. **Store identity and trust records.** Add `identity_v1`, `IdentityRecord`, `Store::{identity, set_identity, set_trusted, trust_table, record_authenticated_identity}` and the optional `certificate_fingerprint` field. Tests: round-trip, reopen, v0.2.0 migration, pre-audit table load, paired-without-pin is unpaired, discovery preserves trust and does not rename, authenticated identity updates name/type and preserves trust. Check: `cargo test -p handfast-store`.
2. **Plaintext hello.** Add `IdentityHello` to `handfast-proto` with validation and tests. Remove the stale TLS claim from the crate doc. Check: `cargo test -p handfast-proto` and `cargo fuzz build` still builds.
3. **Identity types and checks.** Add `Fingerprint`, `LocalIdentity`, `PeerCertificate`, `PeerTrust`, `TlsError` and the pure checks. No sockets. Tests: generation, CN, fingerprint, pin, malformed DER. Check: `cargo test -p handfast-net identity`.
4. **TLS configuration and both roles.** Add the rustls configs, the accept-any verifiers, `LinkListener::bind/accept`, `connect`, the hello, the identity re-exchange and the framed link. Update the `handfast-net` crate doc, which still says TLS is not implemented. Test with two local identities over loopback. Check: one happy-path integration test.
5. **Negative link tests.** Add wrong CN, pin mismatch, identity mismatch, protocol mismatch, timeout and oversized frame tests. Check: `cargo test -p handfast-net --test tls_link`.
6. **Independent testkit TLS peer.** Add `tls_peer` and `tls-self-test`. Replace the loopback tests' second local identity with the testkit peer where the test needs independence. Check: `cargo test -p handfast-testkit` and `cargo run -p handfast-testkit -- tls-self-test`.
7. **Link manager.** Add dedupe, trust snapshot, gate, backoff, pending limit, events, keepalive and shutdown. Tests: duplicate winner, unpaired gate, pair event, backoff, shutdown. Check: `cargo test -p handfast-net --test manager`.
8. **Daemon composition.** Add startup ordering, port 0, discovery wiring, per-device sessions, the pairing event boundary, `record_authenticated_identity` and Unix permissions. Tests: bind failure, migration, discovery-driven dial, print-identity. Check: `cargo test -p handfast-daemon`.
9. **CI and docs.** Update the architecture state table and the README "what works" only after the code lands. Run fmt, clippy, test, msrv, deny, docs, testkit self-test, fuzz and Windows. Check: all nine checks green.

The first two units are data-only and can land before any TLS code. The third is in-memory identity code and tests; it opens no socket. The socket code starts at unit 4. This ordering means a reviewer can reject the trust model before the first `TcpListener` exists.

## Synthesis decision

This package is a synthesis of two whole-shape candidates. Model diversity was not established. The environment blocks nested subagents at depth 1, so both candidates and the judgment ran in the same parent process on the single configured model. There was no independent runner and no model-diverse cross-judge. The coordinator and the independent verifier provide the independent review that the arena could not.

### Candidates

- Candidate 1, "transport-owned TLS": no new crate. `handfast-net` owns the TLS material, both roles, the link and the manager. `handfast-store` owns opaque DER and pins. The daemon composes and owns the per-device host.
- Candidate 2, "identity and session as first-class layers": a new `handfast-identity` crate owns certificate material, fingerprinting and peer trust. `handfast-net` exposes a private typestate handshake and a per-peer session actor. `handfast-runtime` gains a `PacketTransport` trait so the runtime owns the per-device gate. The daemon is a thin wiring layer.

### Base

Candidate 1, "transport-owned TLS," is the base. It has the smaller public surface and the fewer crates. It matches the architecture table, which already names `handfast-net` as the owner of TCP links and the pairing state machine. Its manager actor gives one owner to sockets, dedupe and lifecycle. Its store change is one table and one optional field.

### Grafts from candidate 2

- **Pure identity policy split from rustls.** Candidate 2 separated certificate parsing, CN checking and pin policy from the TLS adapter. The base now has a private `identity` module and a thin `tls` module. `PeerCertificate::check` is the single policy function, and the accept-any verifiers only verify proof of possession. This preserves candidate 1's crate boundary while getting candidate 2's testability.
- **`sha2` for fingerprints.** Candidate 2 chose `sha2` over `ring::digest`. The synthesis accepts the decoupling. A certificate fingerprint should not change because the TLS provider changed.
- **One trust writer.** Candidate 2's `IdentityService` made trust a single facade. The base now states that the pairing unit is the only caller that writes both the store pin and the manager trust snapshot. This closes candidate 1's split-ownership risk between the durable pin and the in-memory table.
- **Explicit private handshake state.** Candidate 2's typestate named the phases. The synthesis keeps the phases as a private enum and a single `connect`/`accept` driver, so callers cannot coordinate stages but the implementation is readable.
- **Explicit session input enum.** Candidate 2's session actor made the packet, paired and disconnect transitions explicit. The daemon session now has `SessionInput` and `SessionHandle`.

### Rejected from candidate 2

- **New `handfast-identity` crate.** It isolates identity knowledge well, but it adds a crate and makes `handfast-store` depend on identity types. The same split is achieved by a private module in `handfast-net` without the dependency. If pairing codes, certificate rotation or trust revocation grow the policy surface, revisit the crate.
- **Public typestate.** Exposing `Connecting -> HelloSent -> TlsEstablished -> Reauthenticated -> Active` makes callers coordinate several methods. That is the shallow-module red flag. One driver is the interface.
- **Runtime `PacketTransport` trait.** It has one production implementation and moves the gate from the composition root into `handfast-runtime`. The runtime is a plugin host, not a session host. The daemon session over `LinkHandle` is the smaller boundary.
- **Gate in the runtime.** The architecture says unpaired packets are dropped before the plugin layer. The manager gate does that earlier and keeps the runtime unchanged.

### Red flag screen

| Red flag | Candidate 1 | Candidate 2 | Synthesis |
| --- | --- | --- | --- |
| Shallow module | Small surface, deep handshake and manager. | `IdentityService` and `DeviceSession` are deep, but the public typestate would be shallow. | One driver, private phases. |
| Information leakage | `PeerCertificate::der` is exposed only for the pairing SAS. No rustls types. | `StoredIdentity` and `IdentityStore` cross the store boundary. | DER accessor only; no rustls types. |
| Temporal decomposition | Modules by knowledge, not execution order. | `fingerprint/local/peer/trust/store/service` by knowledge; typestate is private. | Private phase enum only inside `link`. |
| Pass-through method | `LinkHandle` commands are policy. | `IdentityService::local` and `trust` are policy, not forwarding. | No pass-through. |
| Split ownership | Store pin and manager trust snapshot could diverge. | `IdentityService` is the facade. | Pairing unit is the only writer of both. |
| Two ways to do one task | `connect` and `accept` are roles, not alternatives. | One public `establish`; roles are internal. | One connect, one accept. |
| Importable internals | Modules private, root re-exports. | Modules private. | Modules private, root re-exports. |
| Hand-synced list | Gate uses `types` constants. | Same. | One `is_unpaired_allowed` helper in the manager. |

Both candidates converged on the important security shape: ECDSA P-256, CN = device ID, accept-any verifier plus post-handshake identity check, three-way ID correlation, pin-only trust, store raw DER, bind before announce. That convergence is strong signal. The divergence was where identity policy and the gate live. The synthesis keeps identity policy in net as a private module and the gate in the manager, because the architecture already assigns those responsibilities and the smaller crate graph wins.

## Tradeoffs accepted

- We accept a post-handshake certificate check instead of an in-handshake pin verifier, in exchange for one shared TLS server config and one pure policy function. No application data is read before the check.
- We accept one new table (`identity_v1`) and one optional device field, in exchange for not adding a separate trust table.
- We accept a manager actor with a command channel, in exchange for one owner of sockets, dedupe and lifecycle. This is more code than a mutex map but removes shared mutable state.
- We accept no application idle timeout, in exchange for not disconnecting quiet but healthy peers. TCP keepalive detects dead peers.
- We accept ring's C build over aws-lc-rs, in exchange for a smaller crypto surface, no CMake/NASM requirement, and a license expression whose named licenses are already in `deny.toml`.
- We accept `sha2` as a direct dependency, in exchange for a fingerprint that is independent of the TLS provider.
- We accept that `--print-identity --tcp-port 0` is rejected, in exchange for not binding a socket in a diagnostic command.
- We accept that `DeviceRecord.paired` remains in the schema as a legacy display flag, in exchange for not breaking the JSON records in one step. The pin is the only trust. The next unit reconciles the flag.
- We accept that a corrupt local identity is fatal, in exchange for never silently changing the device fingerprint.

## Alternatives considered

- **New `handfast-identity` crate.** It would own certificate material, the verifier and the trust policy, with `handfast-net` and `handfast-store` depending on it. It isolates policy and tests without sockets, but it adds a crate and either makes the store depend on crypto or needs feature splits. The private module in `handfast-net` gets most of the benefit. Lose on interface size and dependency weight.
- **Public typestate handshake.** It makes illegal states unrepresentable for callers, but callers must coordinate five methods to complete one operation. That is a shallow module. The private enum keeps the implementation explicit and the public API deep.
- **Pin verification inside the rustls verifiers.** It rejects earlier in the handshake, but the listener does not know the expected peer ID until after the plaintext hello, and a shared server config cannot hold per-peer pins. A per-connection config would duplicate the policy. Lose on one-policy simplicity.
- **TLS material in `handfast-proto`.** This is the deleted `cert` module. It puts transport and crypto in a crate whose contract is transport-free. Lose on layering.
- **Separate trust table.** It would keep device discovery data and trust apart, but it creates a second place that encodes pairing state. The optional field on the device record is one writer and one read path. Lose on split ownership.
- **Runtime `PacketTransport` trait.** It would let the runtime own the session and gate over any transport. It has one production implementation and expands the runtime contract. Lose on unnecessary abstraction.

## Open questions and risks

- Should `--tcp-port` be restricted to the 1714-1764 range? The compatibility contract names the range, but the CLI currently accepts any port. This design accepts any bound port and recommends a warning, not a hard failure.
- Does the listener need to send a plaintext identity of its own? The protocol reference describes one hello from the dialer, and this design follows that. Confirm against a real KDE Connect client during the P1 exit before treating it as settled.
- The current pairing state machine allows 300 s of clock skew. The protocol reference describes a 30-minute skew for the pairing timestamp. The pairing prompt unit should reconcile this before it ships. It does not affect the TLS handshake.
- Should the local certificate validity be bounded, for example ten years, instead of rcgen's 1975-4096 default? The custom verifier ignores validity, but another KDE Connect implementation might not. This needs an interop test with a real client before the P1 exit.
- Does the peer certificate need to be self-issued? This design checks CN and pin but not issuer. KDE Connect certificates are self-signed. A strict self-issued check is a protocol assertion, not a security property. Add it only if a real peer requires it.
- Should the daemon update the device name and type from the encrypted identity immediately, or wait for the pairing unit to own device records? This design updates from the authenticated identity and makes discovery preserve existing names. It needs a store method that does not race discovery; the test plan covers it.
- Is TCP keepalive enough for half-open links on Windows, or does the link need a liveness probe? `kdeconnect.ping` is a plugin packet, not a core frame. A probe would cross the plugin boundary. Keep keepalive and revisit if field reports show stuck links.
- The private key is held in `Vec<u8>` and is not zeroized on drop. A follow-up can add `zeroize` or a secret wrapper. The file permissions limit the on-disk exposure; process memory is still a risk.
- The testkit TLS peer duplicates handshake logic on purpose. It must stay independent enough to catch a production bug. This is a maintenance cost, not a one-time cost.
- `x509-parser` and its `time` dependency add a moderate transitive tree. This is the price of not hand-rolling DER parsing. Keep the parser updated and consider a cert-parse fuzz target when the link code lands.
- Ring's C build on Windows with the static CRT is not verified by this design unit. The Windows CI job builds only. The first implementation PR must confirm the build before merge.
- The runtime's `DeviceHost` has no event bus. This unit logs plugin events. The IPC/event surface is future work. The pairing prompt unit needs an event channel, which the daemon already has for `DaemonEvent`.

## Next implementation step

Add the store's `identity_v1` table, `IdentityRecord` and `set_trusted`/`trust_table` methods with the v0.2.0 and pre-audit migration tests. This is the first small unit, it has no sockets, and it settles the persistence and trust boundary before any TLS code exists.

## Sources read

Local:

- `docs/ARCHITECTURE.md`, especially the compatibility contract, workspace table, state section, security section and CI design.
- `docs/PARITY.md`, for the P1 scope and the plugin boundary.
- `crates/handfast-proto/src/{lib,identity,packet,frame,types}.rs`.
- `crates/handfast-net/src/{lib,discovery,link,pairing,error}.rs`.
- `crates/handfast-store/src/lib.rs`.
- `crates/handfast-runtime/src/{lib,device,host,plugin}.rs`.
- `crates/handfast-daemon/src/main.rs`, `crates/handfast-daemon/tests/{cli,discovery}.rs`.
- `crates/handfast-testkit/src/{lib,frame,peer,vectors}.rs`, `crates/handfast-testkit/tests/conformance.rs`.
- `Cargo.toml`, `deny.toml`, `clippy.toml`, `.github/workflows/ci.yml`.
- Git history: `db6ffed` (audit that deleted `crates/handfast-proto/src/cert.rs` and the old `identity_v1` table), `6f85e3f` (discovery wiring), and the deleted v1 `docs/PROTOCOL.md`. The old cert module was read only to understand what was deleted, not copied.

External:

- KDE Connect protocol reference, schemas: https://raw.githubusercontent.com/KDE/kdeconnect-meta/work/protocol-schemas/protocol.md
- Third-party KDE Connect protocol reference with the connection sequence: https://raw.githubusercontent.com/timschneeb/kdeconnect-nx/master/protocol.md
- KDE security advisory CVE-2025-32900: https://kde.org/info/security/advisory-20250418-2.txt
- KDE security advisory CVE-2025-66270: https://kde.org/info/security/advisory-20251128-1.txt
- GitHub advisory GHSA-xcg9-fw4f-9chv: https://github.com/advisories/GHSA-xcg9-fw4f-9chv
- Cezar Lungu, KDE Connect security disclosure (protocol background and pinning): https://cezarlungu.com/kde-connect-security-disclosure/
- crates.io and `cargo info` metadata for rustls 0.23.46, tokio-rustls 0.26.6, rcgen 0.14.10, x509-parser 0.18.1, rustls-pki-types 1.16.0, ring 0.17.14, aws-lc-rs 1.19.0, aws-lc-sys 0.47.0, sha2 0.10.9 and 0.11.1.
