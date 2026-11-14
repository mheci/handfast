# P1 real-client interop

This is the manual exit gate for the P1 link layer. The automated tests use the
independent testkit peer. They cannot prove that the official Android client
and the KDE Connect desktop client accept Handfast's hello, TLS roles,
certificate and pairing behavior. Run this checklist against real clients and
attach the evidence to the P1 exit record.

The run is manual and is not replaced by CI. A green CI proves the testkit and
the production code; it does not prove real-client interop.

## Prerequisites

- A Linux host for `handfastd` and, optionally, the KDE Connect desktop client.
  The Windows build has the same link code but no desktop backend.
- An Android phone with the official KDE Connect app on the same LAN.
- A firewall that allows UDP 1716 and TCP 1716-1764 between the peers.
- A disposable state directory. Do not test against a real profile.
- `tcpdump` or Wireshark on the Handfast host for failure capture.

Open the ports on the Handfast host. For `ufw`:

```sh
sudo ufw allow 1716/udp
sudo ufw allow 1716:1764/tcp
```

On Windows, allow `handfastd.exe` on private networks for UDP 1716 and TCP
1716-1764.

## Build and start

```sh
cargo build --release -p handfast-daemon
mkdir -p /tmp/handfast-interop
RUST_LOG=debug ./target/release/handfastd --data-dir /tmp/handfast-interop
```

`RUST_LOG=debug` shows the link handshake, the negotiated TLS version and the
pairing outcome. The daemon writes logs to stderr and the state to
`/tmp/handfast-interop/state.db`.

Read the local identity before starting, or in a second terminal:

```sh
./target/release/handfastd --print-identity --data-dir /tmp/handfast-interop
```

The printed packet has the device ID in `body.deviceId` and the advertised TCP
port in `body.tcpPort`. The certificate CN equals that device ID.

The startup log prints the local certificate fingerprint:

```text
INFO TLS identity loaded fingerprint=<64 lowercase hex characters>
INFO TCP listener bound tcp_port=1716
INFO discovery listening address=0.0.0.0:1716
```

The fingerprint is public. The private key never appears in a log.

## Expected identity, certificate and pairing behavior

| Step | Expected Handfast behavior | Expected peer behavior |
| --- | --- | --- |
| Discovery | Logs `discovered device` and persists a `devices_v1` record with the announced name and type. | The peer lists the Handfast device within one announce interval. |
| Android link | Accepts or dials, completes the plaintext hello and TLS. The debug log shows `TLS handshake complete version=TLSv1_2`. | The official Android app caps TLS at 1.2 through `SSLContext("TLSv1.2")`. |
| Desktop link | The same handshake with `version=TLSv1_3` when the desktop client offers 1.3. | KDE Connect desktop shows the device as reachable. |
| Certificate CN | Parses one peer certificate, requires exactly one CN and requires CN equal to the peer device ID. | The peer validates the Handfast certificate against its stored or first-seen pin. |
| Pairing request | Prints `Pairing request from <name> (<device ID>)` and the 64-character fingerprint, then waits up to 30 s for `y`. | The peer shows its own pairing request or waits for Handfast. |
| Accept | On `y`, writes the pin, sends `kdeconnect.pair` with `pair: true`, creates the session and runs `on_connect`. The log shows `pairing pin written` and `pairing accepted`. | The peer marks Handfast as paired. A ping from either side arrives. |
| Reject | On `n`, Enter or timeout, sends `kdeconnect.pair` with `pair: false` and writes no pin. | The peer shows the pairing as rejected. |
| Restart | Reuses the same certificate and fingerprint. The log shows the same `TLS identity loaded` fingerprint. | The paired peer reconnects without a new pairing prompt. |
| Unpair from the peer | Receives `pair: false`, clears the stored pin and the manager pin, stops the session and runs `on_disconnect`. | The peer shows Handfast as unpaired. |
| Changed certificate | A pinned peer with a different certificate is rejected before any packet, and the pin stays. | The peer cannot force an unpair by reconnecting with a new certificate. |
| Unpaired traffic | A ping or plugin packet from an unpaired peer is dropped before the daemon. Only `kdeconnect.pair` is accepted. | The peer cannot reach a plugin before pairing. |

## Android checklist

1. Start `handfastd` with `RUST_LOG=debug` and a fresh `--data-dir`.
2. Open KDE Connect on the phone. Confirm the Handfast device appears.
3. Confirm the Handfast log shows `discovered device` for the phone and the
   `devices_v1` record is created.
4. On the phone, open the Handfast device and request pairing.
5. Confirm the Handfast prompt shows the phone name, the phone device ID and a
   64-character fingerprint.
6. Confirm the phone certificate CN equals the phone device ID. The phone
   device ID is in its KDE Connect settings.
7. Type `y` and press Enter.
8. Confirm the phone shows Handfast as paired and a ping from the phone reaches
   the Handfast log as `plugin events` with `Ping`.
