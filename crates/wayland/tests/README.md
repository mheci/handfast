# Compositor validation tests (`tests/validation.rs`)

Phase-4 integration tests that exercise the Wayland bridge against a **live
compositor**. Without a compositor in the environment every test returns
early and passes (skip), so `cargo test` stays green on Windows/macOS/plain
CI runners.

| Test | Needs a live compositor? | Notes |
|---|---|---|
| `session_detect_on_headless_weston` | yes (`WAYLAND_DISPLAY`) | asserts `SessionKind::Wayland`; protocol-table assertion is skipped when no compositor hint (`SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`, `XDG_CURRENT_DESKTOP`, …) is visible |
| `clipboard_roundtrip` | yes | requires data-control support: sway/hyprland/plasma/GNOME 48+; **stock weston fails here** |
| `idle_inhibit_acquire_release` | yes | requires `zwp_idle_inhibit_manager_v1` (wlroots family) or the `dbus-idle` feature + session bus; **stock weston fails here** |
| `virtual_input_connect_rejects_gracefully` | **no** — needs the *absence* of one | runs on any OS, incl. Windows shim |

## Running

```bash
cargo test -p handfast-wayland --test validation
```

Single test with output:

```bash
cargo test -p handfast-wayland --test validation clipboard_roundtrip -- --nocapture
```

The tests read `WAYLAND_DISPLAY` from their own process environment. Launch
the headless compositor first, discover its socket, export it, then run
cargo from the same shell.

### Headless sway (wlroots) — full pass expected

```bash
sudo apt install sway pixman   # or: sudo pacman -S sway
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
mkdir -p "$XDG_RUNTIME_DIR"

sway --headless --unsupported-gpu >/tmp/sway.log 2>&1 &

# Wait for the sockets sway creates in $XDG_RUNTIME_DIR.
for _ in $(seq 100); do
  WAYLAND_DISPLAY="$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f' 2>/dev/null | sort | tail -n1)"
  SWAYSOCK="$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*' -printf '%p' 2>/dev/null | tail -n1)"
  [ -n "$WAYLAND_DISPLAY" ] && [ -n "$SWAYSOCK" ] && break
  sleep 0.1
done
export WAYLAND_DISPLAY SWAYSOCK

cargo test -p handfast-wayland --test validation --all-features
```

`SWAYSOCK` also makes `session_detect_on_headless_weston` exercise the
non-empty protocol-table assertion.

### Headless weston

```bash
sudo apt install weston
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
mkdir -p "$XDG_RUNTIME_DIR"

weston --backend=headless-backend.so --socket=wayland-handfast --idle-time=0 \
  >/tmp/weston.log 2>&1 &
sleep 1
export WAYLAND_DISPLAY=wayland-handfast

cargo test -p handfast-wayland --test validation
```

Weston advertises none of the bridge's protocols (no virtual-input managers,
no data-control, no idle-inhibit), so only `session_detect_*` validates
meaningfully here; the other live-compositor tests fail by design until run
against a supported compositor.

### Headless hyprland

```bash
sudo pacman -S hyprland   # or your distro's package
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
mkdir -p "$XDG_RUNTIME_DIR"

AQ_DRM_DEVICES= HYPRLAND_NO_SD_NOTIFY=1 Hyprland >/tmp/hyprland.log 2>&1 &

for _ in $(seq 100); do
  HYPRLAND_INSTANCE_SIGNATURE="$(ls -t "$XDG_RUNTIME_DIR/hypr" 2>/dev/null | head -n1)"
  WAYLAND_DISPLAY="$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f' 2>/dev/null | sort | tail -n1)"
  [ -n "$HYPRLAND_INSTANCE_SIGNATURE" ] && [ -n "$WAYLAND_DISPLAY" ] && break
  sleep 0.1
done
export WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE

cargo test -p handfast-wayland --test validation
```

`HYPRLAND_INSTANCE_SIGNATURE` feeds the compositor heuristic in `detect.rs`.

## Troubleshooting

- **Tests all "pass" instantly** — no `WAYLAND_DISPLAY` was exported; check
  the socket-discovery loop output before `cargo test`.
- **`XKB ... could not resolve keysym` / keymap compile errors** — install
  `xkeyboard-config` and `libxkbcommon` (the virtual keyboard path compiles a
  US-qwerty keymap).
- **Clipboard round trip times out** — wl-clipboard-rs serves paste requests
  from a background thread; ensure nothing else (e.g. `wl-copy` daemons)
  overwrites the selection between `set_text` and `get_text`.
- **Idle test fails on GNOME/KDE** — those sessions need the `dbus-idle`
  feature and a session bus:
  `cargo test -p handfast-wayland --test validation --features dbus-idle`.
- **No seat advertised warnings** — harmless for these tests; virtual input
  devices do not require seat capabilities.
