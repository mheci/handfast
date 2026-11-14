# P1 step 2: TLS link, certificate persistence and identity re-exchange

Status: revised design package, revision 2. No production code in this unit.

Revision 2 responds to the adversarial reviews at head `30c761f`. The core shape is accepted: TLS lives in `handfast-net`, `handfast-proto` stays transport-free, there is no new identity crate and no runtime `PacketTransport` trait. The protocol sequence has been re-grounded against the real KDE Connect Android and desktop sources in `C:\Users\mah\AppData\Local\Temp\opencode\kde`. Where the third-party protocol reference and the real clients disagree, the real clients win. The changes are listed in [Revision 2 changes](#revision-2-changes).

Goal: make the Handfast link layer real. A `handfastd` process must bind a TCP port in the KDE Connect range, present a stable per-device certificate, authenticate a KDE Connect peer over TLS 1.2 or TLS 1.3, and enforce the protocol-8 identity re-exchange before any plugin sees a packet. This unit also defines the boundary the next unit uses for pairing prompts and persistent trust.

Non-goals:

- Pairing prompts, pairing codes and the user acceptance flow. Designed at the boundary only.
- Payload and file transfer over a second connection.
- Protocol version 7 compatibility.
- Windows service installation, release packaging and version changes.
- Any discovery behavior unrelated to TLS identity security. The existing discovery follow-ups for multi-interface broadcast, socket supervision and `last_seen` write throttling stay out of scope.

## Problem

`handfastd` currently announces `tcpPort` 1716 over UDP discovery and binds no TCP listener. The daemon can discover a phone, persist its name and show it, but it cannot accept a connection, cannot prove which device is on the other end, and cannot persist the certificate material that KDE Connect peers pin.

The audit deleted two pieces that looked related but were dead. `handfast-proto::cert` put certificate bytes in a crate whose contract is transport-free. `identity_v1` in the store was initialized and had load/save methods, but no daemon path ever called them. Neither deletion was a decision that Handfast does not need identity; it was a decision that no owner existed. This design gives the material an owner and keeps the transport-free crate clean.

The shape is non-obvious for four reasons.

1. The protocol reverses the usual TLS roles. The device that dials sends a plaintext identity and then acts as the TLS **server**. The device that accepts acts as the TLS **client**. The listener does not know the expected peer ID until it reads the plaintext hello, so a per-peer verifier would require a per-connection TLS config and a second copy of the identity policy.
2. Protocol 8 sends identity twice. The first identity is plaintext and selects the pairing state. The second is encrypted and identifies the device. If the two device IDs are not correlated, an attacker can claim an unpaired device in the first packet and a paired device in the second (CVE-2025-66270). The design must make that correlation impossible to omit.
3. The real hello is not the four-field packet in the third-party reference. Android sends the full identity packet plus `targetDeviceId` and a **string** `targetProtocolVersion`. Desktop sends a smaller connection packet plus a **numeric** `targetProtocolVersion`. The listener must accept both, and our dialer should send the full identity so both accept it.
4. The real clients require TLS 1.2. Android creates an `SSLContext` for `TLSv1.2`, which caps the connection at TLS 1.2. A TLS 1.3-only design fails against the official Android client. Handfast must offer TLS 1.2 and TLS 1.3, prefer 1.3, and reject anything below 1.2.

## Revision 2 changes

The findings at `30c761f` and their resolutions:

1. **TLS 1.2 and 1.3.** Add the rustls `tls12` feature, `verify_tls12_signature`, RSA signature schemes, TLS 1.2 and 1.3 success tests, and a below-1.2 rejection test. Android's `SSLContext("TLSv1.2")` is the forcing constraint.
2. **Full identity hello.** `IdentityHello` now carries the full `Identity` plus `targetDeviceId` and `targetProtocolVersion`. The parser accepts a full Android hello and the smaller desktop connection packet, and accepts `targetProtocolVersion` as a string or a number. Real Android and desktop fixtures are in the test plan. The "exactly four fields" statement is removed.
3. **Port range.** The daemon binds in 1716-1764 and scans upward within the range. It rejects 0 and out-of-range values in production. The UDP announcement test reads the actual UDP datagram and asserts the advertised port is in range. It no longer trusts the encrypted identity.
4. **Unpaired links.** The manager caps established unpaired links at 42, rate-limits inbound connections per IP (500 ms, 1024 tracked entries) and closes unpaired links idle for 60 s. The pending handshake cap stays at 64. Outbound dials reserve the unpaired slot but do not apply the inbound IP window.
5. **Duplicate links.** The manager now uses last-successful-handshake-wins, matching Android's `addOrUpdateLink` and desktop's `addLink`. A replacement with a different certificate is rejected. The test uses a testkit peer that opens a second link and closes the first.
6. **Discovery compatibility.** A new `DiscoveryAnnouncement` parser accepts the desktop UDP packet, which has only `deviceId`, `deviceName`, `protocolVersion` and `tcpPort`. The current `Identity::from_packet` rejects that packet. A desktop UDP fixture and an integration test are added, and discovery compatibility is an implementation unit before the link.
7. **Keepalive.** `LinkLimits` gains the desktop values: idle 30 s, interval 10 s, count 3, and a 60 s `TCP_USER_TIMEOUT` on platforms that support it. A socket-option test proves the configuration. The dropped-stream-as-keepalive claim is removed.
8. **Peer table.** The manager expires peers by `last_seen` (10 min) and evicts the oldest non-active peer when the 256-entry table is full. A recovery test follows a 256-ID discovery flood.
9. **Full local identity.** The link APIs now take a `LocalEndpoint`, which owns the TLS material and the full local `Identity` (name, type, capabilities, protocol version, actual `tcpPort`). The handshake has a single source for the hello and encrypted identity.
10. **Registry sharing.** `Registry` is not `Clone`. The daemon wraps it in `Arc<Registry>`, and `DeviceSession::start` takes the `Arc`. `DeviceHost::new` receives `registry.as_ref()`.
11. **Limits to the manager.** `LinkLimits` is passed explicitly to `LinkManager::start`, `accept` and `connect`. The manager owns the limits. The listener no longer hides them.
12. **Disconnected sessions.** On `LinkEvent::Disconnected`, the daemon calls `SessionHandle::stop(reason)`. `SessionInput::Disconnected` is constructed by `stop`, and the session task runs `DeviceHost::on_disconnect` before it exits.
13. **`--print-identity` proof.** The test occupies a TCP port in range, runs `--print-identity` with that port, asserts success, opens the store and asserts no `identity_v1` record was written. A certificate is not generated and the TCP port is not bound.
14. **Discovery test port.** `announced_devices_are_persisted` no longer hardcodes 1716. The daemon binds a free port in 1716-1764 and the test peer advertises a different free port in the range.
15. **TLS boundary tests.** TLS 1.2 success, TLS 1.3 success and below-1.2 rejection are explicit tests.
16. **Provider spike.** Implementation unit 0 proves rustls with the ring provider builds on Windows with the static CRT before the link code. If it fails, the fallback is aws-lc-rs after a `deny.toml` review and a CMake/NASM build check.
17. **Resolved open questions.** The listener does not send its own plaintext hello; the dialer sends one. The port range is 1716-1764. The duplicate rule is last-wins. The certificate validity window is the Android window: not-before one year ago, not-after ten years ahead. The remaining open questions are listed at the end.

## Current system (Phase A grounding)

The grounding pass read `handfast-net`, `handfast-store`, `handfast-daemon`, `handfast-runtime`, `handfast-testkit`, `handfast-proto`, the architecture and parity docs, the CI workflow, `deny.toml`, and the git history of the deleted certificate module and identity table. It also read the real KDE Connect Android and desktop sources named in [Sources read](#sources-read). Sources are listed at the end.

| Crate | Current behavior | Constraint for this design |
| --- | --- | --- |
| `handfast-proto` | Packet model, 1 MiB framing, `Identity`, `PairingRequest`, capability sets. Transport-free by doc and by dependency choice. | May gain the plaintext identity hello and the minimal discovery announcement. Must not gain TLS, certificate bytes or fingerprints. |
| `handfast-net` | UDP discovery on 1716, plain TCP `Link`, pure pairing state machine. | Owns the TCP listener, TLS roles, identity re-exchange and link lifecycle. Must not depend on `handfast-store`. |
| `handfast-store` | redb tables for devices, plugin state and settings. Device ID lives in `settings_v1`. No identity table. | Owns opaque DER and fingerprint persistence. No crypto, no TLS policy. |
| `handfast-runtime` | `Device`, `DeviceHost`, plugin dispatch, panic isolation, 5 s plugin timeout. No sockets. | Stays transport-free. The daemon builds one host per paired connected device. `DeviceHost::new` borrows `&Registry`. |
| `handfast-daemon` | Opens store, loads device ID, builds identity with `--tcp-port`, binds discovery, announces every 5 s, receives peers, persists sightings, waits for Ctrl+C. | Becomes the composition root. Binds the TCP listener before announcing, wires discovery to the link manager, runs per-device sessions, and supplies the trust input the manager's packet gate enforces. |
| `handfast-testkit` | Independent wire framing and a plain TCP peer. Does not depend on the production codec in its normal build. | Gains an independent real TLS peer with TLS 1.2, TLS 1.3 and RSA support. Must not depend on `handfast-proto` or `handfast-net` in its normal build. |

Three stale details matter. `handfast-proto`'s crate doc still claims it owns "TLS certificate material" even though the module was deleted. `handfast-net`'s crate doc says TLS is not implemented. `handfastd --print-identity` persists a device ID but does not bind a port, so it cannot print the actual port when `--tcp-port 0` is used. Revision 2 removes the `0` mode entirely.

The existing store has no trust pin for a peer. `DeviceRecord.paired` is a boolean that no transport has ever been able to verify. The next unit owns the pairing prompt. This unit must not let the old boolean imply cryptographic trust.

The current discovery parser is a compatibility bug. `Identity::from_packet` requires `deviceType` and both capability arrays. Desktop's `toUdpDiscoveryPacket` sends only `deviceId`, `deviceName`, `protocolVersion` and `tcpPort`. A real desktop announcement is rejected today. `DiscoveryAnnouncement` fixes that without weakening the encrypted identity parser.

### Protocol sequence (re-grounded against real clients)

The following is verified from the Android `LanLinkProvider.kt`, `DeviceInfo.kt` and `SslHelper.kt`, and the desktop `lanlinkprovider.cpp` and `deviceinfo.h`.

```
A (discovered B)                                      B (listener)
  TCP connect  -------------------------------------->
  plaintext identity hello {A full + target B} ------>  validate targetDeviceId and targetProtocolVersion
  TLS handshake, A = server, B = client  <----------->
  encrypted identity {A full} ----------------------->  compare A deviceId/protocolVersion to hello
  <----------------------- encrypted identity {B full}  compare B deviceId/protocolVersion to discovery/hello
  framed packets, gate: unpaired -> pair only
```

Facts that drive the design:

- **One plaintext hello, from the dialer only.** Android's `udpPacketReceived` dials, builds its full identity, adds `targetDeviceId` and `targetProtocolVersion` as strings, writes it, and then starts TLS. Desktop's `tcpSocketConnected` writes `toConnectionPacket` with numeric target fields and then starts server encryption. The listener never sends a plaintext identity of its own.
- **The dialer is the TLS server.** Android sets `clientMode = (connectionStarted == Locally)`, so a dialed connection is server mode. Desktop calls `startServerEncryption` on the dialed socket. The accepted side is the TLS client.
- **The hello shapes differ.** Android sends the full identity plus target fields. Desktop sends only `deviceId`, `deviceName`, `protocolVersion`, `targetDeviceId` and `targetProtocolVersion`. The listener must accept both. Accepting the smaller packet means `deviceType` and the capability arrays are optional in the hello. The encrypted identity remains full.
- **Target protocol version is a string on Android and a number on desktop.** `targetProtocolVersion` must parse from both a JSON string and a JSON number. The value must be 8.
- **The encrypted identity is full and authoritative.** Android sends `toIdentityPacket` after the handshake; desktop sends `toIdentityPacket` in `encrypted`. Both compare the encrypted device ID and protocol version to the values from the plaintext hello. Android also runs `isValidIdentityPacket`, which requires a non-blank `deviceName` and a valid device ID.
- **The certificate CN must equal the identity device ID.** Desktop's `addLink` compares the TLS peer certificate CN with the identity packet device ID and aborts on a mismatch. Android's trusted TLS context pins the stored certificate, and the untrusted path is trust-all; the encrypted identity is validated separately. Our post-handshake check enforces the CN correlation plus the pin for both roles.
- **TLS versions.** Android uses `SSLContext.getInstance("TLSv1.2")`, so its maximum is TLS 1.2. Desktop uses Qt's TLS stack and can negotiate TLS 1.3. Handfast must offer both. TLS below 1.2 must fail.
- **TLS client certificates.** Android sets `needClientAuth` for trusted devices and `wantClientAuth` for untrusted devices. It always has a key manager and sends its certificate. Handfast requires a client certificate for both paired and unpaired peers so it always has a certificate to show and pin. A protocol-8 peer always sends one.
- **Ports.** Android `MIN_PORT` 1716, `MAX_PORT` 1764, and `openServerSocketOnFreePort(MIN_PORT)` scans upward. Desktop `MIN_TCP_PORT`/`MAX_TCP_PORT` scans from the minimum. Both reject announcements outside the range.
- **Unpaired limits.** Both peers cap unpaired connections at 42. Android rate-limits per IP at 1 s with 255 tracked entries. Desktop rate-limits per IP at 500 ms with 1024 tracked entries.
- **Duplicates.** Both peers replace an existing link with the last successful handshake when the certificate matches. Neither uses a min-initiator tie-break.
- **Keepalive.** Desktop sets `TCP_KEEPIDLE` 30, `TCP_KEEPINTVL` 10, `TCP_KEEPCNT` 3 and `TCP_USER_TIMEOUT` 60 s. Android enables keepalive and uses a 10 s socket timeout during the handshake.
- **Discovery.** Desktop's UDP packet is minimal: `deviceId`, `deviceName`, `protocolVersion`, `tcpPort`. Android's UDP packet is the full identity plus `tcpPort`. Handfast's parser must accept both. The announcement is unauthenticated; protocol 8 exchanges the authoritative identity over TLS.

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

// Load or generate the TLS material. No full identity yet: the TCP port is not bound.
let tls = match store.identity()? {
    Some(record) => TlsIdentity::from_parts(
        DeviceId::new(record.device_id)?,
        record.cert_der.clone(),
        record.key_der.clone(),
    )?,
    None => {
        let tls = TlsIdentity::generate(device_id.clone())?;
        store.set_identity(&IdentityRecord {
            device_id: tls.device_id().as_str().to_string(),
            cert_der: tls.cert_der().to_vec(),
            key_der: tls.key_der().to_vec(),
        })?;
        tls
    }
};
let tls = Arc::new(tls);

// The registry is shared, not cloned. Desktop is Clone.
let desktop = Desktop::default();
let registry = Arc::new(handfast_plugins::registry());
let capabilities = registry.capabilities(&desktop)?;

// Bind in the KDE range before discovery announces anything.
let listener = LinkListener::bind_in_range(args.tcp_port, Arc::clone(&tls)).await?;
let tcp_port = listener.local_addr()?.port();
if !(1716..=1764).contains(&tcp_port) {
    return Err("TCP listener outside the KDE Connect range".into());
}

let identity = Identity::new(
    device_id.clone(),
    DeviceName::new(&device_name()),
    DeviceType::Desktop,
    capabilities,
    Some(tcp_port),
);
let local = Arc::new(LocalEndpoint::new(tls, identity)?);

let discovery = DiscoveryAnnouncement::new(
    device_id.clone(),
    DeviceName::new(&device_name()),
    DeviceType::Desktop,
    tcp_port,
);

let (events_tx, mut events_rx) = mpsc::channel(256);
let mut trust = TrustTable::default();
for (id, pin) in store.trust_table()? {
    trust.set(DeviceId::new(id)?, Fingerprint::from_bytes(pin));
}
let limits = LinkLimits::default();
let (links, link_task) = LinkManager::start(listener, local, trust, limits, events_tx);
let discovery = Discovery::bind(discovery, args.discovery_port).await?;
```

`TlsIdentity::generate` creates the key and certificate. `LocalEndpoint` combines that material with the full local `Identity` and is the one object the link APIs use. The manager starts before discovery can produce a sighting. Discovery binds after the TCP listener.

### Discovery sighting

```rust
links.consider_peer(
    announcement.device_id.clone(),
    SocketAddr::new(address.ip(), announcement.tcp_port),
);
```

The `DiscoveryAnnouncement` parser has already rejected a port outside 1716-1764, so the sighting path uses the field directly. The daemon persists the sighting with the announcement's name and type. The announcement has no capabilities. The encrypted identity updates name and type after the TLS link is up.

### Daemon event loop

```rust
let mut sessions: HashMap<DeviceId, DeviceSession> = HashMap::new();
let mut pending: HashMap<DeviceId, PendingLink> = HashMap::new();

while let Some(event) = events_rx.recv().await {
    match event {
        LinkEvent::Connected { peer, certificate, link, paired } => {
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
                    Arc::clone(&registry),
                    desktop.clone(),
                    daemon_events.clone(),
                );
                sessions.insert(id, session);
            } else {
                pending.insert(
                    peer.device_id.clone(),
                    PendingLink { peer, certificate, link, queued: VecDeque::new() },
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
            if let Some(session) = sessions.remove(&device_id) {
                session.handle().stop(reason.clone());
            }
            pending.remove(&device_id);
            tracing::info!(%device_id, %reason, "device link closed");
        }
    }
}
```

`Registry` is not `Clone`; the `Arc` is the shared construction. `DeviceHost::new` receives `registry.as_ref()`. `Desktop` is `Clone`.

### Pairing unit plug-in

The next unit implements the prompt behind the same event. One function is the only writer of peer trust, which keeps the store and the in-memory manager from diverging.

```rust
async fn accept_pairing(
    store: &Store,
    links: &LinkManagerHandle,
    peer: Identity,
    certificate: PeerCertificate,
    link: LinkHandle,
    registry: Arc<Registry>,
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

- `identity.rs` gains `IdentityHello` and `DiscoveryAnnouncement`.
- `lib.rs` loses the stale claim that the crate owns TLS certificate material.

`handfast-net`:

- `identity.rs`: pure identity types and checks. `Fingerprint`, `TlsIdentity`, `LocalEndpoint`, `PeerCertificate`, `PeerTrust`, `TrustTable`, `TlsError`. No sockets.
- `tls.rs`: rustls 1.2 and 1.3 configuration, the accept-any verifiers, `verify_tls12_signature` and `verify_tls13_signature`, certificate/key conversion, and provider setup. It calls `identity.rs`; it does not own trust policy.
- `link.rs`: plaintext hello, both TLS roles, identity re-exchange, framed reader and writer, keepalive, timeouts. One public `LinkListener::accept`, one public `connect`, and `Link`.
- `policy.rs`: `LinkPolicy` and `AdmissionPermit`. The unpaired cap and the inbound per-IP rate limit live here.
- `manager.rs`: `LinkManager` actor, duplicate rule, trust snapshot, gate, backoff, peer expiry, pending limit, events, shutdown.
- `discovery.rs`, `pairing.rs`, `error.rs`: existing, with `NetError` extended.

Modules are private. The crate root re-exports the public types. This makes the accept-any verifiers unreachable from outside; they are an implementation detail. An agent that sees only the root cannot import an internal stage.

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

- `tls_peer.rs`: an independent TLS peer with its own rcgen certificates, an RSA fixture, rustls config for TLS 1.2 and TLS 1.3, framing and failure modes. It can replace an existing link.
- `tls_self_test`: two testkit peers complete the identity exchange over loopback without the production codec.

### Types and signatures

```rust
// handfast-proto
pub struct IdentityHello {
    pub identity: Identity,
    pub target_device_id: DeviceId,
    pub target_protocol_version: u8,
}

impl IdentityHello {
    pub fn new(identity: Identity, target_device_id: DeviceId) -> Self;
    pub fn to_packet(&self) -> Result<Packet>;
    pub fn from_packet(packet: &Packet) -> Result<Self>;
}

pub struct DiscoveryAnnouncement {
    pub device_id: DeviceId,
    pub device_name: DeviceName,
    pub device_type: DeviceType,
    pub protocol_version: u8,
    pub tcp_port: u16,
}

impl DiscoveryAnnouncement {
    pub fn new(
        device_id: DeviceId,
        device_name: DeviceName,
        device_type: DeviceType,
        tcp_port: u16,
    ) -> Self;
    pub fn to_packet(&self) -> Result<Packet>;
    pub fn from_packet(packet: &Packet) -> Result<Self>;
}
```

`IdentityHello::to_packet` starts from the full `Identity` body and inserts `targetDeviceId` and a numeric `targetProtocolVersion`. `IdentityHello::from_packet` requires `kdeconnect.identity`, a valid `deviceId`, protocol version 8, and a valid target device ID. `deviceName` is optional and sanitized; `deviceType` and both capability arrays are optional and default to `Unknown` and empty. `targetProtocolVersion` accepts a JSON number or a string and must be 8. `device_id` must differ from `target_device_id`. This accepts the Android full hello and the desktop connection packet.

`DiscoveryAnnouncement::from_packet` requires `kdeconnect.identity`, a valid `deviceId`, `protocolVersion` 8 and a `tcpPort` in 1716-1764. It reads `deviceName` and `deviceType` if present and defaults to `Unnamed device` and `Unknown`. It ignores capability fields. `to_packet` emits the minimal desktop shape: `deviceId`, `deviceName`, `protocolVersion` and `tcpPort`. The encrypted identity parser stays strict; only discovery and the plaintext hello are permissive.

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

pub struct TlsIdentity {
    device_id: DeviceId,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    fingerprint: Fingerprint,
}

impl TlsIdentity {
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

pub struct LocalEndpoint {
    tls: Arc<TlsIdentity>,
    identity: Identity,
}

impl LocalEndpoint {
    /// # Errors
    /// Fails when `identity.device_id` does not match the TLS certificate device ID.
    pub fn new(tls: Arc<TlsIdentity>, identity: Identity) -> Result<Self, TlsError>;
    pub fn tls(&self) -> &Arc<TlsIdentity>;
    pub fn identity(&self) -> &Identity;
    pub fn device_id(&self) -> &DeviceId;
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

`LocalEndpoint` is the fix for the missing local identity. `TlsIdentity` has no `Debug` implementation; a derived `Debug` would print the private key.

`TlsIdentity::generate` uses ECDSA P-256 and a certificate validity window of now minus one year to now plus ten years, matching Android's `initialiseCertificate`. The certificate has exactly one subject CN equal to the device ID; rcgen's default subject is replaced, not appended. The private key is PKCS#8 DER.

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

These are typed because the daemon logs them as security events and the tests assert on the exact failure. A generic string would make a wrong-certificate test pass for the wrong reason. `NetError` gains `Tls(TlsError)`, `Timeout(&'static str)`, `LinkClosed` and `Admission`.

```rust
// handfast-net::link
pub struct LinkLimits {
    pub connect_timeout: Duration,        // 10 s
    pub handshake_timeout: Duration,      // 10 s
    pub identity_timeout: Duration,       // 5 s
    pub write_timeout: Duration,          // 10 s
    pub max_pending: usize,               // 64
    pub max_known_peers: usize,           // 256
    pub peer_ttl: Duration,               // 10 min
    pub max_unpaired_links: usize,        // 42
    pub unpaired_idle_timeout: Duration,  // 60 s
    pub per_ip_interval: Duration,        // 500 ms, inbound only
    pub rate_limit_entries: usize,        // 1024
    pub keepalive_idle: Duration,         // 30 s
    pub keepalive_interval: Duration,     // 10 s
    pub keepalive_retries: u32,           // 3
    pub tcp_user_timeout: Duration,       // 60 s
}

impl Default for LinkLimits { /* the values above */ }

pub struct LinkListener { /* TcpListener, Arc<TlsIdentity> */ }

impl LinkListener {
    /// Bind the first free port from `start` through 1764.
    ///
    /// # Errors
    /// Fails when `start` is outside 1716-1764 or every port in the range is taken.
    pub async fn bind_in_range(start: u16, tls: Arc<TlsIdentity>) -> Result<Self, NetError>;
    pub fn local_addr(&self) -> Result<SocketAddr, NetError>;
    pub async fn accept(
        &self,
        local: &LocalEndpoint,
        trust: &TrustTable,
        limits: &LinkLimits,
        policy: &LinkPolicy,
    ) -> Result<Link, NetError>;
}

pub async fn connect(
    addr: SocketAddr,
    local: &LocalEndpoint,
    expected: &DeviceId,
    trust: PeerTrust,
    limits: &LinkLimits,
    permit: AdmissionPermit,
) -> Result<Link, NetError>;

pub struct Link { /* peer, reader, writer, admission permit */ }

impl Link {
    pub fn peer(&self) -> &LinkedPeer;
    pub fn into_parts(self) -> (LinkedPeer, LinkReader, LinkWriter, AdmissionPermit);
}

pub struct LinkedPeer {
    pub identity: Identity,
    pub certificate: PeerCertificate,
    pub remote_addr: SocketAddr,
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

The `initiator` field and the min-initiator tie-break are gone. The real peers use last-successful handshake, not a directional winner.

```rust
// handfast-net::policy
#[derive(Clone)]
pub struct LinkPolicy { /* Arc<Mutex<PolicyState>>, limits */ }

impl LinkPolicy {
    /// Reserve a slot for an incoming connection. A paired peer does not consume
    /// an unpaired slot but still passes the inbound per-IP rate limit. An
    /// unpaired peer is rejected when 42 unpaired slots are reserved or active,
    /// or the IP is inside the 500 ms inbound window.
    pub fn reserve_incoming(
        &self,
        address: IpAddr,
        peer: &DeviceId,
        trust: PeerTrust,
    ) -> Result<AdmissionPermit, NetError>;

    /// Reserve a slot for an outgoing dial. This reserves the unpaired slot but
    /// does not apply the inbound per-IP window; the reciprocal peer must still
    /// be able to accept the connection. Per-peer backoff handles retries.
    pub fn reserve_outgoing(
        &self,
        peer: &DeviceId,
        trust: PeerTrust,
    ) -> Result<AdmissionPermit, NetError>;
}

pub struct AdmissionPermit { /* Drop releases the slot */ }

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
        local: Arc<LocalEndpoint>,
        trust: TrustTable,
        limits: LinkLimits,
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

`LinkLimits` is passed to the manager, `accept` and `connect`. The listener does not hide it. `LinkPolicy` is the admission owner for unpaired slots and inbound per-IP rate limits.

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

`set_trusted` writes `certificate_fingerprint` and `paired = pin.is_some()` in one redb transaction. `trust_table` returns only pins. A record with `paired = true` and no fingerprint is not trusted. `record_authenticated_identity` upserts the name and type for an authenticated device and preserves the pin and `last_seen`. `IdentityRecord` must not derive `Debug`; the key field is private material.

```rust
// handfast-daemon::session
pub struct DeviceSession { /* task, handle */ }

impl DeviceSession {
    pub fn start(
        peer: Identity,
        link: LinkHandle,
        registry: Arc<Registry>,
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

The session task calls `DeviceHost::new(device, registry.as_ref(), desktop.clone())` and constructs `Device { paired: true, connected: true, .. }`. `SessionHandle::stop` sends `SessionInput::Disconnected(reason)`. The session task handles that input by calling `DeviceHost::on_disconnect` and sending any final output best effort before it exits. A `LinkEvent::Disconnected` removes the session from the map and calls `stop`, so `on_disconnect` runs.

### TLS configuration and verifier policy

Both roles use TLS 1.2 and TLS 1.3. TLS 1.3 is preferred by rustls ordering; TLS 1.2 is required for Android. The rustls provider is the ring provider, built per configuration with `builder_with_provider` rather than a process-global install.

The dialer builds a `ServerConfig`:

- `with_client_cert_verifier(AcceptAnyClientCert)`, with `offer_client_auth = true` and `client_auth_mandatory = true`.
- `with_single_cert(local cert chain, local PKCS#8 key)`.
- `with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])`.

The listener builds a `ClientConfig`:

- `dangerous().with_custom_certificate_verifier(AcceptAnyServerCert)`.
- `with_client_auth_cert(local cert chain, local key)`.
- `with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])`.
- The server name is the peer IP as a `ServerName::IpAddress`. The custom verifier ignores the name.

Both accept-any verifiers implement all four signature methods:

- `verify_tls13_signature` delegates to `rustls::crypto::verify_tls13_signature`.
- `verify_tls12_signature` delegates to `rustls::crypto::verify_tls12_signature`.
- `supported_verify_schemes` returns the ring provider's schemes, including RSA-PSS, RSA PKCS#1 and ECDSA.
- The accept-any paths never skip proof that the peer owns the private key. They only defer the identity decision.

The provider's TLS 1.2 cipher suites include ECDHE with RSA and ECDSA certificates. Android's certificate may be RSA; the testkit includes a static RSA certificate and PKCS#8 key fixture generated for tests, not copied from KDE. The verifier and the handshake must work with it in TLS 1.2 and TLS 1.3.

Identity policy is `PeerCertificate::check`, a pure function called once after the handshake and before any frame is read:

1. Parse exactly one certificate from `peer_certificates()`.
2. `x509_parser::parse_x509_certificate` must consume the whole DER input.
3. The subject must contain exactly one common name. Zero, multiple, an empty value or a value that is not a valid `DeviceId` is an error.
4. The CN device ID must equal the expected device ID.
5. For `PeerTrust::Pinned`, the SHA-256 of the full DER must equal the pin. For `Unpaired`, no pin is compared.

Why not check the pin inside the rustls verifier? For the listener, the expected peer ID is not known until after the plaintext hello, and the listener's `ClientConfig` is shared across connections. A per-connection config is possible but duplicates the policy. The post-handshake check has one code path for both roles and is easier to test without a socket. No application data is exchanged before the check.

Why not validate the X.509 chain? KDE Connect uses self-signed device certificates. The security property is the pinned fingerprint and the CN-to-device-ID binding, not a public CA. Chain validation against a system trust store would reject every peer. The design deliberately ignores issuer and validity and relies on the pin, the CN and the TLS proof of possession. The local certificate validity is one year back to ten years ahead, matching Android, so peers that do check validity still accept it.

### Keepalive

Every accepted and dialed TCP stream gets the real desktop keepalive values from `LinkLimits`:

- `TCP_KEEPIDLE` or the platform equivalent: 30 s.
- `TCP_KEEPINTVL`: 10 s.
- `TCP_KEEPCNT`: 3.
- `TCP_USER_TIMEOUT`: 60 s on Unix platforms that expose it.
- `TCP_NODELAY`: on.

The socket helper uses `socket2::SockRef` and `socket2::TcpKeepalive`. On Windows, `TCP_USER_TIMEOUT` is not exposed by `socket2`; Windows uses the keepalive values only. A test creates a connected socket pair, applies the helper, and reads the options back with `socket2` getters. The Unix test asserts all four values; the Windows test asserts the keepalive flag and the values the platform exposes. The old "dropped stream" test is not a keepalive test and does not replace this one.

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

1. `TcpStream::connect` with `connect_timeout`; set `TCP_NODELAY`; apply keepalive.
2. Reject `expected == local.device_id()`.
3. Send `IdentityHello` built from `local.identity()` with the target `expected`.
4. Start the TLS server handshake with the local certificate. TLS 1.2 and 1.3 are enabled.
5. Read the peer certificate and run `PeerCertificate::check(expected, trust)`.
6. Send the local full `Identity` from `local.identity()` over TLS.
7. Read the peer `Identity` with `identity_timeout`.
8. Require the encrypted device ID to equal `expected` and the certificate CN, and the protocol version to be 8.
9. Return `Link` with the peer identity and certificate.

`accept`:

1. Accept TCP; set `TCP_NODELAY` and apply keepalive.
2. Read the plaintext `IdentityHello` with `handshake_timeout`.
3. Require `target_device_id == local.device_id()`, `target_protocol_version == 8`, and `hello.identity.device_id != local.device_id()`.
4. Resolve `trust = trust_table.trust(&hello.identity.device_id)`.
5. Ask `policy.reserve_incoming(address, &hello.identity.device_id, trust)` for an admission permit. A rejection closes before TLS.
6. Start the TLS client handshake with the local certificate. TLS 1.2 and 1.3 are enabled.
7. Read the peer certificate and run `PeerCertificate::check(&hello.identity.device_id, trust)`.
8. Send the local full `Identity` from `local.identity()`; read the peer `Identity`.
9. Require the encrypted device ID to equal the hello device ID and the certificate CN, and the protocol version to be 8.
10. Return `Link` holding the admission permit.

The listener never sends a plaintext identity. Both real clients send exactly one hello from the dialer, and the listener answers with the encrypted identity after the handshake.

`LinkListener::bind_in_range` scans from `start` through 1764 with `SO_REUSEADDR` on Unix, so a daemon restart is not blocked by `TIME_WAIT`. It does not set `SO_REUSEPORT`. A `start` outside 1716-1764 is rejected. The daemon's default is 1716. If all ports in the range are taken, startup fails before discovery announces.

### Link manager

`LinkManager` is a single-owner actor. It owns the listener, the trust snapshot, the admission policy, the peer table and the active links. There is no mutex around the socket map. It is also the single gate: the link task reads frames and forwards them to the manager, and the manager decides whether a packet is allowed.

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
    last_seen: Instant,
    failures: u32,
    next_attempt: Instant,
}

struct ActiveLink {
    generation: u64,
    peer: Identity,
    certificate: PeerCertificate,
    link: LinkHandle,
    control: watch::Sender<LinkControl>,
    paired: bool,
    last_activity: Instant,
    permit: Option<AdmissionPermit>,
}

enum LinkControl {
    Close,
}
```

Rules:

- `ConsiderPeer` updates `last_seen` and the address. It dials only if the peer is not active, not pending and not in backoff. Before an outgoing dial it calls `LinkPolicy::reserve_outgoing`; the permit is passed to `connect` and held by the link. This reserves the unpaired slot but does not apply the inbound per-IP window, so a reciprocal accept inside 500 ms is not rejected. Per-peer backoff handles retries. A rejection skips the dial. Backoff is 5 s, 10 s, 20 s, 40 s, capped at 60 s. A success resets the failure count.
- Incoming handshakes run in their own tasks, accepted up to `max_pending` (64). Beyond that, the connection is closed immediately.
- `LinkPolicy::reserve_incoming` enforces the inbound per-IP interval (500 ms, 1024 tracked entries) and the unpaired cap (42). `LinkPolicy::reserve_outgoing` enforces only the unpaired cap; it must not mark the peer IP, or simultaneous discovery would cause each side's reciprocal accept to be rejected. Both reserve the unpaired slot before TLS; the `AdmissionPermit` is held by the `Link` and then by `ActiveLink`, and released on close. A paired peer gets a permit without consuming an unpaired slot.
- The peer table is capped at 256. On each tick, peers with `last_seen + peer_ttl < now` and no active link are evicted. When the table is full and a new peer arrives, the oldest non-active peer is evicted. If every entry is active, the new sighting is ignored with a warning. A 256-ID discovery flood therefore cannot lock out a later real peer.
- A completed handshake enters the duplicate rule. Last-successful-handshake wins, matching both real peers:
  - If no active link exists, install the new link.
  - If an active link exists and the new link's certificate DER equals the active certificate, close the old link, emit the old `Disconnected`, install the new link, and emit `Connected`.
  - If the certificate differs, close the new link. A different certificate is a different identity, even with the same device ID.
- Closing a link sends `LinkControl::Close` through its watch. The link task exits cleanly. The manager ignores stale messages by generation.
- When the duplicate rule replaces an active link, the manager emits the old link's `Disconnected` before the new link's `Connected`. The daemon processes one ordered event stream, so a stale disconnect cannot remove the new session.
- When a link is installed, the manager emits `Connected` before it starts the link reader task. The daemon session therefore exists before any `Packet` event from that link.
- Each established link gets a `LinkHandle` backed by a bounded channel. The link task selects between inbound frames, outbound packets and the close signal. Inbound frames become `ManagerMessage::Inbound`; the task does not gate.
- The gate runs in the manager:
  - `kdeconnect.pair` from any authenticated link is decoded. A malformed pair body closes the link. A valid request becomes `LinkEvent::PairingRequest` with `paired` set from the trust table.
  - `kdeconnect.identity` after establishment is a protocol violation. Close.
  - Any other packet from an unpaired peer is dropped with a debug log.
  - Any other packet from a paired peer becomes `LinkEvent::Packet`.
  - The unpaired allow list is one helper, `is_unpaired_allowed(ptype)`, used by the gate and the tests.
- `SetTrust { pin: Some }` is accepted only if the active link's certificate fingerprint equals the pin. A stale prompt from a replaced link cannot pin a different certificate. When an unpaired link becomes paired, the manager drops its unpaired admission permit, which frees a slot. `SetTrust { pin: None }` clears trust and closes the active link, so a paired link cannot become an unbounded unpaired link. The command acknowledges after the table update. The next unit awaits the acknowledgement before sending the pairing acceptance, so the gate is already open when the peer's first paired packet arrives. A failed pin never clears trust.
- The manager runs a 10 s tick. It closes an unpaired active link when `now - last_activity > unpaired_idle_timeout` (60 s). It expires stale peer entries on a 60 s tick. `last_activity` updates on every `Inbound` message, including pair packets.
- Connection and pairing events use `send().await` because they are rare and must not be lost. Packet events use `try_send`; if the daemon's bounded channel is full, the packet is dropped with a warning.
- Shutdown stops the accept loop, sends close to every active link, waits for link tasks with a 5 s timeout, then aborts stragglers.

The manager does not know `DeviceHost`. It emits `LinkEvent::Packet`; the daemon routes it to a session with a channel send.

### Daemon composition and gate

- The daemon owns one `DeviceSession` per **paired** connected device. The session owns `DeviceHost`, the device `Device` value and a bounded inbound channel.
- On `Connected`, the daemon first updates the device record from the authenticated identity (`record_authenticated_identity`), not from UDP. Then:
  - If `paired` is true, create the session, call `DeviceHost::on_connect` inside the session task, and send any output packets through the `LinkHandle`.
  - If `paired` is false, store a `PendingLink { peer, certificate, link, queued }`. Do not create a `DeviceHost` and do not call `on_connect`. No plugin sees an unpaired peer.
- On `Packet`, the daemon calls `session.deliver`. The session dispatches through `DeviceHost::dispatch`, sends `Dispatch.packets` through the link, and forwards `Dispatch.events` as `DaemonEvent::PluginEvents`. If a packet arrives before the session exists, it is queued on the `PendingLink` (bounded at 32) and drained when the session is created.
- On `Disconnected`, the daemon calls `SessionHandle::stop(reason)` on the removed session. The session task receives `SessionInput::Disconnected`, calls `DeviceHost::on_disconnect`, sends any final output best effort, and exits. The daemon also removes the pending link.
- The gate is in the manager, before the daemon. An unpaired ping never enters the daemon's channel.
- The pairing prompt unit plugs in at `LinkEvent::PairingRequest`. It receives the peer, the request, the peer certificate, the `paired` state and a `LinkHandle`. It prompts, and on accept calls the one trust writer (`store.set_trusted` plus `links.set_trust`), sends the accept packet, and creates the `DeviceSession` for the now-paired link.
- This unit logs pairing requests and does not answer them. A reject sent before a prompt exists would teach peers a state the next unit cannot reverse without a protocol edge case.

### Startup and shutdown ordering

Run mode:

1. Parse args and initialize tracing.
2. Create the data directory. On Unix set it to 0700.
3. Open the store and set the state file to 0600 on Unix.
4. Load or create the device ID.
5. Load or generate the `TlsIdentity` and persist it.
6. Bind `LinkListener` with `bind_in_range(args.tcp_port)`. The default is 1716; the valid range is 1716-1764. A value of 0 or an out-of-range value is rejected.
7. Read the actual port and build the full local `Identity` and the `LocalEndpoint`.
8. Build the trust table and start the link manager with the listener, local endpoint, trust, limits and event channel.
9. Bind discovery with a `DiscoveryAnnouncement` and start the announce/receive task. The manager exists before the first sighting.
10. Run the event loop until Ctrl+C.

`--print-identity` mode:

- It prints the configured identity and exits. It does not generate a certificate, does not write `identity_v1`, does not bind a TCP port and does not announce.
- `--tcp-port` must be in 1716-1764. `0` and out-of-range values are rejected with a clear error.
- The test occupies the configured TCP port before running the command, which proves the command did not bind it.

Shutdown:

- Ctrl+C sends a shutdown signal to the discovery task and stops the event loop.
- The daemon calls `links.shutdown()` and then stops every session with `SessionHandle::stop`. The manager's shutdown closes every active and pending link; the daemon does not have a session handle for a pending link. This ensures `on_disconnect` runs for paired sessions even though the event loop is no longer reading `LinkEvent`s.
- Discovery stops announcing and receiving.
- The manager stops accepting, closes every link, and waits for link tasks.
- The daemon awaits the discovery task, the manager task and the session tasks with a 5 s timeout, then aborts. The listener is dropped.

### What this design deliberately does not do

- It does not put certificate bytes or rustls in `handfast-proto`.
- It does not make `handfast-net` depend on `handfast-store`.
- It does not make `handfast-runtime` own sockets or TLS.
- It does not add a second trust table. The peer pin lives on the device record.
- It does not use a min-initiator duplicate rule. The real peers use last-successful wins.
- It does not add an application-level idle timeout to paired links. KDE Connect links can be quiet; TCP keepalive detects dead peers. Unpaired links get a 60 s idle timeout.
- It does not auto-unpair on a pin mismatch. An attacker must not be able to clear trust by reconnecting.
- It does not implement the pairing prompt, pairing code or persistent user trust UI.

### Interface depth

The public surface is small relative to the behavior behind it. `LinkListener::bind_in_range/accept`, `connect` and `LinkManager::start` hide the plaintext hello, both TLS versions, certificate parsing, pin policy, identity re-exchange, framing, keepalive, admission policy, timeouts, duplicate resolution and shutdown. Callers do not see a TLS type, a certificate DER type or a protocol stage. The daemon sees domain types: a local endpoint, a peer, an identity, a fingerprint, a packet handle.

The trust policy has one home. `PeerCertificate::check` is the only function that decides whether a certificate belongs to a peer. The gate and admission policy have one home in the manager. The per-device host has one owner in the session. The store is the only writer of durable bytes and pins.

## Data flow

1. The process opens the store and loads the stable device ID from `settings_v1`.
2. It loads `identity_v1`. If missing, it generates an ECDSA P-256 key and a self-signed certificate with CN equal to the device ID and a validity window of one year back to ten years ahead, then writes both in one transaction.
3. It binds the TCP listener by scanning 1716-1764 from the configured start port. The bound address supplies the actual `tcpPort`.
4. It builds the full local `Identity` and the `LocalEndpoint`, then starts the link manager.
5. It builds the minimal `DiscoveryAnnouncement` with the actual port, binds UDP discovery, and starts announcing. The announce cannot advertise a port the process has not bound.
6. Discovery receives a peer announcement. It records a sighting and tells the manager the peer ID and the announced TCP port. The announcement parser accepts the desktop minimal packet and the Android full packet.
7. The manager either dials or accepts. A dial sends the full plaintext hello, then acts as TLS server. An accept reads the hello, then acts as TLS client.
8. After the TLS handshake, each side parses the peer certificate and checks CN and pin. Then both exchange the full identity over TLS. TLS 1.2 and TLS 1.3 are both accepted.
9. The manager checks the three-way ID correlation and the protocol version. It admits the link under the unpaired cap and the inbound per-IP limit, applies the last-wins duplicate rule, and emits `LinkEvent::Connected` with the trust state.
10. The link task forwards inbound frames to the manager. The manager gates them. Pair frames become events. Unpaired non-pair frames are dropped. Paired frames become `LinkEvent::Packet`.
11. The daemon updates the device record from the authenticated identity. It creates a `DeviceSession` only for a paired link. The session calls `on_connect`, dispatches packets, sends plugin output and calls `on_disconnect` on close.
12. Shutdown closes discovery, the listener and every link, then drains the sessions.

## Trust model

### Local identity

- One ECDSA P-256 key pair and one self-signed X.509 certificate per device.
- The certificate common name is the device ID. The device ID is generated as a random UUIDv4 with hyphens removed, so it is 32 lowercase hex characters and satisfies the 32-38 character protocol rule. The generated certificate has exactly one subject CN; rcgen's default subject is replaced, not appended.
- The certificate validity is one year before generation to ten years after, matching Android's `initialiseCertificate`. This matters if a peer validates the window.
- The private key is stored as PKCS#8 DER in `identity_v1`. The certificate is stored as DER. Both live in the redb state file.
- On Unix the data directory is 0700 and `state.db` is 0600. On Windows the file inherits the user's profile ACLs; this is a known gap and is listed in the risks.
- A stored identity is never silently regenerated. A corrupt record, a CN mismatch, a key mismatch or a device ID mismatch with `settings_v1` is a fatal startup error. Regenerating would silently change this device's fingerprint and break every existing paired peer.
- The fingerprint is SHA-256 over the full DER certificate, lowercase hex. It is public and safe to log.

### Unpaired peer

- The TLS handshake requires a client certificate from the dialer and presents the local certificate from the listener. This matches the real clients' key managers and Android's trusted `needClientAuth`.
- The peer certificate must parse, contain exactly one valid device ID as CN, and match the expected peer ID. For the listener the expected ID is the plaintext hello; for the dialer it is the discovery identity that triggered the dial.
- No pin is compared.
- The gate allows only `kdeconnect.pair` after the identity exchange. Every other packet is dropped before the daemon.
- The daemon does not create a `DeviceHost` and does not call `on_connect` for an unpaired link. No plugin sees the peer.
- The manager caps established unpaired links at 42, rate-limits incoming connections per IP at 500 ms with 1024 tracked entries, and closes an unpaired link idle for 60 s.
- The pairing prompt unit can read the certificate and fingerprint from `LinkEvent::PairingRequest`, show it to the user, and pin it on acceptance.

### Paired peer

- The device record has a `certificate_fingerprint`.
- The peer certificate CN must equal the expected device ID.
- The SHA-256 of the full DER certificate must equal the stored pin.
- The plaintext hello device ID, the certificate CN and the encrypted identity device ID must all be equal.
- The protocol version must be 8 in the hello, the encrypted identity and the target fields.
- Any mismatch closes the link before a packet is emitted. The daemon logs a security event with the peer ID, the address and the reason.
- A pin mismatch does not clear the pin. A stolen device ID cannot force an unpair by reconnecting with a new certificate.
- A duplicate link replaces the old one only when the certificate DER matches. Last successful handshake wins. A different certificate is rejected.
- A `pair=false` from the authenticated peer is the unpair signal. The next unit handles it.

### Migration trust rule

`DeviceRecord.paired` is a legacy display flag. The pin is the only cryptographic trust. A record with `paired = true` and no fingerprint is treated as unpaired. This can happen in a state file written by v0.2.0, where pairing was never transport-backed. The next unit's first successful pairing writes the pin and sets `paired` from it. `set_trusted` is the only writer that keeps the two fields in sync.

### Version rule

Only protocol 8 is accepted. The identity parser already rejects other versions. The plaintext hello accepts a target version as a number or a string and requires 8. There is no fallback to version 7 and no reduced-security path.

## Failure modes

| Failure | Behavior | Evidence |
| --- | --- | --- |
| TCP range exhausted | Exit nonzero before discovery announces. | Occupy 1764 and start at 1764. |
| `--tcp-port` 0 or out of range | Reject with a clear error. No bind. | CLI test. |
| Bound port outside 1716-1764 | Should be impossible; defensive check exits. | Unit test. |
| Discovery packet is desktop-minimal | `DiscoveryAnnouncement` accepts it; identity parser stays strict. | Fixture and integration test. |
| Discovery has an out-of-range `tcpPort` | Ignore the announcement. | Parser test. |
| Connect refused or timed out | Log, back off per peer, retry on later discovery. | Manager test. |
| Plaintext hello malformed | Close before TLS. | Link test. |
| Hello target ID or version mismatch | Close before TLS. | Link test. |
| Hello target version is a string | Accept `"8"` as 8. | Proto test with Android fixture. |
| Hello omits type or capabilities | Accept; defaults to Unknown and empty. | Proto test with desktop fixture. |
| Hello claims our own ID | Close. | Link test. |
| TLS below 1.2 | Handshake rejected. | Raw TLS 1.1 ClientHello to the TLS server, or after a valid plaintext hello. |
| TLS 1.2 peer | Handshake succeeds. | Testkit TLS 1.2 test. |
| TLS 1.3 peer | Handshake succeeds. | Testkit TLS 1.3 test. |
| RSA peer certificate | TLS 1.2 and 1.3 handshakes succeed. | RSA fixture test. |
| TLS handshake failure | Close; log at debug. | Link test. |
| Certificate CN missing, duplicated or invalid | Close; security event. | Identity unit test and link test. |
| Paired peer presents a different certificate | Close; keep the pin; security event. | Link test. |
| Encrypted identity ID differs from hello or CN | Close; security event. This is the CVE-2025-66270 regression. | Link test. |
| Encrypted identity version is not 8 | Close; security event. | Link test. |
| Malformed pair body | Close; protocol violation. | Manager test. |
| Frame over 1 MiB, malformed JSON or deeply nested JSON | Proto or serde error; close. The 1 MiB cap bounds the allocation. | Link test. |
| Duplicate connection with the same certificate | Replace the old link; last successful handshake wins. | Manager test with a replacer peer. |
| Duplicate connection with a different certificate | Close the new link. | Manager test. |
| Silent peer after TCP accept | Handshake timeout; pending permit released. | Link test with a silent peer. |
| Unpaired link idle for 60 s | Manager closes it. | Manager test with paused time. |
| 43rd unpaired link | Admission rejects before TLS. | Manager test. |
| More than one inbound connection from one IP in 500 ms | Admission rejects before TLS. | Manager test. |
| Simultaneous discovery, both sides dial | Outbound reservations skip the IP window; both reciprocal accepts succeed. | Manager test. |
| Peer disappears without FIN | Keepalive eventually errors; manager removes the link. | Keepalive option test; no EOF-only claim. |
| 256 unique discovery IDs | Oldest non-active peer is evicted; a later real peer recovers. | Manager flood test. |
| Corrupt identity record | Fatal startup error; no regeneration. | Store and daemon tests. |
| Old pre-audit identity table | Load if valid; fail on device ID mismatch. | Store migration test. |
| v0.2.0 state with paired devices and no pin | Treated as unpaired. | Store migration test. |
| Shutdown with links open | Close listener, signal links, wait 5 s, abort leftovers. | Manager shutdown test. |
| Plugin panic or timeout | Existing `DeviceHost` isolation; link stays open. | Existing runtime tests. |

## Test plan

The test plan is written as the set of checks the implementation unit must pass. Each check names the artifact, not the internal function.

### Provider spike

- Add rustls 0.23 with `ring` and `tls12`, tokio-rustls with `ring` and `tls12`, rcgen with `ring`.
- Build on Linux and on `windows-latest` with `RUSTFLAGS=-C target-feature=+crt-static`.
- A test constructs the ring `CryptoProvider`, builds a TLS 1.2 and TLS 1.3 `ServerConfig` and `ClientConfig`, and completes a loopback handshake with a minimal inline rustls client/server pair. The spike does not use the testkit peer, which lands in unit 5.
- If the Windows static build fails, try aws-lc-rs only after a `deny.toml` license review and a CMake/NASM build check. Record the result before the link unit starts.

### Proto and fixture tests

- `IdentityHello` round-trips a full identity plus numeric target fields.
- It accepts the Android connection fixture with a string `targetProtocolVersion`.
- It accepts the desktop connection fixture with a numeric `targetProtocolVersion` and no `deviceType` or capability arrays.
- It rejects a missing or malformed device ID.
- It rejects a target device ID that is not valid.
- It rejects a target protocol version other than 8, in either representation.
- It rejects a hello whose device ID equals the target.
- `DiscoveryAnnouncement` accepts the desktop UDP fixture with only `deviceId`, `deviceName`, `protocolVersion` and `tcpPort`.
- `DiscoveryAnnouncement` accepts the Android full UDP fixture and ignores the extra fields.
- `DiscoveryAnnouncement` rejects a missing `tcpPort`.
- `DiscoveryAnnouncement` rejects a port outside 1716-1764 and a protocol version other than 8.
- `Identity::from_packet` still rejects the desktop UDP fixture. The permissive parser is only for discovery.

Fixtures are wire packets, not KDE code:

- Android connection packet: full identity fields plus `targetDeviceId` and `targetProtocolVersion` as a string.
- Desktop connection packet: `deviceId`, `deviceName`, `protocolVersion`, `targetDeviceId`, numeric `targetProtocolVersion`.
- Desktop UDP announcement: `deviceId`, `deviceName`, `protocolVersion`, `tcpPort`.
- Android UDP announcement: full identity fields plus `tcpPort`.

### Identity and certificate unit tests

- A generated identity has CN equal to the device ID and a fingerprint equal to SHA-256 of the DER certificate.
- The certificate validity is approximately one year back to ten years ahead.
- The fingerprint hex form is 64 lowercase characters.
- `TlsIdentity::from_parts` rejects an empty certificate, an empty key, a malformed certificate, a CN that is not the device ID, a malformed key, and a key that does not match the certificate public key.
- `LocalEndpoint::new` rejects an identity whose device ID differs from the TLS identity.
- `PeerCertificate::parse` rejects no CN, more than one CN, an empty CN, an invalid device ID and trailing DER after the certificate.
- `PeerCertificate::check` accepts an unpaired peer with a matching CN.
- It rejects an unpaired peer with a different CN.
- It accepts a pinned peer with a matching fingerprint.
- It rejects a pinned peer whose certificate changed.
- It never treats a missing pin as trust.

### TLS version and algorithm tests

These are TLS-only loopback tests. They do not run the plaintext hello, certificate check or identity exchange, which belong to the link unit.

- A minimal rustls client/server pair restricted to TLS 1.2 completes a handshake.
- A minimal rustls client/server pair restricted to TLS 1.3 completes a handshake.
- A raw TLS 1.1 ClientHello is rejected at the TLS layer. Send it directly to the TLS server side of the loopback pair. This test does not go through the link handshake, which would fail at the plaintext hello first.
- A rustls client/server pair using the RSA certificate and PKCS#8 key fixture completes TLS 1.2 and TLS 1.3 handshakes. This proves the RSA signature schemes in both accept-any verifiers.
- The accept-any verifiers' `supported_verify_schemes` includes the RSA and ECDSA schemes the provider reports.

### Link integration tests

These use the independent testkit TLS peer, not a second copy of the production code.

- A dialer and listener complete the hello, TLS handshake, certificate check and identity exchange, then exchange a ping.
- A testkit peer restricted to TLS 1.2 completes the hello, certificate check and identity exchange.
- A testkit peer restricted to TLS 1.3 completes the same sequence.
- A testkit peer with an RSA certificate completes the full link over TLS 1.2 and TLS 1.3.
- The dialer's hello contains the full identity and the target fields. The listener accepts it.
- A desktop-shaped hello with no type or capabilities is accepted.
- A listener presents a certificate whose CN is a different device ID. The dialer closes before identity.
- A dialer presents a certificate whose CN is a different device ID. The listener closes.
- A testkit peer sends a plaintext hello for ID A and an encrypted identity for ID B. The link closes with an identity mismatch. This is the CVE-2025-66270 regression.
- A testkit peer sends a string target version. The link proceeds. A testkit peer sends a non-8 target version. The link closes.
- A pinned peer is accepted when the certificate matches.
- A pinned peer is rejected when the certificate changes, and the pin remains in the trust table.
- A peer that connects and sends nothing hits the handshake timeout.
- A peer that sends a partial identity frame hits the identity timeout. After establishment there is no idle read timeout for paired links; the 1 MiB cap and keepalive bound the damage.
- A peer that declares a frame larger than 1 MiB is closed before a larger allocation.
- An unpaired peer can complete the link, but a ping is dropped and a pair request is emitted.
- An unpaired link creates no `DeviceHost` and receives no `on_connect` callback.
- A testkit peer opens a second link for the same device with the same certificate. The manager keeps the new link and emits `Disconnected` for the old one before `Connected` for the new one.
- A testkit peer opens a second link with a different certificate. The manager closes the new link.
- The keepalive helper sets the configured idle, interval, retries and user timeout. The Unix test reads all four back from the socket.

### Testkit tests

- A testkit TLS self-test runs two independent peers over loopback. They generate their own certificates, complete the hello, TLS and identity exchange, and exchange a ping.
- The testkit peer supports TLS 1.2 only, TLS 1.3 only, and both.
- The testkit peer supports an RSA certificate and key fixture.
- The testkit peer can be configured to present a wrong CN, a certificate that does not match a seeded pin, an identity mismatch, a protocol mismatch, a string target version, a silent handshake and an oversized frame.
- The testkit peer can open a replacement link for an existing device and close the old one.
- The testkit does not import `handfast-proto` or `handfast-net` in its normal build. The conformance tests may use production types as consumers.

### Daemon integration tests

- Occupy TCP port 1764 and start `handfastd` with `--tcp-port 1764`. The daemon cannot scan past the range and exits nonzero without announcing. Do not occupy only the configured start port, because `bind_in_range` scans upward.
- Occupy an in-range port below 1764 and start the daemon at that port. It binds the next free port and announces the actual port in 1716-1764.
- Start the daemon with `--tcp-port` in 1716-1764 and a free discovery port. A testkit peer announces itself. The daemon dials, completes TLS, and sends its encrypted identity. This proves bind-before-announce and the discovery-to-manager wiring.
- Bind a second UDP socket with `SO_REUSEADDR` on the daemon's discovery port before starting it. Receive the daemon's announcement datagram, parse it with `DiscoveryAnnouncement`, and assert `tcpPort` is in 1716-1764 and not zero. This is the port-0 regression test.
- Send a desktop-minimal UDP announcement to the daemon. The daemon persists the device and attempts a dial to the advertised port. This is the discovery compatibility integration test.
- Start the daemon with `--tcp-port` 0. It exits with a clear error and does not announce.
- Start the daemon with `--tcp-port` 1700 or 1800. It exits with a clear error.
- `--print-identity` with a port in range. Occupy that TCP port first. The command succeeds, prints the identity, writes no `identity_v1` record and does not bind the port.
- Create a v0.2.0 state file with `settings_v1.device_id` and a discovered device. Start the daemon. Assert that `identity_v1` is created, the certificate CN matches the device ID, a restart reuses the same certificate and fingerprint, and the device record survives.
- Create a state file with `identity_v1` whose device ID disagrees with `settings_v1`. The daemon exits with a clear error and does not announce.
- An unpaired testkit peer completes the link. The daemon holds a `PendingLink`, creates no `DeviceHost`, and logs the pair request. The test can assert the peer receives no plugin packet.
- A paired testkit peer disconnects. The daemon calls `SessionHandle::stop`; the session runs `DeviceHost::on_disconnect` before exit. Test with a recording plugin factory.
- The existing `announced_devices_are_persisted` test must use a free daemon TCP port in 1716-1764 and a different free port for the test peer's advertised `tcpPort`. It must not hardcode 1716.

### Manager and policy tests

- 42 unpaired links can be established. The 43rd is rejected before TLS.
- A second incoming connection from one IP inside 500 ms is rejected; after 500 ms it is accepted.
- An outbound dial does not mark the inbound per-IP window. Simultaneous discovery with two Handfast peers completes both dials; both reciprocal `accept` calls return `Ok(Link)` inside 500 ms, and last-wins converges without a deadlock.
- An unpaired link idle for 60 s is closed by the tick; a paired idle link is not.
- The peer table expires a stale peer after `peer_ttl`.
- After a 256-ID discovery flood, a real testkit peer is admitted and connected. The test asserts the real peer's link event.
- A duplicate link with the same certificate replaces the old one. A duplicate with a different certificate is rejected.
- Shutdown closes all links and releases every admission permit.

### Security regression tests

- Plaintext hello ID, certificate CN and encrypted identity ID all differ pairwise. Each mismatch must fail. This is the CVE-2025-66270 matrix.
- A paired peer with a changed certificate is rejected and the pin is not removed.
- A `SetTrust` with a fingerprint that does not match the active link's certificate is rejected.
- An unpaired peer sends a `kdeconnect.ping` and a plugin packet. Neither reaches the `DeviceHost`. A pair packet does reach the pairing event.
- A spoofed UDP identity with a different name does not overwrite the authenticated name of a connected device.
- A peer connecting to the TLS server without a client certificate is rejected by the mandatory client-auth setting.
- TLS below 1.2 is rejected. TLS 1.2 and 1.3 are accepted.
- An RSA peer is accepted.

### CI

All nine existing jobs must stay green:

1. `fmt`.
2. `clippy` with `-D warnings`.
3. `test` for the workspace and doc tests.
4. `msrv` on Rust 1.90.0 with `--locked`.
5. `deny` for advisories, licenses, bans and sources.
6. `docs` with `RUSTDOCFLAGS="-D warnings"`.
7. `testkit self-test`, extended with the TLS 1.2 and TLS 1.3 self-test.
8. `fuzz`, existing frame and identity targets.
9. `windows` build with the static CRT. This is also the provider spike for ring.

## Dependency plan

| Crate | Version | Used by | License | MSRV | Why |
| --- | --- | --- | --- | --- | --- |
| `rustls` | 0.23.46 | `handfast-net`, `handfast-testkit` | Apache-2.0 OR ISC OR MIT | 1.71 | TLS 1.2 and 1.3, custom verifiers, ring provider. |
| `tokio-rustls` | 0.26.6 | `handfast-net`, `handfast-testkit` | MIT OR Apache-2.0 | 1.71 | Async TLS over Tokio streams. |
| `rcgen` | 0.14.10 | `handfast-net`, `handfast-testkit` | MIT OR Apache-2.0 | 1.88 | Self-signed ECDSA P-256 certificate generation. |
| `x509-parser` | 0.18.1 | `handfast-net` | MIT OR Apache-2.0 | 1.67 | Parse the peer certificate and extract the CN. |
| `sha2` | 0.11.1 | `handfast-net` | MIT OR Apache-2.0 | 1.88 | SHA-256 certificate fingerprint, independent of the TLS provider. |
| `time` | 0.3.55 | `handfast-net` | MIT OR Apache-2.0 | 1.88 | Certificate validity window matching Android. |
| `rustls-pki-types` | 1.16.0 (transitive) | via rustls | MIT OR Apache-2.0 | 1.60 | Certificate and key DER types. Kept out of the public API. |
| `ring` | 0.17.14 (transitive) | via rustls and rcgen | Apache-2.0 AND ISC | 1.66 | TLS crypto provider. Not a direct dependency. |

Feature choices:

```toml
rustls = { version = "0.23.46", default-features = false, features = ["ring", "std", "tls12"] }
tokio-rustls = { version = "0.26.6", default-features = false, features = ["ring", "tls12"] }
rcgen = { version = "0.14.10", default-features = false, features = ["crypto", "ring"] }
x509-parser = "0.18.1"
sha2 = "0.11"
time = "0.3"
```

`default-features = false` removes `aws-lc-rs` and the post-quantum preference. The explicit `tls12` feature is required. `rcgen`'s default `pem` feature is removed; the store uses DER only. `tokio-rustls`'s default `aws_lc_rs` feature is replaced by `ring`.

`handfast-net` uses rustls, tokio-rustls, rcgen, x509-parser, sha2 and time. `handfast-testkit` uses rustls, tokio-rustls and rcgen. `handfast-daemon` adds `handfast-runtime` and `handfast-desktop` path dependencies so it can name `DeviceHost`, `Registry` and `Desktop`; it already depends on `handfast-plugins`. `handfast-net` and `handfast-daemon` add `handfast-testkit` as a dev-dependency for the integration tests. `handfast-daemon` adds no TLS dependency. `rustls-pki-types` stays transitive so no rustls type appears in the public API.

### ring versus aws-lc-rs

`rustls` 0.23 defaults to `aws-lc-rs`. That pulls `aws-lc-sys` 0.47.0, whose license expression includes `MIT-0` in an OR branch. The expression is acceptable under the current allow list because the other branches are allowed, but it adds a license family that `deny.toml` does not name and a CMake/NASM build. `ring` is `Apache-2.0 AND ISC`, both named, and needs only the C toolchain already used by the Windows runner. `rcgen` with the ring feature uses the same provider as rustls. One crypto implementation covers key generation, TLS and signatures. The tradeoff is that ring is C and assembly rather than pure Rust. The workspace's `unsafe_code = "deny"` applies to Handfast code, not to audited upstream crates. The smaller crypto surface and the simpler build favor ring.

The provider spike decides. If ring cannot build on Windows with the static CRT, fall back to aws-lc-rs only after checking `cargo-deny` against the full license expression and confirming CMake and NASM on the Windows runner. The rest of the design is provider-neutral behind the rustls config builder.

### sha2 versus ring::digest

`sha2` is a direct dependency so the fingerprint function does not depend on the TLS provider. If the provider changes, the certificate fingerprint stays stable. `sha2` 0.11.1 is pure Rust and has an allowed license. The cost is a small transitive digest tree.

### X.509 parsing

`x509-parser` is the only new parser. It is used once, on the peer certificate DER, to extract the subject CN. It has no crypto feature enabled. The alternatives are `rustls-webpki`, which does not expose the subject CN as a convenience, and hand-rolled DER parsing, which would add unsafe-adjacent complexity for no benefit. The parser consumes untrusted input, so the parser version must be kept current and a fuzz target is a sensible follow-up when the link unit lands.

### MSRV and deny

The highest new MSRV is 1.88 (`rcgen`, `sha2`, `time`). The workspace MSRV is 1.90. All licenses are in `deny.toml`'s allow list. No wildcard versions, no git sources. `multiple-versions` remains a warning, not a failure. The Windows build uses the static CRT; the provider spike proves ring or selects the fallback.

## Migration

### From a v0.2.0 state file

A v0.2.0 state file has `settings_v1` with `device_id`, `devices_v1` records and `plugin_state_v1`. It has no `identity_v1`.

On the first TLS-aware start:

1. Open the existing database without clearing tables.
2. Read `device_id` from `settings_v1`. If it is absent, generate and store it.
3. `identity_v1` is absent. Generate a self-signed certificate for the existing device ID and persist it in one write transaction. Do this before binding the listener and before discovery announces.
4. Device records deserialize with `certificate_fingerprint = None`. A record with `paired = true` and no pin is treated as unpaired. No pin is invented.
5. The link manager trusts only pins. The first successful pairing after the next unit writes one.

If the process crashes between generation and persistence, the next start generates a new certificate. No peer can have seen the old one because the listener has not been bound and discovery has not announced.

### From a pre-audit state file with `identity_v1`

The pre-audit table used key `1` and JSON `{ device_id, cert_der, key_der }`. The new store uses the same table name and shape.

- If the record is present and its device ID equals `settings_v1.device_id`, load it. Parse the certificate and key. Require CN equal to the device ID and the key to match the certificate.
- If the device ID differs, fail closed. Two stable identities in one state file is corruption, and choosing one silently could break a peer's pin.
- If the record is present but invalid, fail closed. Do not regenerate. A paired peer may have pinned this certificate.

### File permissions

On Unix, the data directory is created with 0700 and the redb file is set to 0600 after creation. The private key is in that file. On Windows, the file inherits the user profile ACL. A future unit can add an explicit DACL through a Windows-specific crate; this unit does not add an unsafe or platform-specific dependency for that.

### `--print-identity`

It remains a read-only diagnostic. It loads or creates the device ID, builds the identity from the configured port and prints it. It does not bind, generate a certificate, write `identity_v1` or announce. `--tcp-port` must be in 1716-1764; `0` and out-of-range values are rejected. The existing CLI test that asserts a stable identity and a port in range continues to pass.

### Read compatibility

`DeviceRecord` gains one optional field with `#[serde(default)]`. Old records deserialize. New records are readable by old code because serde ignores the unknown field by default. The `identity_v1` table is invisible to old code. A downgrade is not safe: an old binary that rewrites a device record drops `certificate_fingerprint` and silently loses the pin. Treat a downgrade as a re-pair.

## Implementation sequence

Each unit ends in a check. Do not start the next unit until the check passes.

0. **Provider spike.** Add rustls, tokio-rustls and rcgen with ring and `tls12`, plus a small loopback TLS 1.2 and 1.3 config test. Build on Linux and on Windows with `+crt-static`. Record ring or the justified fallback. Check: CI `windows` and `test` jobs on a scratch branch.
1. **Store identity and trust records.** Add `identity_v1`, `IdentityRecord`, `Store::{identity, set_identity, set_trusted, trust_table, record_authenticated_identity}` and the optional `certificate_fingerprint` field. Tests: round-trip, reopen, v0.2.0 migration, pre-audit table load, paired-without-pin is unpaired, discovery preserves trust and does not rename, authenticated identity updates name/type and preserves trust. Check: `cargo test -p handfast-store`.
2. **Proto compatibility.** Add `IdentityHello` with the full identity body and string-or-number target version. Add `DiscoveryAnnouncement` with the minimal desktop shape. Add the Android, desktop connection and desktop UDP fixtures. Remove the stale TLS claim from the crate doc. Check: `cargo test -p handfast-proto` and `cargo fuzz build`.
3. **Discovery parser.** Change `handfast-net::discovery` to parse and emit `DiscoveryAnnouncement` and accept the desktop minimal UDP fixture. Test the minimal packet and the Android full packet. This lands before the link unit, as the review requires. Check: `cargo test -p handfast-net discovery`.
4. **Identity types and checks.** Add `Fingerprint`, `TlsIdentity`, `LocalEndpoint`, `PeerCertificate`, `PeerTrust`, `TlsError`, the certificate validity window and the pure checks. No sockets. Tests: generation, CN, validity, fingerprint, pin, malformed DER, local endpoint mismatch. Check: `cargo test -p handfast-net identity`.
5. **Independent testkit TLS peer and RSA fixture.** Add `tls_peer` with TLS 1.2/1.3, the RSA certificate and PKCS#8 key fixture, minimal UDP announcement and replacement mode. Add `tls-self-test`. This unit lands before the TLS config and link units because both use the peer and the RSA fixture. Check: `cargo test -p handfast-testkit` and `cargo run -p handfast-testkit -- tls-self-test`.
6. **TLS 1.2 and 1.3 configuration.** Add the rustls configs, `verify_tls12_signature`, `verify_tls13_signature`, RSA schemes and the accept-any verifiers. Test with a minimal TLS-only loopback client/server for both versions and with the RSA fixture from unit 5. Check: the TLS-only version and algorithm tests. They do not use the testkit peer or the link handshake.
7. **Both link roles.** Add `LinkListener::bind_in_range/accept`, `connect`, the full plaintext hello, the full local identity, the identity re-exchange, the framed link and the keepalive helper. Test with the testkit peer from unit 5 for TLS 1.2 and 1.3. Check: the full-link happy-path tests for hello, certificate check and identity exchange.
8. **Negative link tests.** Add wrong CN, pin mismatch, identity mismatch, protocol mismatch, string/number target, timeouts, oversized frame, TLS 1.1 rejection and RSA. Check: `cargo test -p handfast-net --test tls_link`.
9. **Link manager and policy.** Add last-wins dedupe, trust snapshot, gate, admission policy (42 unpaired, inbound per-IP 500 ms, idle 60 s), peer expiry and flood recovery, backoff, limits, events and shutdown. Tests: duplicate replacer, unpaired cap, inbound rate limit, simultaneous discovery, idle, flood recovery, shutdown. Check: `cargo test -p handfast-net --test manager`.
10. **Daemon composition.** Add the port-range scan, full local endpoint, `Arc<Registry>` sessions, `SessionHandle::stop` on disconnect, discovery announcement wiring, `--print-identity` no-cert/no-bind behavior, Unix permissions and the updated discovery test ports. Tests: range, print identity, migration, desktop UDP announcement, discovery-driven dial, disconnect `on_disconnect`. Check: `cargo test -p handfast-daemon`.
11. **CI and docs.** Update the architecture state table and the README "what works" only after the code lands. Run fmt, clippy, test, msrv, deny, docs, testkit self-test, fuzz and Windows. Check: all nine checks green.

The store and proto units are data-only and can land before any TLS code. The discovery parser unit lands before the link unit. The identity, testkit and TLS config units open no link listener. The link listener starts at unit 7. This ordering means a reviewer can reject the trust model and the provider before the first `TcpListener` exists.

## Synthesis decision

This package is a synthesis of two whole-shape candidates, revised after two adversarial reviews at head `30c761f`. Model diversity was not established. The environment blocks nested subagents at depth 1, so both candidates and the judgment ran in the same parent process on the single configured model. There was no independent runner and no model-diverse cross-judge. The coordinator and the independent verifier provide the independent review that the arena could not.

### Candidates

- Candidate 1, "transport-owned TLS": no new crate. `handfast-net` owns the TLS material, both roles, the link and the manager. `handfast-store` owns opaque DER and pins. The daemon composes and owns the per-device host.
- Candidate 2, "identity and session as first-class layers": a new `handfast-identity` crate owns certificate material, fingerprinting and peer trust. `handfast-net` exposes a private typestate handshake and a per-peer session actor. `handfast-runtime` gains a `PacketTransport` trait so the runtime owns the per-device gate. The daemon is a thin wiring layer.

### Base

Candidate 1, "transport-owned TLS," is the base. It has the smaller public surface and the fewer crates. It matches the architecture table, which already names `handfast-net` as the owner of TCP links and the pairing state machine. Its manager actor gives one owner to sockets, dedupe and lifecycle. Its store change is one table and one optional field.

The reviews confirmed the accepted core shape: transport-owned TLS in `handfast-net`, `handfast-proto` transport-free, no new identity crate, no runtime `PacketTransport` trait. Revision 2 does not reopen those decisions. It corrects protocol facts and lifecycle mechanics inside the accepted shape.

### Grafts from candidate 2

- **Pure identity policy split from rustls.** Candidate 2 separated certificate parsing, CN checking and pin policy from the TLS adapter. The base has a private `identity` module and a thin `tls` module. `PeerCertificate::check` is the single policy function, and the accept-any verifiers only verify proof of possession.
- **`sha2` for fingerprints.** Candidate 2 chose `sha2` over `ring::digest`. The synthesis accepts the decoupling. A certificate fingerprint should not change because the TLS provider changed.
- **One trust writer.** Candidate 2's `IdentityService` made trust a single facade. The base states that the pairing unit is the only caller that writes both the store pin and the manager trust snapshot.
- **Explicit private handshake state.** Candidate 2's typestate named the phases. The synthesis keeps the phases as a private enum and a single `connect`/`accept` driver.
- **Explicit session input enum.** Candidate 2's session actor made the packet, paired and disconnect transitions explicit. The daemon session now has `SessionInput`, and `SessionHandle::stop` constructs `Disconnected`.

### Rejected from candidate 2

- **New `handfast-identity` crate.** It isolates identity knowledge well, but it adds a crate and makes `handfast-store` depend on identity types. The same split is achieved by a private module in `handfast-net` without the dependency. If pairing codes, certificate rotation or trust revocation grow the policy surface, revisit the crate.
- **Public typestate.** Exposing `Connecting -> HelloSent -> TlsEstablished -> Reauthenticated -> Active` makes callers coordinate several methods. That is the shallow-module red flag. One driver is the interface.
- **Runtime `PacketTransport` trait.** It has one production implementation and moves the gate from the composition root into `handfast-runtime`. The runtime is a plugin host, not a session host.
- **Gate in the runtime.** The architecture says unpaired packets are dropped before the plugin layer. The manager gate does that earlier and keeps the runtime unchanged.

### Revision 2 re-grounding

The two candidates were sketched before the real Android and desktop sources were available. Revision 2 keeps the base but changes these protocol decisions:

- TLS 1.2 is required, not optional. `tls12`, `verify_tls12_signature` and RSA schemes are in the base.
- The hello is the full identity plus target fields, not four fields. The parser accepts the smaller desktop packet and a string or numeric target version.
- The port is scanned in 1716-1764, not any port.
- Unpaired links are capped and rate-limited, and have an idle timeout.
- Duplicate links are last-wins, not min-initiator.
- Discovery gets its own permissive announcement parser.
- Keepalive values are explicit.
- The peer table expires and evicts.
- `LocalEndpoint` carries the full local identity, `LinkLimits` reaches the manager, `Registry` is shared through `Arc`, and `Disconnected` stops the session.

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
| Missing state in a type | `LocalIdentity` lacked the full local identity. | `IdentityService` held it. | `LocalEndpoint` now owns TLS material and the full identity. |
| Hidden configuration | The manager could not see `LinkLimits`. | Same. | Limits are an explicit manager parameter. |
| Stale session | Disconnect removed the session without stopping it. | Session actor handled it. | `SessionHandle::stop` constructs `SessionInput::Disconnected`. |

Both candidates converged on the important security shape: ECDSA P-256, CN = device ID, accept-any verifier plus post-handshake identity check, three-way ID correlation, pin-only trust, store raw DER, bind before announce. That convergence is strong signal. The reviews confirmed the trust model. Revision 2 keeps it and fixes the protocol and lifecycle gaps.

## Tradeoffs accepted

- We accept TLS 1.2 support, including RSA signature schemes and a TLS 1.2 test peer, in exchange for compatibility with the official Android client.
- We accept the port scan in 1716-1764, in exchange for real-peer compatibility. A user cannot bind an arbitrary TCP port.
- We accept the unpaired cap, the inbound per-IP rate limit and the unpaired idle timeout, in exchange for DoS resistance that matches the real peers. The idle timeout is a Handfast policy, not a copied peer value.
- We accept last-successful-handshake-wins for duplicate links, in exchange for matching Android and desktop. Two simultaneous Handfast dials may take one extra reconnect to converge.
- We accept a permissive discovery parser and a strict encrypted identity parser, in exchange for desktop UDP compatibility without weakening the authenticated path.
- We accept a post-handshake certificate check instead of an in-handshake pin verifier, in exchange for one shared TLS config and one pure policy function. No application data is read before the check.
- We accept one new table (`identity_v1`) and one optional device field, in exchange for not adding a separate trust table.
- We accept a manager actor with a command channel, in exchange for one owner of sockets, dedupe and lifecycle.
- We accept a short mutex around the admission counters, in exchange for one exact cap across concurrent accept tasks. It does not guard sockets or link state.
- We accept no application idle timeout for paired links, in exchange for not disconnecting quiet but healthy peers. TCP keepalive detects dead peers. Unpaired links do get an idle timeout.
- We accept ring's C build over aws-lc-rs, in exchange for a smaller crypto surface and no CMake/NASM requirement. The provider spike can still force a fallback.
- We accept `sha2` and `time` as direct dependencies, in exchange for a provider-independent fingerprint and an Android-shaped certificate validity window.
- We accept that a corrupt local identity is fatal, in exchange for never silently changing the device fingerprint.

## Alternatives considered

- **New `handfast-identity` crate.** It would own certificate material, the verifier and the trust policy, with `handfast-net` and `handfast-store` depending on it. It isolates policy and tests without sockets, but it adds a crate and either makes the store depend on crypto or needs feature splits. The private module in `handfast-net` gets most of the benefit. Lose on interface size and dependency weight.
- **Public typestate handshake.** It makes illegal states unrepresentable for callers, but callers must coordinate five methods to complete one operation. That is a shallow module. The private enum keeps the implementation explicit and the public API deep.
- **Pin verification inside the rustls verifiers.** It rejects earlier in the handshake, but the listener does not know the expected peer ID until after the plaintext hello, and a shared server config cannot hold per-peer pins. A per-connection config would duplicate the policy. Lose on one-policy simplicity.
- **TLS material in `handfast-proto`.** This is the deleted `cert` module. It puts transport and crypto in a crate whose contract is transport-free. Lose on layering.
- **Separate trust table.** It would keep device discovery data and trust apart, but it creates a second place that encodes pairing state. The optional field on the device record is one writer and one read path. Lose on split ownership.
- **Runtime `PacketTransport` trait.** It would let the runtime own the session and gate over any transport. It has one production implementation and expands the runtime contract. Lose on unnecessary abstraction.
- **TLS 1.3 only.** It is simpler and avoids RSA, but the official Android client would not connect. Lose on real-client compatibility.
- **Min-initiator duplicate tie-break.** It converges two Handfast peers deterministically, but the real clients replace the old link. Matching them is more important than a local tie-break. Lose on real-client compatibility.
- **Single permissive identity parser.** It would accept the desktop UDP packet, but it weakens the encrypted identity validation. Separate strict and permissive parsers keep the trust boundary clear. Lose on safety.

## Open questions and risks

- The existing pairing state machine allows 300 s of clock skew. The protocol reference describes a 30-minute skew for the pairing timestamp. The pairing prompt unit should reconcile this before it ships. It does not affect the TLS handshake.
- Windows state-file ACLs are inherited, not explicitly restricted. The private key is protected by the user profile ACL. A later unit can add an explicit DACL.
- Ring's C build on Windows with the static CRT is not yet proven by this document. Implementation unit 0 is the gate. If ring fails, aws-lc-rs is the fallback after a `deny.toml` and CMake/NASM review.
- The UDP announcement test relies on two `SO_REUSEADDR` UDP sockets on the same port receiving a broadcast on the CI kernel. If the Linux runner does not deliver it, the test needs a deterministic unicast announce target, which would add test-only surface. Verify in unit 10 before writing the test.
- The 60 s unpaired idle timeout is a Handfast policy. The real peers do not expose a matching constant in the reviewed sources. The pairing prompt unit must coordinate it with the 30 s pairing request window, or the link can close while a prompt is open.
- The 500 ms inbound per-IP interval is the desktop value. Android uses 1 s. Handfast chooses the stricter inbound value. This is a deliberate divergence, not a protocol requirement. Outbound dials use the unpaired cap and per-peer backoff, not the IP window.
- `x509-parser` and its `time` dependency add a moderate transitive tree. Keep the parser updated and consider a cert-parse fuzz target when the link code lands.
- The testkit TLS peer duplicates handshake logic on purpose. It must stay independent enough to catch a production bug. This is a maintenance cost, not a one-time cost.
- The runtime's `DeviceHost` has no event bus. This unit logs plugin events. The IPC/event surface is future work.
- The real-client tests are still needed before the P1 exit: one Android TLS 1.2 connection and one desktop TLS 1.3 connection, including an RSA certificate case if a real peer presents one.

## Next implementation step

Add the rustls ring and `tls12` dependencies and run the provider spike on Linux and Windows with the static CRT. If the spike passes, add the store's `identity_v1` table and trust methods with the v0.2.0 and pre-audit migration tests. Both are small and verifiable before any socket code.

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
- Git history: `db6ffed` (audit that deleted `crates/handfast-proto/src/cert.rs` and the old `identity_v1` table), `6f85e3f` (discovery wiring), and the deleted v1 `docs/PROTOCOL.md`.

Real KDE Connect sources, read for protocol behavior only. No KDE code is copied into Handfast.

- Android `SslHelper.kt`: `SSLContext("TLSv1.2")`, client/server mode, client auth for trusted and untrusted devices, certificate generation with CN = device ID and a one-year-back to ten-year-ahead validity window, SHA-256 certificate hash.
- Android `LanLinkProvider.kt`: UDP and TCP handshake order, one plaintext hello from the dialer, full identity plus string target fields, TLS server on dial, TLS client on accept, 1716-1764 port scan, 42 unpaired cap, 1 s per-IP rate limit, encrypted identity device ID and protocol version correlation, last-successful-link replacement.
- Android `DeviceInfo.kt`: full identity packet fields, minimal validation requiring a non-blank device name and a valid device ID.
- Desktop `lanlinkprovider.cpp`: UDP minimal discovery packet, TLS server on dial, TLS client on accept, connection packet fields, port range, 42 unpaired cap, 500 ms per-IP rate limit, encrypted identity correlation, last-successful-link replacement, keepalive values.
- Desktop `deviceinfo.h`: `toUdpDiscoveryPacket`, `toConnectionPacket` and `toIdentityPacket` field sets, validation rules.

Third-party protocol references and advisories:

- KDE Connect protocol reference, schemas: https://raw.githubusercontent.com/KDE/kdeconnect-meta/work/protocol-schemas/protocol.md
- Third-party protocol reference: https://raw.githubusercontent.com/timschneeb/kdeconnect-nx/master/protocol.md. Its four-field hello is insufficient; the real clients above win.
- KDE security advisory CVE-2025-32900: https://kde.org/info/security/advisory-20250418-2.txt
- KDE security advisory CVE-2025-66270: https://kde.org/info/security/advisory-20251128-1.txt
- GitHub advisory GHSA-xcg9-fw4f-9chv: https://github.com/advisories/GHSA-xcg9-fw4f-9chv
- Cezar Lungu, KDE Connect security disclosure: https://cezarlungu.com/kde-connect-security-disclosure/
- crates.io and `cargo info` metadata for rustls 0.23.46, tokio-rustls 0.26.6, rcgen 0.14.10, x509-parser 0.18.1, rustls-pki-types 1.16.0, ring 0.17.14, aws-lc-rs 1.19.0, aws-lc-sys 0.47.0, sha2 0.10.9 and 0.11.1, time 0.3.55.
- `socket2` 0.6.5 source for `TcpKeepalive` and `set_tcp_user_timeout`.
