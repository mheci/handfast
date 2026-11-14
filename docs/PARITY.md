# Parity matrix

Target: the 31 plugins shipped by `kdeconnect-kde` on a Linux desktop, using the
official KDE Connect Android/iOS apps over protocol version 8.

Status legend:

- **contract**: the v2 plugin implements the packet schema and trait contract.
- **backend**: the Linux desktop adapter for the plugin exists and is tested.
- **e2e**: the plugin has been exercised against a real KDE Connect client.

Phases: **P0** foundation, **P1** link and core transfer, **P2** desktop sync,
**P3** input and files, **P4** display and advanced.

| # | Plugin | Direction | Wire packets | Linux backend | Phase | Contract | Backend | E2E |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | `battery` | phone -> desktop | `kdeconnect.battery` | none | P0 | yes | n/a | no |
| 2 | `clipboard` | phone -> desktop | `kdeconnect.clipboard`, `.clipboard.connect` | `wl-clipboard` on Wayland, `xclip` on X11 | P2 | yes | yes | no |
| 3 | `connectivity-report` | phone -> desktop | `kdeconnect.connectivity_report`, `.connectivity_report.request` | none | P2 | yes | n/a | no |
| 4 | `contacts` | phone -> desktop | `kdeconnect.contacts.request_all_uids_timestamps`, `.request_vcards_by_uid`, `.response_uids_timestamps`, `.response_vcards` | vCard store | P2 | no | no | no |
| 5 | `digitizer` | phone -> desktop | `kdeconnect.digitizer`, `.digitizer.session` | libei absolute pointer with pressure | P4 | no | no | no |
| 6 | `findmyphone` | desktop -> phone | `kdeconnect.findmyphone.request` | none | P2 | no | n/a | no |
| 7 | `findthisdevice` | phone -> desktop | `kdeconnect.findmyphone.request`, `.private.findthisdevice` | `notify-send` | P2 | yes | yes | no |
| 8 | `lockdevice` | phone -> desktop | `kdeconnect.lock`, `.lock.request` | logind / DBus screensaver | P2 | no | no | no |
| 9 | `mmtelephony` | ModemManager -> desktop | `kdeconnect.telephony`, `.telephony.request_mute` | ModemManager DBus | P4 | no | no | no |
| 10 | `mousepad` | phone -> desktop | `kdeconnect.mousepad.request`, `.echo`, `.keyboardstate` | libei pointer + keyboard | P3 | no | no | no |
| 11 | `mpriscontrol` | phone -> desktop | `kdeconnect.mpris`, `.mpris.request` | `playerctl` | P2 | yes | yes | no |
| 12 | `mprisremote` | desktop -> phone | `kdeconnect.mpris`, `.mpris.request` | MPRIS DBus (remote players) | P2 | no | no | no |
| 13 | `notifications` | phone -> desktop | `kdeconnect.notification`, `.notification.action`, `.notification.reply`, `.notification.request` | `notify-send`, `gdbus` close | P2 | yes | yes | no |
| 14 | `pausemusic` | phone -> desktop | `kdeconnect.telephony`, `kdeconnect.mpris.request` | `playerctl` | P2 | yes | yes | no |
| 15 | `ping` | bidirectional | `kdeconnect.ping` | none | P0 | yes | n/a | no |
| 16 | `presenter` | phone -> desktop | `kdeconnect.presenter` | libei pointer injection | P3 | yes | no | no |
| 17 | `remotecommands` | phone -> desktop | `kdeconnect.runcommand`, `.runcommand.request`, `.runcommand.output` | configured command runner | P3 | no | no | no |
| 18 | `remotecontrol` | desktop -> phone | `kdeconnect.mousepad.request` | none | P3 | no | n/a | no |
| 19 | `remotekeyboard` | desktop -> phone | `kdeconnect.mousepad.request` | none | P3 | no | n/a | no |
| 20 | `remotesystemvolume` | desktop -> phone | `kdeconnect.systemvolume`, `.systemvolume.request` | PipeWire or wpctl | P2 | no | no | no |
| 21 | `runcommand` | bidirectional | `kdeconnect.runcommand`, `.runcommand.request`, `.runcommand.output` | configured command runner | P3 | no | no | no |
| 22 | `screensaver-inhibit` | connection state | none (uses link state) | `systemd-inhibit` | P2 | yes | yes | no |
| 23 | `sendnotifications` | desktop -> phone | `kdeconnect.notification`, `.notification.request` | freedesktop notifications DBus | P2 | no | no | no |
| 24 | `sftp` | phone -> desktop | `kdeconnect.sftp`, `.sftp.request` | embedded Rust SFTP server | P3 | no | no | no |
| 25 | `share` | bidirectional | `kdeconnect.share.request`, `.share.request.update` | TLS payload channel, `LocalFiles`, `xdg-open` for URLs | P1 | yes | yes | no |
| 26 | `shareinputdevices` | desktop -> remote | `kdeconnect.shareinputdevices`, `.shareinputdevices.request`, `kdeconnect.mousepad.request` | libei input capture + forwarding | P4 | no | no | no |
| 27 | `shareinputdevicesremote` | remote -> desktop | `kdeconnect.shareinputdevices`, `.shareinputdevices.request`, `kdeconnect.mousepad.request` | libei input injection | P4 | no | no | no |
| 28 | `sms` | bidirectional | `kdeconnect.sms.messages`, `.sms.request`, `.sms.request_conversation`, `.sms.request_conversations`, `.sms.request_attachment`, `.sms.attachment_file` | phone-owned data, desktop UI | P2 | no | no | no |
| 29 | `systemvolume` | phone -> desktop | `kdeconnect.systemvolume`, `.systemvolume.request` | `wpctl` | P2 | yes | yes | no |
| 30 | `telephony` | phone -> desktop | `kdeconnect.telephony`, `.telephony.request_mute` | DBus notifications | P2 | no | no | no |
| 31 | `virtualmonitor` | phone -> desktop | `kdeconnect.virtualmonitor`, `.virtualmonitor.request` | Wayland virtual-output protocol | P4 | no | no | no |

## Platform notes

KDE Connect's own matrix is asymmetric. The Android app does not implement
virtual monitors, and a few plugins are one-way. v2 matches the desktop half of
each plugin and does not claim Android-side features the official app does not
have.

- **Run command**: Android can trigger desktop commands, but the desktop cannot
  trigger Android commands.
- **SMS and contacts**: the phone owns the data. The desktop stores, displays and
  sends requests.
- **Virtual monitor**: desktop-only; the official Android client has no virtual
  display support.
- **Clipboard images**: the wire protocol carries text. File clipboard entries
  use `kdeconnect.clipboard.file` and are a v2 extension target, not a v1
  requirement.

## Test strategy

Every plugin must ship with:

1. Golden wire vectors under `crates/handfast-testkit/tests/vectors/`.
2. A contract test with `MockDesktop` if it touches the desktop.
3. A conformance test over the Rust peer for its packet flow.
4. A real-device checklist entry before `e2e` is marked yes.

The roster test in `handfast-plugins` fails if a plugin is added, removed or
renamed without updating this table's source of truth.
