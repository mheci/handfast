# Architecture

## Goals

1. Full parity with the 31 plugins shipped by `kdeconnect-kde` on Linux.
2. Wire compatibility with KDE Connect protocol version 8 and the official
   Android/iOS apps.
3. One Rust workspace, one async runtime, one plugin model.
4. Rust-only test and release tooling. No Python peer, no shell packaging
   scripts in the critical path.
5. A CI that is fast, deterministic and honest: every required job is a real
   gate.

## Non-goals for v2

- Replacing the Android or iOS clients.
- Windows and macOS desktop backends. The adapter traits leave room for them,
  but v2 ships Linux only.
- Cloud relay, accounts or telemetry.

## Compatibility contract

The wire contract is frozen independently of the implementation:

- Protocol version 8.
- Packet frame: 4-byte big-endian length followed by UTF-8 JSON. Maximum frame
  size 1 MiB.
- UDP discovery on port 1716. TCP control and payload links use the 1716-1764
  range.
- TLS 1.2 and TLS 1.3. Certificates are self-signed, the device ID is the
  certificate common name, and peers pin the certificate fingerprint learned
  during pairing. TLS 1.2 is required by the official Android client.
- Protocol-8 identity re-exchange: the plaintext identity, the certificate CN
  and the TLS identity must agree on device ID, and every protocol version must
  be 8. A mismatch aborts the link.
- Payload transfers use a second TCP connection to the port announced in
  `payloadTransferInfo.port`.

`handfast-testkit` owns the golden vectors and the independent Rust peer. The
production codec must not be the only implementation used to test itself.

## Workspace

| Crate | Responsibility | Must not depend on |
| --- | --- | --- |
| `handfast-proto` | Packet model, framing, identity, capabilities | networking, desktop |
| `handfast-net` | UDP discovery, TCP links, pairing state machine | plugins, desktop |
| `handfast-runtime` | Plugin trait, registry, per-device host, events, supervision | concrete plugins |
| `handfast-plugins` | 31-plugin roster and plugin implementations | daemon, UI |
| `handfast-desktop` | Desktop adapter traits, mock backend, Linux backends | protocol |
| `handfast-store` | redb tables, settings, device and plugin state | runtime |
| `handfast-testkit` | Independent wire peer, golden vectors, conformance harness | production codec (dev-dependency only) |
| `handfast-daemon` | `handfastd` composition root | UI |
| `xtask` | Format, lint, test and packaging entry points | runtime |

Planned: `handfast-ipc` (Unix socket frames), `handfast-cli` (`hfctl`),
`handfast-tui` (ratatui), `handfast-gui` (iced).

Dependency direction is enforced by review and by crate boundaries, not by a
cycle-checking tool.

## Runtime model

```
                       +-------------------+
 UDP announce  ------> |  handfast-net     |
 TCP + TLS     ------> |  link + pairing   |
                       +---------+---------+
                                 |
                                 v
                       +-------------------+
                       | handfast-runtime  |
                       | device host       |
                       | plugin supervisor |
                       +----+---------+----+
                            |         |
                +-----------+         +-------------+
                v                                   v
       +-------------------+               +-------------------+
       | handfast-plugins  |               | handfast-desktop  |
       | 31 plugin modules |               | D-Bus, Wayland,   |
       +-------------------+               | PipeWire, libei   |
                                           +-------------------+
```

One `handfastd` process owns:

- the discovery socket and all device links;
- one `LinkManager` actor, which owns the TCP listener, admission policy, trust
  snapshot, duplicate resolution and link lifecycle;
- one `DeviceHost` per paired connected device;
- the plugin registry and enabled-plugin set;
- the redb store;
- the desktop adapters;
- the IPC socket for `hfctl` and the GUI.

Each device link is a Tokio task. Each plugin invocation is bounded by a timeout
and wrapped in `catch_unwind`; a panicking plugin is disabled for that device
and reported over the event bus. Plugins never own sockets. They receive decoded
packets and return outbound packets plus typed events.