9. Confirm the Handfast debug log shows `version=TLSv1_2` for this link.
10. Stop and restart `handfastd` with the same `--data-dir`. Confirm the
    fingerprint is unchanged and the phone reconnects without a new prompt.
11. Send a ping from Handfast by using a paired peer action if the peer UI
    offers one, or confirm the phone receives the session's `on_connect`
    packets if the plugin emits any.

## Desktop checklist

1. Start `handfastd` with `RUST_LOG=debug`.
2. Start `kdeconnectd` on the desktop, or open the KDE Connect settings app.
3. Confirm each client sees the other.
4. Request pairing from the KDE Connect desktop device page.
5. Confirm the Handfast prompt shows the desktop name, device ID and
   fingerprint, and that the desktop certificate CN equals its device ID.
6. Accept on Handfast. Confirm the desktop shows Handfast as paired and that
   `version=TLSv1_3` appears when the desktop offers TLS 1.3.
7. Restart Handfast and confirm the desktop reconnects with the same
   fingerprint and no new prompt.
8. If the desktop presents an RSA certificate, confirm the handshake still
   completes. The testkit proves the RSA path; this step checks a real peer.

## Reject and unpair checks

1. Start with a fresh state directory. Request pairing from the peer.
2. Answer `n`. Confirm the peer shows rejection and the Handfast log shows
   `pairing rejected`. There is no pin in the state file.
3. Pair again and accept. Confirm the peer is paired.
4. Unpair from the peer. Confirm the Handfast log shows the link close and the
   session stop, and that a later pairing request prompts again.
5. Reconnect with the old pin and a changed certificate if the peer allows it.
   Confirm Handfast rejects the link and keeps the pin.

## Failure capture

Capture all of the following for a failed step:

- The full `handfastd` stderr with `RUST_LOG=debug`. Do not truncate the first
  handshake lines.
- The peer logs. Android: `adb logcat -d | grep -i kdeconnect > android.log`.
  Desktop: `journalctl --user -u kdeconnectd -b > kdeconnectd.log` or run
  `kdeconnectd` in a terminal with
  `QT_LOGGING_RULES="kdeconnect.*=true"`.
- A packet capture. On the Handfast host:
  `sudo tcpdump -i any -s 0 -w handfast-p1.pcap 'udp port 1716 or tcp portrange 1716-1764'`.
- The local identity packet from `--print-identity`.
- The peer device ID, peer certificate CN and the fingerprint shown in the
  Handfast prompt.
- The Handfast version or commit SHA, the peer app version and the OS versions.

The state file contains the private key. Do not attach `state.db` to an issue.
The identity packet, fingerprints and packet capture are enough for the link
diagnosis. If the state file is needed, copy it to a private location and
redact the `identity_v1` key material.

## Exit criteria

The P1 real-client gate passes when:

- The Android client completes discovery, TLS 1.2, pairing acceptance, ping and
  restart reuse.
- The desktop client completes discovery, TLS 1.3, pairing acceptance, ping and
  restart reuse.
- Reject writes no pin and unpair clears it.
- A changed certificate is rejected without clearing the pin.
- The captured logs show the negotiated TLS version and the three-way device ID
  correlation for each link.

Until this run passes, P1 is not done even when every CI check is green.

## Share and file transfer

This is the P1 share exit gate. Run it after the share units in
`docs/design/p1-share-file.md` land. CI proves the parser, the save policy and
the independent testkit payload peer. It cannot prove that the official clients
accept Handfast's share packets and payload TLS. The run is manual and is not
replaced by CI.

Dependency note. Not every share unit has merged. The inbound checks need the
daemon to compose the share path and its `--incoming-dir` flag. The outbound
checks also need the `hfctl` control client. Record a step as blocked while its
binary or flag is absent. A blocked step is not a pass. Delete this note once the
daemon composes the share path and `hfctl` has merged.

### Prerequisites

- A paired Android phone and a paired KDE Connect desktop client. The link
  checklist above must pass first.
- TCP 1716-1764 open on the Handfast host. The control link uses 1716-1764 and
  the payload uses 1739-1764 inside that range. The firewall rules above cover
  it.
- A fresh `--data-dir` and a disposable `--incoming-dir`. Do not point
  `--incoming-dir` at a real download directory.
- `tcpdump` on the Handfast host for failure capture:

```sh
sudo tcpdump -i any -s 0 -w share.pcap 'tcp portrange 1716-1764'
```

Start the daemon with both directories set:

```sh
RUST_LOG=debug ./target/release/handfastd \
  --data-dir /tmp/handfast-share \
  --incoming-dir /tmp/handfast-share-incoming
```

The incoming directory defaults to `$XDG_DOWNLOAD_DIR/Handfast`, falling back to
`~/Downloads/Handfast`, on Unix, and to `%USERPROFILE%\Downloads\Handfast` on
Windows. The daemon creates `<incoming-dir>/.handfast-tmp` for partial files and
sweeps only that directory.

