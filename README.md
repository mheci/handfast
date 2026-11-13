# Handfast

Clean-room, Rust-native KDE Connect implementation.

> **P1 link milestone on `main`.** `handfastd` now discovers peers, binds a
> KDE Connect TCP port, completes a TLS 1.2 or TLS 1.3 link with certificate
> pinning and protocol-8 identity re-exchange, prompts for pairing and runs the
> ping and battery plugins for paired devices. Payload and file transfer,
> desktop backends and most plugins are still not wired. The 0.2.0 packages
> remain a foundation and packaging milestone.

## Install

The 0.2.0 packages are attached to the GitHub release.

Windows 11, x64:

```text
handfast-0.2.0-x64.msi
```

Open the MSI and run `handfastd` from `C:\Program Files\Handfast`. The 0.2.0
MSI is unsigned. The release workflow signs it when the Windows signing secrets
are configured. State is written to `%LOCALAPPDATA%\handfast`.

Debian 12, amd64:

```sh
sudo apt install ./handfastd_0.2.0_debian12_amd64.deb
```

Ubuntu 24.04, amd64:

```sh
sudo apt install ./handfastd_0.2.0_ubuntu24.04_amd64.deb
```

Arch Linux, x86_64:

```sh
sudo pacman -U handfastd-bin-0.2.0-1-x86_64.pkg.tar.zst
```

The Linux packages install `/usr/bin/handfastd`. There is no service unit yet.
The daemon is a console program. Run it directly:

```sh
handfastd --print-identity
handfastd
```

Linux state lives in `$XDG_DATA_HOME/handfast`, or
`~/.local/share/handfast` when `XDG_DATA_HOME` is unset. Use `--data-dir` to
override it.

## Build from source

Requires Rust 1.90 or newer.

```sh
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo run -p handfast-testkit -- self-test
```

## What works on main

- Packet framing: 4-byte big-endian length, JSON body, 1 MiB hard limit.
- Packet validation: namespaced type, object body, payload fields that appear
  together, payload size bounds.
- Identity and capabilities: protocol 8 parsing, device ID and name
  validation, control and bidi character removal.
- Pairing state machine with request timestamps, clock skew and thirty-second
  expiry, driven by the daemon prompt.
- TLS link: ECDSA P-256 certificate with CN equal to the device ID, TLS 1.2
  and TLS 1.3, mutual client certificates, certificate pinning, protocol-8
  plaintext and encrypted identity correlation, keepalive and bounded
  handshakes.
- Link manager: 42 unpaired links, inbound per-IP rate limit, peer expiry,
  last-successful-handshake duplicate resolution, backoff and shutdown.
- Daemon pairing prompt: the terminal prompt prints the peer name, device ID
  and certificate fingerprint, pins on `y` and writes no pin on reject.
- Plugin runtime: registry, per-device host, five-second timeout, panic
  isolation and permanent disable after a failure.
- Plugin roster: all 31 KDE Connect plugins are listed. Ping, battery and
  clipboard text are implemented. Clipboard requires a desktop adapter.
- State store: redb tables for devices, per-device plugin state, settings and
  the daemon TLS identity. Trust and pairing updates are atomic.
- Independent testkit with TLS 1.2 and 1.3 peers, golden vectors and four fuzz
  targets.
- `handfastd --print-identity` and a stable, persisted device ID and
  certificate.
- UDP discovery in `handfastd`: identity announcements, peer receive and
  device persistence. A peer that announces this daemon's own ID is ignored.

Not wired yet:

- Payload and file transfer over the second TCP connection.
- Linux desktop backends for clipboard, notifications, media, volume, input
  and the remaining plugins.
- The other 28 plugins.
- The real Android and desktop interop run. The checklist is in
  [docs/INTEROP.md](docs/INTEROP.md); it is the P1 exit gate.

The 0.2.0 packages open no network sockets. On `main`, `handfastd` announces
its identity over UDP discovery on port 1716, binds a TCP port in 1716-1764,
accepts or dials peers, runs the TLS link and pairing prompt, and persists
discovered and authenticated devices until Ctrl+C.

## Architecture