### Link layer

The daemon binds a TCP listener in 1716-1764 before discovery announces. The
link manager dials a discovered peer or accepts an inbound connection. The
dialer sends the plaintext identity hello and acts as the TLS server; the
listener acts as the TLS client, matching the real clients. After the handshake
both sides parse the peer certificate, check the CN and pin, and exchange the
full identity over TLS. The link emits a `Connected` event only after all three
device IDs agree.

Unpaired links are admitted under a 42-link cap and a 500 ms inbound per-IP
window, are idle-closed after 60 s and can send only `kdeconnect.pair`. The
daemon creates no `DeviceHost` for an unpaired peer. The terminal pairing prompt
is the only writer of peer trust; it pins the certificate fingerprint on `y`.
A paired link runs one session task with a `DeviceHost` and calls
`on_disconnect` when the link closes.

## Plugin model

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn meta(&self) -> &'static PluginMeta;

    async fn on_connect(&mut self, ctx: &mut PluginContext<'_>) -> PluginResult {
        Ok(PluginOutput::default())
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> PluginResult;

    async fn on_disconnect(&mut self, ctx: &mut PluginContext<'_>) -> PluginResult {
        Ok(PluginOutput::default())
    }
}
```

`PluginOutput` contains outbound packets and typed `PluginEvent`s. The host
forwards packets to the link and events to the daemon/UI.

`PluginMeta` is the single source of truth:

```rust
pub struct PluginMeta {
    pub name: &'static str,
    pub title: &'static str,
    pub status: PluginStatus,       // Implemented | Planned
    pub incoming: &'static [&'static str],
    pub outgoing: &'static [&'static str],
    pub desktop: DesktopRequirement, // None | Clipboard | Notifications | ...
}
```

Only `Implemented` plugins are advertised in the identity capabilities. A
`Planned` plugin is visible in the parity matrix and the CLI, but it is never
announced to a peer and never receives packets.

## Desktop adapters

`handfast-desktop` defines small traits, not one god object:

| Trait | Used by | Linux backend |
| --- | --- | --- |
| `Clipboard` | clipboard | `wl-clipboard-rs` (Wayland), `x11-clipboard` (X11) |
| `Notifier` | notifications, sendnotifications, findthisdevice | `org.freedesktop.Notifications` over zbus |
| `Media` | mpriscontrol, mprisremote, pausemusic | `org.mpris.MediaPlayer2` over zbus |
| `Volume` | systemvolume, remotesystemvolume | PipeWire or `wpctl` |
| `Input` | mousepad, presenter, digitizer, remotecontrol, remotekeyboard | portal `RemoteDesktop` + libei |
| `Screensaver` | screensaver-inhibit | `org.freedesktop.login1` over zbus |
| `Shell` | share URL, findthisdevice | xdg-open via `std::process::Command` |

Traits return `Result` and are `Send + Sync`. In P1 the daemon composes a
`Desktop` from optional adapters. A plugin whose required adapter is missing is
not advertised on that host; the registry already filters by `Desktop`.

`MockDesktop` implements every trait with in-memory recording. Plugin contract
tests use it; Linux backends get their own integration tests behind `cfg(target_os = "linux")`.

## State

`redb`, a pure-Rust ACID embedded key-value store. No system SQLite, no C
toolchain, identical behavior on Linux and developer machines.

Tables:

- `devices_v1`: device records keyed by ID. Each record carries an optional
  `certificate_fingerprint`, the only cryptographic trust. `paired` is a legacy
  display flag derived from the pin.
- `plugin_state_v1`: per-device enable/disable.
- `settings_v1`: key/value, including the stable `device_id`.
- `identity_v1`: singleton record with the device ID, DER certificate and
  PKCS#8 DER private key. The private key is zeroized when the in-memory
  `TlsIdentity` drops.
- Later: notifications, contacts and transfers.

Schemas are versioned by table name and migrated with copy-on-open. The
database lives at `$XDG_DATA_HOME/handfast/state.db`. On Unix the directory is
0700 and the file is 0600. On Windows both get a protected DACL for the current
user; see [Windows state-file and incoming-file ACLs](#windows-state-file-and-incoming-file-acls).

## IPC

Planned: a Unix domain socket at `$XDG_RUNTIME_DIR/handfast/handfastd.sock`,
peer-credential checked. Frames are 4-byte length-prefixed JSON with a 1 MiB
limit, reusing the same envelope rules as the wire protocol but a separate
schema. Requests are idempotent; events are broadcast.

## Security

- TLS 1.2 and TLS 1.3 with pinned peer certificates. TLS below 1.2 is rejected.
- The certificate CN must equal the peer device ID. The plaintext hello,
  certificate CN and encrypted identity must all name the same device. This is
  the CVE-2025-66270 correlation.
- Trust is the SHA-256 certificate pin. A record with `paired = true` and no
  pin is unpaired. A pin mismatch never clears the pin.
- Pairing requests carry a timestamp and are bound to the TLS certificate.
  Pairing code derivation lands in P1.
- Unpaired devices can send only pair packets. Every other packet is dropped
  before the plugin layer, and no `DeviceHost` exists for an unpaired peer.
- Frame size, payload size and JSON nesting are capped before allocation.
- The private key is held in `Zeroizing<Vec<u8>>` and zeroized when
  `TlsIdentity` drops.
- File transfers reject absolute paths, `..`, symlink escapes and reserved names.
- Run commands are opt-in, stored as argv arrays, never shell-interpolated.
- The daemon runs as a user service with `NoNewPrivileges`, `ProtectSystem=strict`
  and a restricted address family set.

### Windows state-file and incoming-file ACLs

The Unix path sets the state directory to 0700 and `state.db` to 0600. Windows
gets an explicit protected DACL instead of the inherited profile ACL. The same
policy covers the incoming root and its `.handfast-tmp` directory.

The DACL policy:

- One access-allowed ACE for the current user SID from the process token
  (`OpenProcessToken` plus `GetTokenInformation(TokenUser)`) with
  `FILE_ALL_ACCESS`.
- `SetNamedSecurityInfoW` with `SE_FILE_OBJECT` and
  `DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION`, so
  inherited profile ACEs are removed.
- A directory's ACE carries `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE`. A
  `state.db` or incoming temp file created later starts with the same single
  user ACE before its own explicit call.
- No SYSTEM or Administrators ACE. The state holds the device private key and
  incoming files hold user data. Those principals can already take ownership or
  use backup privileges, so a permanent read ACE adds exposure without adding a
  capability.

The implementation is `handfast-desktop::winacl`, the only module with
`windows-sys` 0.61 calls. The daemon already depends on `handfast-desktop`, so
the helper is shared without a new crate and the unsafe surface stays one
reviewed file. `GetNamedSecurityInfoW` reads the DACL back. Applying the same
ACL twice is idempotent. It runs:

- on the data directory at startup, before the store opens;
- on `state.db` immediately after the store creates it;
- on the incoming root and `.handfast-tmp` in `LocalFiles::new`, whether they
  were created or already existed.

On Windows a failure to set the DACL is fatal, not a warning.

The test is a `#[cfg(windows)]` read-back. It asserts the DACL is protected,
has exactly one ACE, and that ACE grants `FILE_ALL_ACCESS` to the current user
SID. It covers the state directory, the state file, the incoming root and
`.handfast-tmp`. The `windows` CI job runs it. The second local user rejection
is a manual step because CI has no second interactive account:

