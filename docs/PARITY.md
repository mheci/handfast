# Parity matrix

Target: the 31 plugins shipped by `kdeconnect-kde` on a Linux desktop, using the
official KDE Connect Android/iOS apps over protocol version 8.

Status legend:

- **contract**: the v2 plugin implements the packet schema and trait contract.
- **backend**: the desktop adapter for the plugin exists and is tested.
- **e2e**: the plugin has been exercised against a real KDE Connect client.

Phases: **P0** foundation, **P1** link and core transfer, **P2** desktop sync,
**P3** input and files, **P4** display and advanced.

| # | Plugin | Direction | Wire packets | Adapter | Phase | Contract | Backend | E2E |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | `battery` | bidirectional | `kdeconnect.battery`, `.battery.request` | sysfs; `Win32_Battery` on Windows | P0 | yes | yes | no |
| 2 | `clipboard` | bidirectional | `kdeconnect.clipboard`, `.clipboard.connect` | `wl-clipboard` on Wayland, `xclip` on X11, `clipboard-win` on Windows | P2 | yes | yes | no |
| 3 | `connectivity-report` | phone -> desktop | `kdeconnect.connectivity_report`, `.connectivity_report.request` | none | P2 | yes | n/a | no |
| 4 | `contacts` | phone -> desktop | `kdeconnect.contacts.request_all_uids_timestamps`, `.request_vcards_by_uid`, `.response_uids_timestamps`, `.response_vcards` | vCard model | P2 | yes | n/a | no |
| 5 | `digitizer` | phone -> desktop | `kdeconnect.digitizer`, `.digitizer.session` | `evdev` uinput tablet on Wayland; `enigo` absolute pointer on Windows (no pressure) | P4 | yes | yes | no |
| 6 | `findmyphone` | desktop -> phone | `kdeconnect.findmyphone.request` | none | P2 | yes | n/a | no |
| 7 | `findthisdevice` | phone -> desktop | `kdeconnect.findmyphone.request`, `.private.findthisdevice` | `notify-send` | P2 | yes | yes | no |
| 8 | `lockdevice` | phone -> desktop | `kdeconnect.lock`, `.lock.request` | `loginctl` | P2 | yes | yes | no |
| 9 | `mmtelephony` | ModemManager -> desktop | `kdeconnect.telephony`, `.telephony.request_mute` | ModemManager DBus | P4 | no | no | no |
| 10 | `mousepad` | phone -> desktop | `kdeconnect.mousepad.request`, `.echo`, `.keyboardstate` | `ydotool` and `wtype` on Wayland; `enigo` on Windows | P3 | yes | yes | no |
| 11 | `mpriscontrol` | bidirectional | `kdeconnect.mpris`, `.mpris.request` | `playerctl` | P2 | yes | yes | no |
| 12 | `mprisremote` | bidirectional | `kdeconnect.mpris`, `.mpris.request` | peer player model | P2 | yes | n/a | no |
| 13 | `notifications` | phone -> desktop | `kdeconnect.notification`, `.notification.action`, `.notification.reply`, `.notification.request` | `notify-send`, `gdbus` close | P2 | yes | yes | no |
| 14 | `pausemusic` | phone -> desktop | `kdeconnect.telephony`, `kdeconnect.mpris.request` | `playerctl` | P2 | yes | yes | no |
| 15 | `ping` | bidirectional | `kdeconnect.ping` | none | P0 | yes | n/a | no |
| 16 | `presenter` | phone -> desktop | `kdeconnect.presenter` | `ydotool` and `wtype` on Wayland; `enigo` on Windows | P3 | yes | yes | no |
| 17 | `remotecommands` | bidirectional | `kdeconnect.runcommand`, `.runcommand.request` | peer command model | P3 | yes | n/a | no |
| 18 | `remotecontrol` | desktop -> phone | `kdeconnect.mousepad.request` | none | P3 | yes | n/a | no |
| 19 | `remotekeyboard` | desktop -> phone | `kdeconnect.mousepad.request` | none | P3 | yes | n/a | no |
| 20 | `remotesystemvolume` | bidirectional | `kdeconnect.systemvolume`, `.systemvolume.request` | peer sink model | P2 | yes | n/a | no |
| 21 | `runcommand` | bidirectional | `kdeconnect.runcommand`, `.runcommand.request`, `.runcommand.output` | configured command runner | P3 | yes | yes | no |
| 22 | `screensaver-inhibit` | connection state | none (uses link state) | `systemd-inhibit` | P2 | yes | yes | no |
| 23 | `sendnotifications` | desktop -> phone | `kdeconnect.notification`, `.notification.request` | freedesktop notifications DBus | P2 | no | no | no |
| 24 | `sftp` | phone -> desktop | `kdeconnect.sftp`, `.sftp.request` | `russh` SFTP client | P3 | yes | yes | no |
| 25 | `share` | bidirectional | `kdeconnect.share.request`, `.share.request.update` | TLS payload channel, `LocalFiles`, `xdg-open` for URLs | P1 | yes | yes | no |
| 26 | `shareinputdevices` | desktop -> remote | `kdeconnect.shareinputdevices`, `.shareinputdevices.request`, `kdeconnect.mousepad.request` | libei input capture + forwarding | P4 | no | no | no |
| 27 | `shareinputdevicesremote` | remote -> desktop | `kdeconnect.shareinputdevices`, `.shareinputdevices.request`, `kdeconnect.mousepad.request` | `enigo` absolute pointer on Windows; declines shares on Wayland | P4 | yes | yes | no |
| 28 | `sms` | bidirectional | `kdeconnect.sms.messages`, `.sms.request`, `.sms.request_conversation`, `.sms.request_conversations`, `.sms.request_attachment`, `.sms.attachment_file` | phone-owned message cache | P2 | yes | n/a | no |
| 29 | `systemvolume` | bidirectional | `kdeconnect.systemvolume`, `.systemvolume.request` | `wpctl` | P2 | yes | yes | no |
| 30 | `telephony` | phone -> desktop | `kdeconnect.telephony`, `.telephony.request_mute` | `notify-send`; toast on Windows | P2 | yes | yes | no |
| 31 | `virtualmonitor` | phone -> desktop | `kdeconnect.virtualmonitor`, `.virtualmonitor.request` | Wayland virtual-output protocol | P4 | no | no | no |

