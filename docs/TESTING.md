# TESTING.md — manual test checklist

Execution target: **Phase 4**. All boxes start unchecked; CI covers the
automated subset (`cargo test`, fuzz smoke, headless sway), this file tracks
what only a human with real hardware can verify.

## 1. Pairing flow (Android KDE Connect peer)

Prerequisites: Android device with KDE Connect app, both on same LAN, UDP/TCP
1716 reachable.

- [ ] Daemon starts via `systemctl --user enable --now handfast.service` and
      stays running (`systemctl --user status handfast.service`).
- [ ] Phone appears in `hfctl devices` within one discovery cycle.
- [ ] `hfctl pair <id>` shows a confirmation prompt on the phone.
- [ ] Accepting on the phone flips state to paired in `hfctl devices`.
- [ ] Rejecting on the phone returns a clean error, daemon stays up.
- [ ] Certificate fingerprint is stored per device after first successful
      TLS session; tampering with it breaks the next connect with a clear
      error.
- [ ] Unpair (`hfctl unpair <id>`) removes pairing and the phone confirms
      removal on its side.
- [ ] Re-pair works without restarting either side.

## 2. Per-plugin exercise matrix

One row per roster plugin (17). "Steps" are minimum coverage.

| # | Plugin              | Steps                                                        | Expected                                    | Done |
|---|---------------------|--------------------------------------------------------------|---------------------------------------------|------|
| 1 | battery             | drain/charge phone while paired                               | level + charging update live                | [ ]  |
| 2 | clipboard           | copy text phone -> desktop, then desktop -> phone             | both directions sync                        | [ ]  |
| 3 | connectivity_report | toggle airplane/wifi on phone                                 | state reflected promptly                    | [ ]  |
| 4 | contacts            | request contact list from plugin surface                      | vcards fetched, no duplicates               | [ ]  |
| 5 | findmyphone         | trigger from desktop                                          | phone rings at full volume; repeat cancels  | [ ]  |
| 6 | mousepad            | move pointer, click, scroll, type from phone keyboard         | events land in focused Wayland window       | [ ]  |
| 7 | mpris               | play/pause/seek local player from phone; play phone media     | two-way transport + metadata loop           | [ ]  |
| 8 | notifications       | receive notification, act, reply, dismiss                     | mirrored and actionable both ways           | [ ]  |
| 9 | pause_music         | start call on phone while desktop plays audio                 | desktop player pauses, resumes after        | [ ]  |
| 10| ping                | run ping from hfctl                                           | round-trip latency reported                 | [ ]  |
| 11| run_commands        | define command, list, execute from phone                      | command runs as user, output visible        | [ ]  |
| 12| remote_filesystem   | browse SFTP endpoint of phone                                 | mount/browse works, unmount clean           | [ ]  |
| 13| share               | send url, text, and a >100 MB file each direction             | received intact; progress events stream     | [ ]  |
| 14| sms                 | open conversation, read history, send message                 | threads render; sends succeed               | [ ]  |
| 15| system_volume       | change volume from phone                                      | desktop sink volume follows                 | [ ]  |
| 16| telephony           | incoming call and SMS while paired                            | ring/notification shown on desktop          | [ ]  |
| 17| virtual_input       | exercise virtual input path vs portal-ei path under GNOME     | both paths inject correctly                 | [ ]  |

## 3. Crash-recovery drills

Run each against a paired, active session:

- [ ] `kill -9 $(pidof handfastd)` mid-transfer: unit restarts
      (Restart=on-failure), SQLite WAL recovers, transfer resumable or
      cleanly failed, no orphaned sockets.
- [ ] Network loss (disable wifi) for 60 s then restore: connections drop,
      supervisor backoff cycles, devices rediscover automatically.
- [ ] Suspend/resume the machine: session reconnects within seconds of wake;
      no duplicate device entries.
- [ ] Corrupt `state.db3` (truncate mid-page): daemon reports clear error and
      rebuilds schema without losing the certificate/key in config dir.
- [ ] Delete cache dir contents while stopped: first start regenerates.

## 4. Compositor matrix

For each compositor x capability, verify primary protocol path (and fallback
where listed): input = virtual keyboard/pointer (portal RemoteDesktop under
GNOME), clipboard = ext-data-control (wlr fallback), idle = idle-inhibit
(ScreenSaver D-Bus fallback).

| Capability | Plasma 6 Wayland | GNOME 47+ | Sway 1.9+ | Hyprland |
|------------|------------------|-----------|-----------|----------|
| input      | [ ]              | [ ]       | [ ]       | [ ]      |
| clipboard  | [ ]              | [ ]       | [ ]       | [ ]      |
| idle inhibit | [ ]            | [ ]       | [ ]       | [ ]      |

## 5. IPC client parity (hfctl vs handfast-gui)

Every GUI-visible action must behave identically through the CLI:

- [ ] `daemon_info` matches hello frame in GUI about dialog.
- [ ] Device list ordering/state identical in TUI and GUI.
- [ ] Pair/unpair initiated from either client reflects immediately in both.
- [ ] Plugin toggles from TUI update GUI view without restart.
- [ ] File send from CLI produces the same `transfer_progress` stream as GUI.
- [ ] Notification dismiss parity (list + dismiss from both clients).
- [ ] Clipboard get/set round-trips from both clients.
- [ ] `log_record` events render in TUI log pane and GUI activity view.