1. Create a standard local user.
2. As an administrator, create `C:\acl-test` and grant the second user read
   access to that folder only. Do not let the grant inherit.
3. As the primary user, run `handfastd --data-dir C:\acl-test\data` and stop it
   after `state.db` exists.
4. As the second user, `type C:\acl-test\data\state.db` and
   `dir C:\acl-test\data` must both be denied. The second user can read
   `C:\acl-test`, so the denial comes from the state DACL.

The incoming root and `.handfast-tmp` get the same DACL from
`LocalFiles::new`; the read-back test covers them.

## CI design

Required jobs:

1. `fmt` - `cargo fmt --all -- --check`.
2. `clippy` - `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
3. `test` - `cargo test --workspace --all-features --all-targets`, plus doc tests.
4. `msrv` - Rust 1.90.0, `cargo check --locked --workspace --all-features`.
5. `deny` - `cargo-deny` for advisories, licenses, bans and sources. One
   security gate, not two overlapping ones.
6. `docs` - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`.
7. `testkit-self-test` - `cargo test -p handfast-testkit` and the self-test
   and TLS self-test binaries. This proves the independent peer and the
   production codec agree and that TLS 1.2 and 1.3 complete with ECDSA and RSA
   certificates.
8. `fuzz` - nightly `cargo-fuzz` smoke for the frame, strict identity,
   plaintext hello and discovery announcement parsers.
