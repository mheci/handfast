# ARCHITECTURE.md — how Handfast is put together

Status: Phase 1 (scaffolding & architecture). Sections marked *Phase 3/4*
describe designed behavior that lands with the plugin and validation phases.

## Overview

```text
                       LAN
   +---------+   UDP/TLS :1716    +------------------------------------+
   |  phone  | <------------------> |             handfastd              |
   | (KDE    |   kdeconnect.* JSON  |  discovery | TLS | pairing | plugins |
   | Connect)|                      +--------+-----------+---------------+
   +---------+                               |           |
                                    UDS      |           | Wayland bridge
                          $XDG_RUNTIME_DIR/  |           v
                          handfast.sock      |     compositor (Plasma/GNOME/
                                 |           |     sway/Hyprland)
               +-----------------+           |
               |  (JSON frames)  |           | D-Bus
               v                 v           v
        +-------------+   +-------------+  MPRIS / notifications /
        | handfast-gui|   |    hfctl    |  org.freedesktop.login1
        | (Iced GUI)  |   | (CLI / TUI) |
        +-------------+   +-------------+
```

One daemon per user session. `handfastd` is the only process that talks to
the network, to Wayland, and to session D-Bus; every front end speaks only
the IPC contract in [IPC.md](IPC.md).

## Crate map

| Crate                | Responsibility                                                        | May depend on                     |
|----------------------|-----------------------------------------------------------------------|-----------------------------------|
| `handfast-core`      | Event bus, XDG paths, SQLite state store, supervision primitives       | nothing in-workspace              |
| `handfast-protocol`  | Packet framing, identity model, TLS certificate pinning                | core-free primitives only (`bytes`, `serde`) |
| `handfast-plugins`   | Plugin trait, registry, metadata for the 17-plugin roster              | `core`                            |
| `handfast-wayland`   | Virtual keyboard/pointer, data-control clipboard, idle-inhibit         | `core`                            |
| `handfast-ipc`       | Length-delimited JSON over Unix socket; typed client/server            | `core`                            |
| `handfast-daemon`    | Wires everything into the supervised `handfastd` binary                | all of the above                  |
| `hfctl`              | CLI/TUI front end                                                      | `ipc` (+ `core` paths)            |
| `handfast-gui`       | Iced desktop application                                               | `ipc` (+ `core` paths)            |
| `xtask`              | Release tooling: dist, completions/man smoke checks                    | dev-only                          |

Rules mirrored from [CONTRIBUTING.md](../CONTRIBUTING.md): `core` depends on
no workspace crate; `protocol` never imports `core`; **only** `daemon` wires
crates together; GUI and TUI communicate with the daemon exclusively through
`ipc`. Linux-only code sits behind `#[cfg(target_os = "linux")]` with
compiling shims so Windows/macOS builds still typecheck.

## Supervision tree

`handfast-core::supervise::Supervisor` spawns named tasks from restartable
factories:

- The supervisor spawns one task each for **discovery**, **connections**
  (per-peer TLS sessions), and **plugins**.
- A child that returns `Err` or panics triggers a respawn after exponential
  backoff: starting at **100 ms**, doubling per consecutive failure, capped
  at **30 s**, reset to 100 ms once a child stays alive for a healthy window
  of 30 s.
- Panics are isolated: children are plain tokio tasks whose `JoinError`
  surfaces the panic to the watcher; the daemon process itself never dies
  with its tasks.
- Every crash/restart is published as a `LogRecord` event on the core bus,
  which the IPC layer forwards verbatim to connected clients (`hfctl logs`,
  the TUI status line, and the GUI activity view).

## Persistence model

All persistent writes go through `handfast_core::store::atomic_write`
(temp file -> fsync -> rename), per the ground rules in CONTRIBUTING.

| What                    | Where                                   | Notes                                  |
|-------------------------|-----------------------------------------|----------------------------------------|
| Paired devices, plugin toggles, transfers history | data dir `~/.local/state/handfast/state.db3` | SQLite in WAL mode; survives crashes mid-write |
| Device certificate + private key | config dir `~/.config/handfast/` | mode `0600`; generated on first run    |
| User configuration      | `~/.config/handfast/config.toml`        | written atomically                     |
| Regenerable caches      | `~/.cache/handfast/`                    | safe to delete while stopped           |

The database is opened with WAL journaling so a crash during a write leaves
the last committed transaction intact. Nothing else in the tree is written
non-atomically.

## Wayland-first

Session detection precedence (pure environment inspection, no compositor
round-trip):

1. Direct socket hints: `WAYLAND_DISPLAY` / `WAYLAND_SOCKET`,
   `SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`.
2. `XDG_SESSION_TYPE` (`wayland` > `x11` > `tty`).
3. Desktop hints: `XDG_CURRENT_DESKTOP`, `DESKTOP_SESSION`.
4. Fallback: `Unknown`; the daemon still runs (network + plugins without
   desktop integration) and reports the degraded state via IPC.

Capability matrix — preferred protocol first, fallback second:

| Function          | Primary                                        | Fallback                                        |
|-------------------|------------------------------------------------|--------------------------------------------------|
| Keyboard input    | `zwp_virtual_keyboard_manager_v1`              | portal RemoteDesktop (portal-ei)                 |
| Pointer input     | `zwlr_virtual_pointer_manager_v1`              | portal RemoteDesktop (portal-ei) — preferred under GNOME |
| Clipboard watch/set | `ext-data-control-v1`                        | `wlr-data-control-unstable-v1`                   |
| Idle inhibit      | `zwp_idle_inhibit_unstable_v1`                 | `org.freedesktop.ScreenSaver` D-Bus              |

There is no X11/XWayland/XTEST backend anywhere in the tree, by rule 3 of
CONTRIBUTING.

Tested-compositor targets (validation executes in Phase 4):

| Compositor        | Version floor | Status            |
|-------------------|---------------|-------------------|
| KDE Plasma        | Plasma 6 Wayland | pending Phase-4 |
| GNOME             | 47+           | pending Phase-4 (RemoteDesktop path) |
| Sway              | 1.9+          | pending Phase-4   |
| Hyprland          | current stable | pending Phase-4  |

## IPC layering

Clients (`handfast-gui`, `hfctl`) connect to `$XDG_RUNTIME_DIR/handfast.sock`
and exchange length-delimited JSON: requests/responses plus a pushed event
stream. The full contract — framing, method table, error codes, event
catalogue, versioning policy, and client rules — is specified in
[IPC.md](IPC.md). That document is authoritative; this section intentionally
stays short.

## Performance budget

Held from Phase 1 onward, measured in CI benchmarks and re-verified in
Phase 4:

| Metric                        | Budget        | How it is achieved                              |
|-------------------------------|---------------|-------------------------------------------------|
| Idle RSS                      | < 40 MB       | no polling buffers, bounded queues, compact state |
| Idle CPU                      | < 1 %         | fully event-driven design                       |
| Wakeups while idle            | near zero     | readiness-based I/O; timers only where needed   |

Event-driven guarantees (CONTRIBUTING ground rule 4): no polling loops
anywhere. Discovery uses socket readiness, clipboard uses Wayland data-control
events, device state changes ride the bus, and any unavoidable poll would need
a written justification in its PR.

Allocators: `jemalloc` on `linux-gnu` targets, `mimalloc` elsewhere,
feature-gated in `crates/daemon`; both chosen for fragmentation resistance
under long-running transfer workloads.
