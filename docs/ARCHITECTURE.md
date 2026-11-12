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
- UDP discovery on port 1716. TCP control and payload links use the 1714-1764
  range.
- TLS 1.3. Certificates are self-signed, the device ID is the certificate common
  name, and peers pin the certificate fingerprint learned during pairing.
- Protocol-8 identity re-exchange: the plaintext identity and the TLS identity
  must agree on device ID and protocol version. A mismatch aborts the link.
- Payload transfers use a second TCP connection to the port announced in
  `payloadTransferInfo.port`.

`handfast-testkit` owns the golden vectors and the independent Rust peer. The
production codec must not be the only implementation used to test itself.

## Workspace

| Crate | Responsibility | Must not depend on |
| --- | --- | --- |
| `handfast-proto` | Packet model, framing, identity, capabilities, certificates, fingerprints | networking, desktop |
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
- one `DeviceHost` per connected device;
- the plugin registry and enabled-plugin set;
- the SQLite store;
- the desktop adapters;
- the IPC socket for `hfctl` and the GUI.

Each device link is a Tokio task. Each plugin invocation is bounded by a timeout
and wrapped in `catch_unwind`; a panicking plugin is disabled for that device
and reported over the event bus. Plugins never own sockets. They receive decoded
packets and return outbound packets plus typed events.

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

Traits return `Result` and are `Send + Sync`. The daemon composes a `Desktop`
from optional adapters. A plugin whose required adapter is missing is not
advertised on that host.

`MockDesktop` implements every trait with in-memory recording. Plugin contract
tests use it; Linux backends get their own integration tests behind `cfg(target_os = "linux")`.

## State

`redb`, a pure-Rust ACID embedded key-value store. No system SQLite, no C
toolchain, identical behavior on Linux and developer machines.

Tables:

- `devices_v1`: device records keyed by ID.
- `plugin_state_v1`: per-device enable/disable.
- `settings_v1`: key/value.
- `identity_v1`: daemon certificate material.
- Later: notifications, contacts and transfers, keyed per device.

Schemas are versioned by table name and migrated with copy-on-open. The
database lives at `$XDG_DATA_HOME/handfast/state.redb`.

## IPC

Planned: a Unix domain socket at `$XDG_RUNTIME_DIR/handfast/handfastd.sock`,
peer-credential checked. Frames are 4-byte length-prefixed JSON with a 1 MiB
limit, reusing the same envelope rules as the wire protocol but a separate
schema. Requests are idempotent; events are broadcast.

## Security

- TLS 1.3 with pinned peer certificates.
- Protocol-8 re-exchange validation: plaintext and encrypted identity must match.
- Pairing requests carry a timestamp and are bound to the TLS certificate.
  Pairing codes are derived from both certificate fingerprints.
- Unpaired devices can send only identity and pair packets. Every other packet is
  dropped before the plugin layer.
- Frame size, payload size and JSON nesting are capped before allocation.
- File transfers reject absolute paths, `..`, symlink escapes and reserved names.
- Run commands are opt-in, stored as argv arrays, never shell-interpolated.
- The daemon runs as a user service with `NoNewPrivileges`, `ProtectSystem=strict`
  and a restricted address family set.

## CI design

Required jobs, all on `ubuntu-latest`:

1. `fmt` - `cargo fmt --all -- --check`.
2. `clippy` - `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
3. `test` - `cargo test --workspace --all-features --all-targets`, plus doc tests.
4. `msrv` - Rust 1.90.0, `cargo check --locked --workspace --all-features`.
5. `deny` - `cargo-deny` for advisories, licenses, bans and sources. One
   security gate, not two overlapping ones.
6. `docs` - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`.
7. `conformance` - `cargo test -p handfast-testkit` and the self-test binary.

Later phases add, only when the code exists:

8. `wayland` - headless compositor smoke for desktop adapters.
9. `fuzz` - nightly `cargo-fuzz` smoke for frame, JSON and identity parsers.
10. `package` - `cargo xtask package` for deb, rpm, Arch and portable tarball.

No `continue-on-error`. A lane that is not ready is not a CI job; it is a task
in the roadmap.

## Migration

- v1 remains at tag `v1-archive`.
- v2 is developed on `rewrite/v2` and replaces `main` only after the P1 exit
  criteria pass: discovery, TLS, pairing, ping, battery, share and file transfer
  against a real KDE Connect client.
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
