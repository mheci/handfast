# Handfast

<img src="assets/branding/handfast-mark.svg" alt="Handfast" width="96" height="96" align="right">

Clean-room, Rust-native KDE Connect implementation.

The mark is two interlocking rings: one teal for the desktop, one copper for
the phone, woven on a single axis. See [docs/branding.md](docs/branding.md) for
the palette, files and usage rules.

> **0.3.0 ships one binary.** `handfast` starts the daemon, talks to it, installs
> its own autostart, updates itself and opens a window. `handfastd` and `hfctl`
> are gone. The packages are `handfast`, `handfast-bin` and a per-user
> `handfast-<version>-x64.msi`. See
> [docs/releases/0.3.0.md](docs/releases/0.3.0.md) for the upgrade note.

## Install

The 0.3.0 packages are attached to the GitHub release.

Windows 11, x64:

```text
handfast-0.3.0-x64.msi
```

Open the MSI. It installs per user to `%LOCALAPPDATA%\Handfast`, adds a Start
Menu shortcut named `Handfast` that runs `handfast gui`, and writes state to
`%LOCALAPPDATA%\handfast`. The 0.3.0 MSI is signed when the Windows signing
secrets are configured. The MSI registers the logon task with
`handfast service install` and removes it on uninstall. The MSI adds an inbound
firewall rule named `Handfast-<your user SID>` for the installed executable,
which covers UDP 1716 and TCP 1716-1764, during install. That is the same name
`handfast service install` and `handfast service uninstall` compute from the
current user, so running them from an administrator prompt replaces the
installer's rule instead of adding a second one. If the rule is
missing, run `handfast service install` from an administrator prompt; it adds
the rule when elevated and prints the exact command otherwise.

Debian 12, amd64:

```sh
sudo apt install ./handfast_0.3.0_debian12_amd64.deb
systemctl --user enable --now handfast
```

Ubuntu 24.04, amd64:

```sh
sudo apt install ./handfast_0.3.0_ubuntu24.04_amd64.deb
systemctl --user enable --now handfast
```

Arch Linux, x86_64:

```sh
sudo pacman -U handfast-bin-0.3.0-1-x86_64.pkg.tar.zst
systemctl --user enable --now handfast
```

The Linux packages install `/usr/bin/handfast` and the systemd user unit at
`/usr/lib/systemd/user/handfast.service`. The deb carries `Conflicts`,
`Replaces` and `Provides: handfastd`; the Arch package carries
`conflicts=('handfastd-bin')` and `replaces=('handfastd-bin')`. They upgrade a
0.2.0 install in one step.

Run the same binary for every job:

```sh
handfast daemon            # run the daemon
handfast gui               # open the window
handfast status [--json]   # print the running daemon's status
handfast identity          # print this device's identity packet
handfast share --device <id> <path>
handfast pair --accept|--reject <id>
handfast service install|uninstall|start|stop|status
handfast update check|apply
handfast version
```

`handfast service install` writes a unit or task for the current binary, which
is what a portable install under `~/.local/bin` needs. A packaged install
already has the unit; enable it with `systemctl --user enable --now handfast`.

Linux state lives in `$XDG_DATA_HOME/handfast`, or
`~/.local/share/handfast` when `XDG_DATA_HOME` is unset. Use `--data-dir` to
override it.

## Build from source

Requires Rust 1.90 or newer.

```sh
cargo build --workspace
cargo build -p handfast-daemon --features gui   # with the window
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p handfast-testkit -- self-test
cargo run -p xtask -- build                     # release build with the GUI
```

A plain `cargo build` is the headless build. The `gui` feature needs the X11 and
Wayland development packages on Linux; the `gui` CI job installs
`libx11-dev libxkbcommon-dev libwayland-dev libgl1-mesa-dev`.

## What works

- One `handfast` binary with `daemon`, `gui`, `status`, `identity`, `share`,
  `pair`, `service`, `update` and `version` subcommands, and a global
  `--data-dir`.
- Packet framing: one UTF-8 JSON line per packet, 1 MiB hard limit. The local
  control channel keeps its 4-byte length frame and 64 KiB cap.
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
  and certificate fingerprint, pins on `y` and writes no pin on reject. The same
  prompt answers from the window or `handfast pair --accept|--reject <id>`, so a
  daemon started by the autostart service still pairs. The first answer wins and
  a closed link cancels the prompt.
- Plugin runtime: registry, per-device host, five-second timeout, panic
  isolation and permanent disable after a failure.