9. `windows` - `cargo build --workspace --all-features --locked` plus
   `cargo test -p handfast-net --all-features` on `windows-latest` with the
   static CRT used by the MSI. The test step covers the Windows keepalive
   socket-option read-back.

Later phases add, only when the code exists:

10. `wayland` - headless compositor smoke for desktop adapters.

No `continue-on-error`. A lane that is not ready is not a CI job; it is a task
in the roadmap.

## Release packaging

A tag matching the workspace version runs `.github/workflows/release.yml`.
The tag must match `[workspace.package] version`, for example
`v0.2.0`. The workflow builds native packages in the target
environment:

- Debian 12 amd64 `.deb` in `rust:1.90-bookworm`.
- Ubuntu 24.04 amd64 `.deb` on `ubuntu-24.04`.
- Arch Linux x86_64 `handfastd-bin` `.pkg.tar.zst` in `archlinux:latest`.
- Windows 11 x64 `.msi` with WiX, built with the static CRT.

The Windows job signs `handfastd.exe` and the MSI when the signing secrets are
configured, verifies both signatures and otherwise publishes an unsigned MSI.
The `publish` job attaches every asset to the GitHub release. The required
secrets and the install commands are in [RELEASING.md](RELEASING.md).

## Migration

- v1 remains at tag `v1-archive`.
- v2 replaced `main` as the P0 foundation. It is alpha and not a usable client
  until the P1 exit criteria pass: discovery, TLS, pairing, ping, battery, share
  and file transfer against a real KDE Connect client.
- A v0.2.0 state file has `settings_v1`, `devices_v1` and `plugin_state_v1` and
  no `identity_v1`. On the first P1 start the daemon creates and persists a
  certificate for the existing device ID before binding the listener. Device
  records deserialize with no pin. A record with `paired = true` and no pin is
  treated as unpaired; no pin is invented. A restart reuses the stored
  certificate and fingerprint.
- A stored identity is never regenerated. A corrupt record, a CN mismatch or a
  device ID that disagrees with `settings_v1` is a fatal startup error.
- No v1 source is copied into v2. Protocol behavior is re-derived from the KDE
  protocol reference and the official client behavior.

## Roadmap

- **P0 Foundation** - workspace, protocol codec, identity, runtime contract,
  31-plugin roster, desktop traits, store, Rust peer, CI.
- **P1 Link and core transfer** - discovery, TLS, pairing, capabilities,
  daemon, share and file transfer.
- **P2 Desktop sync** - clipboard, notifications both ways, MPRIS both ways,
  volume both ways, contacts, connectivity, telephony, SMS, find and lock,
  screensaver inhibit.
- **P3 Input and files** - mousepad, remote keyboard/control, presenter,
  digitizer, run commands, SFTP.
- **P4 Display and advanced** - virtual monitor, input sharing, ModemManager
  telephony, TUI and GUI parity.
