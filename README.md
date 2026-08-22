# Handfast

**Handfast** binds your phone and your Linux desktop together — a standalone,
Wayland-first, KDE Connect-compatible connectivity daemon written in 100% Rust.

> Handfasting: the ritual act of binding two hands together. Pairing two
> devices is exactly that — a fast knot you can untie whenever you choose.

## Status: Phase 1 (scaffolding & architecture)

This repository currently contains the Phase 1 skeleton: workspace layout,
daemon supervision core, IPC contract, Wayland bridge surface, Iced GUI shell,
TUI/CLI shell, CI/CD, monitoring, and Arch packaging. Protocol implementation
lands in Phase 2; plugin implementations in Phase 3.

## Components

| Component | Crate / Binary | Description |
|---|---|---|
| Daemon | `handfastd` (`crates/daemon`) | Supervised tokio service; discovery, pairing, TLS, plugins |
| Core library | `handfast-core` | Event bus, XDG paths, SQLite state store, supervision primitives |
| Protocol library | `handfast-protocol` | Zero-copy packet framing, identity model, TLS certificate pinning |
| Plugins | `handfast-plugins` | Plugin trait + registry (battery, clipboard, input, MPRIS, ...) |
| Wayland bridge | `handfast-wayland` | Virtual keyboard/pointer (`zwp_virtual_*`), data-control clipboard, idle-inhibit |
| IPC | `handfast-ipc` | Length-delimited JSON over Unix domain socket + typed client |
| GUI | `handfast-gui` | **Iced** application, Elm-style update loop, Wayland-native via winit |
| CLI/TUI | `hfctl` | systemctl-style control CLI; `hfctl tui` opens the ratatui interface |

## Design pillars

- **Wayland-first.** Remote input via `zwp_virtual_keyboard_v1` /
  `zwp_virtual_pointer_v1` (libei / RemoteDesktop portal preferred where
  available), clipboard via `ext-data-control` / `wlr-data-control`, screensaver
  inhibition via `idle-inhibit-unstable-v1`. No X11, no XWayland, no XTEST.
- **Supervised to the task level.** Any panicking connection handler or plugin
  is restarted with exponential backoff; the daemon never dies with its tasks.
- **Event-driven only.** Readiness-based sockets and timers; zero polling loops.
- **Atomic state.** Config/state writes are temp-file → fsync → rename;
  device pairing lives in SQLite and survives crashes.

See [`docs/`](docs/) for the full picture:
[NAMING](docs/NAMING.md) · [ARCHITECTURE](docs/ARCHITECTURE.md) ·
[PROTOCOL](docs/PROTOCOL.md) · [IPC](docs/IPC.md) · [TESTING](docs/TESTING.md)

## Building

```sh
cargo build --release --all-features      # binaries in target/release/
cargo test --all-features
```

Headless builds skip the GUI:

```sh
cargo build --release -p handfastd -p hfctl
```

## Running (Arch Linux)

```sh
# from AUR once published, or from packaging/arch:
systemctl --user enable --now handfast.service
hfctl devices          # list discovered devices
hfctl pair <device-id>
hfctl tui              # full terminal UI
handfast-gui           # Iced desktop app
```

## License

MIT — see [LICENSE-MIT](LICENSE-MIT). Handfast is an independent clean-room
implementation speaking the KDE Connect protocol; it contains no upstream code
and is not affiliated with KDE e.V.