- Plugin roster: all 31 KDE Connect plugins are listed and 13 are implemented:
  battery, clipboard, connectivity report, find this device, lock device, MPRIS
  control, notifications, pause music, ping, presenter, screensaver inhibit,
  share and system volume.
- Share and file transfer over the second TLS connection, both directions, with
  a safe incoming folder, no-clobber names and a bounded extra-bytes policy.
- State store: redb tables for devices, per-device plugin state, settings and
  the daemon TLS identity. Trust and pairing updates are atomic.
- Independent testkit with TLS 1.2 and 1.3 peers, golden vectors and four fuzz
  targets.
- `handfast identity`, `handfast status [--json]` and a stable, persisted device
  ID and certificate.
- UDP discovery in `handfast daemon`: identity announcements, peer receive and
  device persistence. A peer that announces this daemon's own ID is ignored.
- Per-user autostart: `handfast service install` writes a systemd user unit or a
  Task Scheduler logon task, starts the daemon and adds the Windows firewall
  rule when elevated. A non-elevated run prints the exact administrator command.
- Self-update: `handfast update check` reads the latest release, and
  `handfast update apply` verifies `SHA256SUMS`, checks Authenticode on Windows
  and replaces the running binary atomically.
- The `gui` feature: an `egui` window that starts, stops and reports the daemon
  over the control channel, and answers pairing requests with the peer name,
  device ID and fingerprint.

Not wired yet:

- 18 of the 31 plugins.
- Windows and macOS desktop backends. The adapter traits leave room for them,
  but v2 ships Linux only.
- The real Android and desktop interop run. The checklist is in
  [docs/INTEROP.md](docs/INTEROP.md); it is the P1 exit gate.

`handfast daemon` announces its identity over UDP discovery on port 1716, binds
a TCP port in 1716-1764, accepts or dials peers, runs the TLS link and pairing
prompt, serves share and the implemented plugins, and persists discovered and
authenticated devices until it stops.

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
| `handfast-daemon` | The `handfast` binary, daemon composition root and control client |
| `xtask` | Local fmt, clippy, test, build, packaging and checksum tasks |
| `fuzz` | Separate cargo-fuzz workspace for the frame and permissive parsers |

The v1 implementation is archived at tag `v1-archive`. No v1 source is copied
into v2. Protocol behavior is re-derived from the KDE Connect protocol and the
official client behavior.

## Protocol target

- KDE Connect protocol version 8.
- One UTF-8 JSON line per LAN link packet, 1 MiB hard limit. The local control
  channel uses a 4-byte big-endian length frame.
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
- The Windows state directory, `state.db`, the incoming root and
  `.handfast-tmp` get a protected DACL for the current user. See
  [ARCHITECTURE.md](docs/ARCHITECTURE.md#windows-state-file-and-incoming-file-acls).
- The Windows release signs the executable and MSI when
  `WINDOWS_CERT_PFX_BASE64` and `WINDOWS_CERT_PASSWORD` are configured, then
  verifies both signatures. The Windows updater refuses an unsigned asset.
- The update path accepts HTTPS to a fixed host allowlist, checks `SHA256SUMS`
  from the same release, verifies Authenticode on Windows and refuses on any
  mismatch. It runs as the user and never elevates.
- The daemon runs as the logged-in user, from a systemd user unit or a Task
  Scheduler logon task, so the desktop adapters keep the user's session bus and
  the state stays in the user's profile.
- The release binary contains BSL-1.0 code and OFL-1.1 and Ubuntu-font-1.0 font
  data. `THIRD-PARTY-NOTICES.md` carries the texts and ships in every package.

## Development

CI runs on GitHub Actions:

- formatting, clippy with warnings denied, tests and doc tests;
- MSRV 1.90, `cargo-deny`, documentation build and testkit self-test;
- the `gui` job, which builds and tests the window with the X11 and Wayland
  development packages;
- nightly fuzzing for the frame, identity, hello and discovery parsers;
- a Windows build with the static CRT used by the MSI and the Windows
  `handfast-net` socket-option tests.

Releases are tag driven. The tag must match the workspace version, for example
`v0.3.0`. The release workflow builds one `handfast` binary per platform, wraps
it in native Debian 12, Ubuntu 24.04, Arch Linux and per-user Windows packages,
adds the portable assets and `SHA256SUMS`, then publishes them as GitHub release
assets. See [docs/RELEASING.md](docs/RELEASING.md) for the signing secrets and
the release runbook, and
[docs/releases/0.3.0.md](docs/releases/0.3.0.md) for the current notes.

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
