# Parity matrix

Target: the 31 plugins shipped by `kdeconnect-kde` on a Linux desktop, using the
official KDE Connect Android/iOS apps over protocol version 8.

Every plugin in the roster is now implemented, so the `Contract` column is `yes`
throughout. `Backend` is `n/a` where the plugin owns no desktop adapter.

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
| 4 | `contacts` | phone requests, desktop serves | `kdeconnect.contacts.request_all_uids_timestamps`, `.request_vcards_by_uid`, `.response_uids_timestamps`, `.response_vcards` | vCard directory | P2 | yes | yes | no |
| 5 | `digitizer` | phone -> desktop | `kdeconnect.digitizer`, `.digitizer.session` | `xdotool` absolute pointer | P4 | yes | yes | no |
| 6 | `findmyphone` | desktop -> phone | `kdeconnect.findmyphone.request` | none | P2 | yes | n/a | no |
| 7 | `findthisdevice` | phone -> desktop | `kdeconnect.findmyphone.request`, `.private.findthisdevice` | `notify-send` | P2 | yes | yes | no |
| 8 | `lockdevice` | phone -> desktop | `kdeconnect.lock`, `.lock.request` | `loginctl` | P2 | yes | yes | no |
| 9 | `mmtelephony` | desktop -> phone | `kdeconnect.telephony`, `.telephony.request_mute` | `mmcli` | P4 | yes | yes | no |
| 10 | `mousepad` | phone -> desktop | `kdeconnect.mousepad.request`, `.echo`, `.keyboardstate` | `xdotool` pointer + keyboard | P3 | yes | yes | no |
| 11 | `mpriscontrol` | phone -> desktop | `kdeconnect.mpris`, `.mpris.request` | `playerctl` | P2 | yes | yes | no |
| 12 | `mprisremote` | desktop -> phone | `kdeconnect.mpris`, `.mpris.request` | phone-owned; cached player state, no local adapter | P2 | yes | n/a | no |
| 13 | `notifications` | phone -> desktop | `kdeconnect.notification`, `.notification.action`, `.notification.reply`, `.notification.request` | `notify-send`, `gdbus` close | P2 | yes | yes | no |
| 14 | `pausemusic` | phone -> desktop | `kdeconnect.telephony`, `kdeconnect.mpris.request` | `playerctl` | P2 | yes | yes | no |
| 15 | `ping` | bidirectional | `kdeconnect.ping` | none | P0 | yes | n/a | no |
| 16 | `presenter` | phone -> desktop | `kdeconnect.presenter` | in-process presentation view | P3 | yes | yes | no |
| 17 | `remotecommands` | desktop -> phone | `kdeconnect.runcommand`, `.runcommand.request`, `.runcommand.output` | none | P3 | yes | n/a | no |
| 18 | `remotecontrol` | desktop -> phone | `kdeconnect.mousepad.request` | none | P3 | yes | n/a | no |
| 19 | `remotekeyboard` | desktop -> phone | `kdeconnect.mousepad.request`, `.mousepad.echo`, `.mousepad.keyboardstate` | none | P3 | yes | n/a | no |
| 20 | `remotesystemvolume` | desktop -> phone | `kdeconnect.systemvolume`, `.systemvolume.request` | none | P2 | yes | n/a | no |
| 21 | `runcommand` | phone -> desktop | `kdeconnect.runcommand`, `.runcommand.request`, `.runcommand.output` | allowlisted `plugins.toml` commands, executed without a shell | P3 | yes | yes | no |
| 22 | `screensaver-inhibit` | connection state | none (uses link state) | `systemd-inhibit` | P2 | yes | yes | no |
| 23 | `sendnotifications` | desktop -> phone | `kdeconnect.notification`, `.notification.request` | `notify-send` mirror, gated by an app allowlist | P2 | yes | yes | no |
| 24 | `sftp` | phone -> desktop | `kdeconnect.sftp`, `.sftp.request` | `sshd`, chrooted to one configured root on loopback | P3 | yes | yes | no |
| 25 | `share` | bidirectional | `kdeconnect.share.request`, `.share.request.update` | TLS payload channel, `LocalFiles`, `xdg-open` for URLs | P1 | yes | yes | no |
| 26 | `shareinputdevices` | phone -> desktop | `kdeconnect.shareinputdevices`, `.shareinputdevices.request`, `kdeconnect.mousepad.request` | `xdotool`, gated on an explicit share request | P4 | yes | yes | no |
| 27 | `shareinputdevicesremote` | desktop -> phone | `kdeconnect.shareinputdevices`, `.shareinputdevices.request`, `kdeconnect.mousepad.request` | none | P4 | yes | n/a | no |
| 28 | `sms` | desktop -> phone | `kdeconnect.sms.messages`, `.sms.request`, `.sms.request_conversation`, `.sms.request_conversations`, `.sms.request_attachment`, `.sms.attachment_file` | none; the phone owns the store | P2 | yes | n/a | no |
| 29 | `systemvolume` | phone -> desktop | `kdeconnect.systemvolume`, `.systemvolume.request` | `wpctl` | P2 | yes | yes | no |
| 30 | `telephony` | phone -> desktop | `kdeconnect.telephony` | `notify-send` | P2 | yes | yes | no |
| 31 | `virtualmonitor` | phone -> desktop | `kdeconnect.virtualmonitor`, `.virtualmonitor.request` | `wlay` or headless `weston` | P4 | yes | yes | no |

## Platform notes

KDE Connect's own matrix is asymmetric. The Android app does not implement
virtual monitors, and a few plugins are one-way. v2 matches the desktop half of
each plugin and does not claim Android-side features the official app does not
have.

- **Run command**: Android can trigger desktop commands, but the desktop cannot
  trigger Android commands. `runcommand` therefore executes only keys named in
  `plugins.toml`; `remotecommands` is the desktop-to-phone direction and needs no
  configuration.
- **SMS and contacts**: the phone owns the message store, so `sms` only requests
  and caches. Contacts run the other way: the desktop serves its own address
  book, and the `contacts` backend is installed only when `[contacts] directory`
  is configured.
- **Virtual monitor**: desktop-only; the official Android client has no virtual
  display support.
- **Clipboard images**: the wire protocol carries text. File clipboard entries
  use `kdeconnect.clipboard.file` and are a v2 extension target, not a v1
  requirement.

## Configuration

Settings live in one file next to the daemon state, `plugins.toml`, overridable
with `--config-file`. A missing file means an empty configuration, which is the
safe default: `runcommand` executes nothing, `sendnotifications` forwards
nothing, and `sftp` refuses to start.

```toml
[runcommand]
# key:command, or key:Display name:command
commands = "lock:loginctl lock-session, music:Music:playerctl play-pause"

[sendnotifications]
apps = "firefox,kmail"

[sftp]
root = "/home/alice/Public"
user = "alice"

[contacts]
directory = "/home/alice/addressbook"
```

A malformed file fails at start with a line number rather than being silently
ignored.

## Test strategy

Every plugin must ship with:

1. Golden wire vectors under `crates/handfast-testkit/tests/vectors/`.
2. A contract test with `MockDesktop` if it touches the desktop.
3. A conformance test over the Rust peer for its packet flow.
4. A real-device checklist entry before `e2e` is marked yes.

The roster test in `handfast-plugins` fails if a plugin is added, removed or
renamed without updating this table's source of truth.