## Platform notes

KDE Connect's own matrix is asymmetric. The Android app does not implement
virtual monitors, and a few plugins are one-way. v2 matches the desktop half of
each plugin and does not claim Android-side features the official app does not
have.

- **Run command**: the desktop and Android each keep their own command list.
  `runcommand` executes local commands; `remotecommands` triggers the peer's
  list. Commands are argv arrays with no shell, configured locally with
  `handfast command add --name NAME -- PROGRAM ARG...` and capped at 64 entries.
  Each run has a 30 s timeout and streams stdout and stderr back through
  `kdeconnect.runcommand.output`.
- **Remote commands**: the desktop keeps the command list a peer reports and
  sends list, run, setup and stop requests. Commands always execute on the
  peer; the desktop never runs them locally. Command output
  (`.runcommand.output`) is not fetched yet.
- **SMS and contacts**: the phone owns the data. The desktop stores, displays and
  sends requests. The contacts plugin keeps the parsed vCard list in memory for
  each connected device and clears it on disconnect; `handfast contacts` lists
  it and `--refresh` asks the phone for a fresh sync. The SMS plugin caches
  messages per device, derives conversations from the latest message in each
  thread, and sends plain-text messages with `handfast sms`; attachments and
  MMS are not implemented.
- **Virtual monitor**: desktop-only; the official Android client has no virtual
  display support.
- **Clipboard images**: the wire protocol carries text. File clipboard entries
  use `kdeconnect.clipboard.file` and are a v2 extension target, not a v1
  requirement.
- **Telephony**: the desktop shows incoming, missed and active call
  notifications and closes them on cancel. `handfast telephony <device-id>
  --mute` sends `kdeconnect.telephony.request_mute`, which the Android app
  handles by muting its ringer.
- **Remote system volume**: the desktop keeps the audio sinks a peer reports and
  sends volume and mute requests. The official Android app's SystemVolume plugin
  is a controller, not a sink provider, so this plugin is exercised between two
  desktops until a peer app provides sinks.
- **Remote media**: the desktop keeps the players a peer reports and sends
  transport, volume and seek requests. Android provides phone media through
  `MprisReceiverPlugin`, so the phone's media appears on the desktop. Album art
  URLs are modelled, but album art payloads are not fetched yet.
- **Seek units**: `Seek` is microseconds and `SetPosition` is milliseconds, as
  the protocol specifies. Android's `MprisReceiverPlugin` applies `SetPosition`,
  `setVolume` and actions but not `Seek`, so relative seek works with desktop
  peers until the Android app adds it.