### Android to Handfast

1. Share a photo to Handfast. Confirm the log shows `share.request`, the
   filename, the declared size and the payload port. Confirm the file appears in
   the incoming directory with the original name and no `.part` file remains.
2. Share text. When the daemon composes a clipboard adapter, confirm the text is
   on the desktop clipboard. Without one, confirm the warning and the `ShareText`
   event. Confirm the log line does not contain the text body.
3. Share an `https` URL. When the daemon composes a shell adapter, confirm the
   browser opens it. Without one, confirm the warning and the `ShareUrl` event.
   Confirm the log line does not contain the URL body.
4. Share a `file://` URL if the app allows it. Confirm Handfast drops it with a
   log line and opens nothing.
5. Share an empty file. Confirm a zero-byte file appears and no payload
   connection is made.
6. Share a file larger than 100 MiB. Confirm progress and a complete file.
7. Cancel a transfer from the phone. Confirm the `.part` file is deleted and a
   `ShareFailed` event is logged.
8. Share two files at once. Confirm both arrive, the second gets a suffix if the
   names collide, and no file is partial.
9. Confirm the payload TLS version in the log is TLS 1.2 for Android. Confirm
   Handfast's payload handshake and idle timeouts are 8 s, inside Android's 10 s
   payload socket timeout.

### Desktop to Handfast

1. Share a file from the KDE Connect desktop client. Confirm the same behavior
   as the Android photo step.
2. Share text and an `https` URL. Confirm the same behavior as the Android text
   and URL steps, including the adapter fallback.
3. Confirm the payload TLS version is TLS 1.3 when the desktop offers it.
4. Send a large first file and a second file queued behind it. Confirm the
   second payload connection is established early, both files arrive in order,
   and no file is partial.

### Handfast to a real client

Blocked until units 10 and 11 land. These steps need the `hfctl` binary and its
`share` command from unit 10, and the daemon composition from unit 11, both in
`docs/design/p1-share-file.md`.

1. Start `handfastd` with the same `--data-dir`.
2. Run `hfctl share --device <peer-id> /path/to/file`.
3. Confirm the phone or desktop receives the file and `hfctl` exits zero.
4. Run `hfctl share` with a zero-byte file. Confirm the peer creates an empty
   file and Handfast binds no payload port.
5. Run `hfctl share` with a directory or a special file. Confirm it exits
   non-zero and sends no packet.
6. Run `hfctl share` as a different local user on Unix. Confirm the peer
   credential check rejects it.

### Expected logs and events

| Action | Expected Handfast log | Expected event |
| --- | --- | --- |
| File share | `share.request` with the filename, declared size and payload port | `ShareProgress`, then `ShareCompleted` with the final path and byte count |
| Text share | The text length, never the body | `ShareText` |
| URL share | The URL length, never the body | `ShareUrl` |
| Unsupported URL scheme | The scheme and a drop reason | None |
| Empty file | `share.request` with the filename and no payload port | `ShareCompleted` with 0 bytes |
| Cancelled or truncated transfer | The payload error and its reason | `ShareFailed`, no file left |
| Batch | The file count and total declared size | `ShareBatch` |

The daemon logs every text-bearing event, including `Clipboard`, `ShareText` and
`ShareUrl`, by type and length. A log line that contains the text or the URL
body fails the step. Progress events are best effort and may be dropped.
Completion and failure events are not.

### Failure capture

Capture all of the following for a failed step:

- The full `handfastd` stderr with `RUST_LOG=debug`.
- Android: `adb logcat -d | grep -i kdeconnect > android.log`.
- Desktop: `journalctl --user -u kdeconnectd -b > kdeconnectd.log` or a
  `kdeconnectd` terminal with `QT_LOGGING_RULES="kdeconnect.*=true"`.
- The packet capture from the prerequisite command.
- The peer device ID and the payload certificate fingerprint.
- The Handfast commit SHA, the peer app version and the OS versions.

Do not attach `state.db`. It contains the private key.

### Exit criteria

The P1 share gate passes when:

- Android and the desktop complete file and empty-file inbound shares.
- Text and URL inbound shares reach the clipboard and the browser when those
  adapters are present, and log the warning plus `ShareText` or `ShareUrl` when
  an adapter is absent.
- A large file completes with no partial file.
- A cancelled or truncated transfer leaves no file.
- An unpaired peer cannot share.
- A payload connection with a wrong certificate is rejected.
- The payload TLS version is 1.2 for Android and 1.3 for the desktop when
  offered.
- `hfctl share` completes the outbound file, empty-file and invalid-path checks
  against both clients.
- A full share channel or session queue closes the link and reports a failure
  rather than hanging the peer.
- The Windows incoming and state-file ACLs are verified or recorded as the
  remaining P1 release blocker.

Until this run passes, P1 is not done even when every CI check is green.