| Crate | Responsibility |
| --- | --- |
| `handfast-proto` | Packet model, framing, identity, capabilities, pairing bodies |
| `handfast-net` | UDP discovery, TLS links, link manager, pairing state machine |
| `handfast-runtime` | Plugin trait, registry, per-device host, supervision |
| `handfast-plugins` | 31-plugin roster and the implemented plugins |
| `handfast-desktop` | Desktop adapter traits and the mock backend |
| `handfast-store` | redb state, settings and migrations |
| `handfast-testkit` | Independent wire peer, TLS peers, golden vectors and self-test |
| `handfast-daemon` | The `handfastd` binary and the pairing prompt |
| `xtask` | Local fmt, clippy, test and doc tasks |
| `fuzz` | Separate cargo-fuzz workspace for the frame and permissive parsers |

The v1 implementation is archived at tag `v1-archive`. No v1 source is copied
into v2. Protocol behavior is re-derived from the KDE Connect protocol and the
official client behavior.

## Protocol target

- KDE Connect protocol version 8.
- 4-byte big-endian length, JSON body, 1 MiB hard limit.
- UDP discovery on port 1716.
- TCP control links in the 1716-1764 range. Payload transfer uses a second
  connection to the port announced in `payloadTransferInfo.port`.
- TLS 1.2 and TLS 1.3 with certificate pinning, CN equal to the device ID and
  protocol-8 identity re-exchange.

The target is wire compatibility with the official KDE Connect Android and iOS
apps, and behavioral parity with the 31 plugins shipped by `kdeconnect-kde`.

## Security

- The workspace denies `unsafe_code`.
- Frame size, payload size and packet structure are validated before use.
- Peer-provided device names are stripped of control and bidi characters.
- A peer certificate must have exactly one CN equal to its device ID, and the
  plaintext hello, certificate CN and encrypted identity must all name the same
  device.
- Trust is the SHA-256 certificate pin. `paired = true` without a pin is not
  trusted.
- Unpaired peers can send only pairing packets; no plugin host exists for them.
- The private key is zeroized when `TlsIdentity` drops.
- `handfast-proto` has fuzz targets for frame decoding, the strict identity,
  the plaintext hello and the discovery announcement.
- The Windows state file inherits the user profile ACL. An explicit protected
  DACL is a P1 security follow-up with its API and test named in
  [ARCHITECTURE.md](docs/ARCHITECTURE.md#windows-state-file-acl-follow-up).
- The Windows release signs the executable and MSI when
  `WINDOWS_CERT_PFX_BASE64` and `WINDOWS_CERT_PASSWORD` are configured, then
  verifies both signatures. The 0.2.0 MSI is unsigned.

## Development

CI runs on GitHub Actions:

- formatting, clippy with warnings denied, tests and doc tests;
- MSRV 1.90, `cargo-deny`, documentation build and testkit self-test;
- nightly fuzzing for the frame, identity, hello and discovery parsers;
- a Windows build with the static CRT used by the MSI and the Windows
  `handfast-net` socket-option tests.

Releases are tag driven. The tag must match the workspace version, for example
`v0.2.0`. The release workflow builds native Debian 12, Ubuntu 24.04 and Arch
Linux packages plus the Windows MSI, then publishes them as GitHub release
assets. See [docs/RELEASING.md](docs/RELEASING.md) for the signing secrets and
the release runbook.

## Roadmap

- **P0 Foundation** (0.2.0): workspace, protocol codec, identity, runtime
  contract, 31-plugin roster, desktop traits, store, testkit, packaging and CI.
- **P1 Link and core transfer**: discovery, TLS, pairing, capabilities, ping,
  battery, share and file transfer against a real client.
- **P2 Desktop sync**: clipboard, notifications both ways, MPRIS both ways,
  volume both ways, contacts, connectivity, telephony, SMS, find and lock,
  screensaver inhibition.
- **P3 Input and files**: mousepad, remote keyboard and control, presenter,
  digitizer, run commands, SFTP.
- **P4 Display and advanced**: virtual monitor, input sharing, ModemManager
  telephony, TUI and GUI parity.

## Documentation

- [Architecture](docs/ARCHITECTURE.md): crate layout, runtime, link layer,
  plugin model, desktop adapters, security and CI.
- [Interop](docs/INTEROP.md): the real Android and desktop checklist, commands,
  expected identity, certificate and pairing behavior, and failure capture.
- [Parity matrix](docs/PARITY.md): the 31 plugins, packets, backends, phases
  and test status.
- [Releasing](docs/RELEASING.md): tags, signing secrets, package assets and
  install commands.

## License

MIT. Handfast speaks the KDE Connect wire protocol but shares no code with the
KDE projects and is not affiliated with KDE e.V.