## Windows 11 backends

The Windows daemon installs every implemented adapter that an in-box tool or a
safe crate can provide. It never resolves a peer-driven executable through
`PATH`; each tool is an absolute path under `%SystemRoot%\System32`.

| Capability | Windows path | Notes |
| --- | --- | --- |
| Clipboard | `clipboard-win` | In-process text read and write. |
| Notifications | toast through `powershell.exe` | XML text is escaped; close removes the toast by tag. |
| Media | SMTC through `powershell.exe` | Play, pause, toggle, stop, next, previous, volume, and now playing. Position is reported when the source publishes it. |
| Volume | `volumecontrol-windows` | WASAPI default output; percent and mute. |
| Screensaver | `keepawake` | `SetThreadExecutionState` on a dedicated thread. |
| Lock | `rundll32.exe user32.dll,LockWorkStation` | Best effort; `rundll32` reports its own status. |
| Shell | `explorer.exe` | Only `http` and `https` URLs are opened. |
| Files | `LocalFiles` | The state and incoming directories carry a protected DACL. |

Gaps, named rather than hidden:

- **Input**: relative pointer motion, buttons and keys are available through
  `enigo` on Windows and `ydotool` plus `wtype` on Wayland. The `digitizer`
  drawing tablet injects absolute position on both: Wayland opens a virtual
  tablet through the safe `evdev` uinput API and gets pen pressure for free,
  while Windows moves the real pointer through `enigo` and maps the eraser to
  the right button. Windows pressure is the named gap: the Windows pen
  injection APIs need unsafe code or a wrapper crate the workspace does not
  have, so pressure is ignored there and never faked. Input capture for
  `shareinputdevices` is still missing.
- **Remote input**: `handfast remote-input` sends pointer moves, scrolls,
  clicks and keys to a phone over `kdeconnect.mousepad.request`. The phone
  injects the input; nothing is injected locally. Special key codes are 1-32,
  matching the codes the desktop `mousepad` plugin accepts.
- **Shared input receive**: `shareinputdevicesremote` moves the pointer to the
  entry edge with an absolute `enigo` move on Windows. Wayland has no portable
  absolute pointer. `ydotool` cannot move to absolute coordinates, and libei
  needs a portal consent and a new dependency. On Wayland the plugin still
  advertises its capability, so a peer can start a share. The plugin declines
  that share with a zero-offset release, so the peer gets its pointer back.
  Only the main display is used, so a multi-monitor layout enters at a corner
  of the primary display.
- **SFTP**: the phone runs the SFTP server. `handfast sftp <device-id> --start`
  sends `kdeconnect.sftp.request`, and the phone replies with a one-session
  password plus the port, user, root and its reported IP. `--list`,
  `--download` and `--upload` connect with the `russh` client and copy through
  the phone's server. The phone's server enforces its own storage roots.
- **Player volume read**: SMTC exposes `TryChangeVolumeAsync` but no per-session
  volume getter, so `PlaybackState.volume` is `None` on Windows.
- **Clipboard push**: the session polls every 2 s because the clipboard trait has
  no change notification. The selection is capped at 256 KiB, and an inbound
  write is not echoed back.
- **Toast identity**: the toast uses the `Handfast` AUMID. A Start Menu shortcut
  carrying `System.AppUserModel.ID=Handfast` is the packaging follow-up if a
  Windows build drops the toast.
- **Remaining planned plugins**: `sendnotifications` needs a local notification
  source; the Linux DBus monitor is not implemented. The Windows
  `UserNotificationListener` polling path works unpackaged in a live probe
  (status `Allowed`, one toast counted, text readable), but it needs a safe
  wrapper and a one-time consent, so it stays out until that is tested.
  `virtualmonitor` needs a virtual display driver; the Linux `krdpserver`/RDP
  client half is not implemented. The `shareinputdevices` capture pair needs
  libei on Wayland and a hook backend on Windows. `mmtelephony` needs
  ModemManager and has no Android counterpart.

## Test strategy

Every plugin must ship with:

1. Golden wire vectors under `crates/handfast-testkit/tests/vectors/`.
2. A contract test with `MockDesktop` if it touches the desktop.
3. A conformance test over the Rust peer for its packet flow.
4. A real-device checklist entry before `e2e` is marked yes.

The roster test in `handfast-plugins` fails if a plugin is added, removed or
renamed without updating this table's source of truth.
