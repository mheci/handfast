# Handfast

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![Latest release](https://img.shields.io/github/v/release/mheci/handfast)](https://github.com/mheci/handfast/releases/latest)
[![CI](https://github.com/mheci/handfast/actions/workflows/ci.yml/badge.svg)](https://github.com/mheci/handfast/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/MSRV-1.90-blueviolet)](Cargo.toml)

Connect your Android phone to your Linux desktop over your local network — no cloud,
no account, no phone vendor in the loop. Handfast speaks the **KDE Connect wire
protocol**, so it pairs with the standard [KDE Connect](https://kdeconnect.kde.org/)
Android app and interoperates with other KDE Connect clients.

Handfast is a **from-scratch, Rust-native implementation**: it shares zero code with
the KDE projects (`kdeconnect-kde`, `kdeconnect-android`, libkdeconnect) and is not
affiliated with KDE e.V. The GUI, the TUI/CLI, the daemon, the protocol codec, the CI —
everything is Rust.

Built **Wayland-first**: the daemon talks to your display server through a small
Wayland bridge (virtual input, clipboard, idle inhibit), and the desktop app is an
Iced GUI that runs natively on Wayland.

> Personal project, experimental. Anything can change or break without notice.
> Expect rough edges while the protocol surface matures.

---

## What you can do today

| Area | Status |
| --- | --- |
| Device discovery (UDP broadcast) + pairing + unpairing | ✅ Live |
| TLS connections with **certificate pinning** | ✅ Live |
| Notifications relay — desktop ⇄ phone, with reply/dismiss routing | ✅ Live |
| Battery status (request + periodic reports) | ✅ Live |
| Ping, find-my-phone, pause-music, run-commands, system volume | ✅ Live |
| Share requests (URL / text / file announcement) | ✅ Queued; surfacing lands in Phase 3 |
| SMS sending from the desktop (via paired phone) | ✅ Live |
| TUI (`hfctl tui`) + rich CLI + Iced GUI | ✅ Live |
| File transfer byte streams — KDE Connect-compatible payload channel (send + receive, progress, cancel) | ✅ Live |
| Mousepad input, full two-way clipboard sync, contacts, connectivity report | 🚧 Phase 3–4 |

### File transfers

Files travel over the KDE Connect payload channel, exactly like the reference
implementations: the sender announces `payloadSize` + `payloadTransferInfo`
(`{"port": N}`) in a `kdeconnect.share.request` packet, the receiver dials
that port back, both sides wrap the socket in TLS (receiver = TLS client,
sender = TLS server), and exactly `payloadSize` raw bytes stream over it.
Control packets are newline-delimited JSON, matching upstream. Inbound files
land under `$XDG_DATA_HOME/handfast/downloads` (override with
`--data-dir`); peer-supplied names are sanitized and colliding names get
numbered copies, so a hostile sender can never overwrite existing files.

Use it from the CLI:

```console
hfctl send <device-id> /path/to/file     # send a file
hfctl transfers                           # list in-flight transfers
hfctl transfer-cancel <transfer-id>       # cancel (partial file is removed)
```

## Architecture

Handfast is a 9-crate Cargo workspace:

```
crates/
├── core/       Event bus, SQLite state store, paths, supervisor (panic-restart + backoff)
├── protocol/   KDE Connect-compatible wire protocol: packet framing, identity, TLS, transfer
├── plugins/    Plugin registry + implementations (battery, notifications, share, run-commands, …)
├── wayland/    Wayland bridge: virtual input, clipboard watch, idle inhibit, portal EI
├── ipc/        Local IPC: typed JSON frames over a Unix domain socket (peercred-checked)
├── daemon/     handfastd — the pairing/routing daemon (discovery, handshake, device manager)
├── tui/        hfctl — ratatui TUI + full CLI
├── gui/        handfast-gui — Iced desktop app (Wayland-native)
└── xtask/      Release tooling: dist packaging, shell completions
```

The daemon is the only process that talks to the display server and owns the
network + SQLite state; `hfctl` and `handfast-gui` talk to it over a local Unix
socket. Plugins are **fault-isolated packet transformers**: if one panics, the
supervisor restarts it with exponential backoff instead of killing the session.

## Install

Prebuilt packages are attached to every [release](https://github.com/mheci/handfast/releases):

| Distro | Package | File (from latest release) |
| --- | --- | --- |
| Arch Linux | `handfast` | `handfast-<version>-1-x86_64.pkg.tar.zst` (since 0.1.0) |
| Debian / Ubuntu | `handfast` | `handfast_<version>-1_amd64.deb` (since 0.1.1) |
| Fedora | `handfast` | `handfast-<version>-1.x86_64.rpm` (since 0.1.1) |
| Any Linux | portable | `handfast-<version>-x86_64-unknown-linux-gnu.tar.zst` |

### Arch Linux — prebuilt package

```sh
pacman -U handfast-*-x86_64.pkg.tar.zst
```

### Arch Linux — build it yourself (AUR-style)

Use the shipped `PKGBUILD` (also attached to every release):

```sh
curl -LO https://github.com/mheci/handfast/releases/latest/download/PKGBUILD
makepkg -si
```

### Debian / Ubuntu — prebuilt package

```sh
sudo apt install ./handfast_<version>-1_amd64.deb
```

Installs `handfastd` / `hfctl` / `handfast-gui`, the systemd user unit, desktop
file, icon, and shell completions. Runtime deps (`libwayland-client0`,
`libxkbcommon0`, `libsqlite3-0`) are pulled in automatically.

### Fedora — prebuilt package

```sh
sudo dnf install ./handfast-<version>-1.x86_64.rpm
```

### Any distro — portable binaries

Download `handfast-<version>-x86_64-unknown-linux-gnu.tar.zst`, unpack, and run
`handfastd` / `hfctl` / `handfast-gui` from wherever you like. A `systemd` user unit,
desktop file, and icon are included under `packaging/`.

### From source

Prerequisites: a Rust toolchain (**1.90+**; see [MSRV](#development)) plus the
development headers for Wayland, xkbcommon, and SQLite
(Debian/Ubuntu: `libwayland-dev libxkbcommon-dev libsqlite3-dev`; Arch: `wayland libxkbcommon sqlite`).

```sh
git clone https://github.com/mheci/handfast && cd handfast
cargo build --release --locked --all-features \
  -p handfastd -p handfast-tui -p handfast-gui
```

Binaries land in `target/release/`.

## Getting started

1. Install **KDE Connect** on your Android phone and make sure both devices are on
   the same Wi-Fi network.
2. Start the daemon:

   ```sh
   systemctl --user enable --now handfast.service   # packaged installs
   # or directly:
   ./handfastd &
   ```

3. Pair from either side:

   ```sh
   hfctl devices        # your phone shows up here
   hfctl pair <device-id>
   ```

4. Use it:

   ```sh
   hfctl tui            # full-screen terminal UI
   handfast-gui         # desktop app
   ```

Need help reading logs? `RUST_LOG=debug ./handfastd`.

## CLI reference (`hfctl`)

| Command | What it does |
| --- | --- |
| `hfctl tui` | Interactive terminal UI (default) |
| `hfctl status` | Print daemon identity/ping summary, exit |
| `hfctl devices` | List known devices (id, name, type, paired, state) |
| `hfctl pair <id>` / `hfctl unpair <id>` | Start / revoke pairing |
| `hfctl plugins` | Inspect or toggle per-device plugins |
| `hfctl send <file> <id>` | Send a local file to a device |
| `hfctl transfers` / `transfer-cancel <id>` | Inspect / cancel transfers |
| `hfctl notifications` | Inspect or dismiss mirrored notifications |
| `hfctl clipboard` | Read or overwrite the local clipboard text |
| `hfctl volume` | Read or change the local output volume |
| `hfctl battery <id>` | Ask a device for battery state |
| `hfctl sms <id> <number> <text>` | Send an SMS from the paired phone |
| `hfctl runcommand <id>` | Inspect commands executable on a device |
| `hfctl share-text <id> --text <t>` / `share-url <id> --url <u>` | Push text / open a URL on a device |
| `hfctl logs` | Capture recent daemon log records from the event stream |
| `hfctl completions <shell>` | Emit a shell completion script (bash/zsh/fish/…) |

Run `hfctl <command> --help` for per-command options.

## Configuration

The daemon persists state under XDG dirs:

- **State (SQLite):** `$XDG_DATA_HOME/handfast/state.db` (`~/.local/share/handfast`)
- **Config:** `$XDG_CONFIG_HOME/handfast/` (plugin enable/disable, pairing policy)
- **Cache:** `$XDG_CACHE_HOME/handfast/`
- **IPC socket:** `$XDG_RUNTIME_DIR/handfast` (see `handfast_ipc::default_socket_path`)

Logging follows `RUST_LOG` (`info` default; `debug` for protocol tracing).

## Development

```sh
# Build & test
cargo build --workspace --all-features
cargo test --workspace --all-features --exclude handfast-gui --exclude handfast-ipc
cargo test -p handfast-ipc --all-features -- --test-threads=1   # serial: shared socket
cargo test -p handfast-gui --all-features

# Lint (CI enforces -D warnings)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Fuzzing (nightly toolchain)
cargo +nightly fuzz build -O --sanitizer=none
cargo +nightly fuzz run <target> --sanitizer=none -- -max_total_time=60

# Benchmarks (criterion)
cargo bench -p handfast-protocol

# Idle-resource targets (peak RSS < 40 MB, avg CPU < 1 %)
./scripts/perf.sh

# Release tooling
cargo run -p xtask -- --help
```

**MSRV.** The workspace targets **Rust 1.90** (`rust-version` in `Cargo.toml`), enforced
by the `msrv` CI job against the locked dependency tree. Bumping dependencies that
raise the floor will fail CI, not your users' machines.

## CI / CD

Three GitHub Actions workflows:

- **CI** — fmt, clippy (`-D warnings`), tests (split core/GUI/IPC), RustSec audit,
  cargo-deny, release build, Arch `makepkg` smoke, fuzz smoke, headless Wayland smoke.
  The Arch/fuzz/Wayland lanes are *tolerated* (`continue-on-error`) while they mature.
- **Release** — release-please versioning on `main`; on `v*` tags builds native
  binaries, assembles the Arch package + PKGBUILD natively, and publishes to a
  GitHub Release (AUR sync gated on a secret).
- **Monitor** — self-healing watcher that files `[ci-failure]` tracking issues for
  red runs on `main`; fixes reference the issue via a `Fixes:` trailer.

## Security & privacy

- **No cloud.** Discovery, pairing, and all traffic stay on your LAN.
- **Certificate pinning.** TLS peers are authenticated by pinned certificates
  exchanged during pairing, so a rogue device on the network cannot impersonate a
  paired peer.
- **Fault isolation.** Plugins run behind a supervisor that restarts panicking
  instances; oversized IPC frames and hostile packet lengths are rejected before
  allocation.
- The daemon unit uses `NoNewPrivileges`, `ProtectSystem=strict`, and a restricted
  address family set (see `packaging/systemd/handfast.service`).

## Troubleshooting

- **`hfctl` says "daemon not reachable"** — is `handfastd` running? Check
  `systemctl --user status handfast` or run it in a terminal with `RUST_LOG=debug`.
- **Phone never appears in `hfctl devices`** — same Wi-Fi network? Multicast/broadcast
  allowed by the router? Firewall rules that block **UDP/TCP port 1716** (the KDE
  Connect discovery/data port) will hide devices.
- **Pairs but no notifications** — notification sync needs the desktop D-Bus
  session; start the daemon from your graphical session (not a bare SSH shell).

## Contributing

PRs welcome. Conventional commits; `cargo fmt` + `clippy -D warnings` must stay
clean. Found a bug or want a feature? Open an
[issue](https://github.com/mheci/handfast/issues).

## License

MIT — see [LICENSE-MIT](LICENSE-MIT). Handfast speaks the KDE Connect protocol but
contains no upstream KDE code and is not affiliated with KDE e.V.
