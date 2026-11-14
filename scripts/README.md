# Share interop scripts

`scripts/interop-share.sh` and `scripts/interop-share.ps1` run the manual P1
share gate in `docs/INTEROP.md` against a real Android or KDE Connect desktop
peer. Each script starts `handfastd` with a throwaway `--data-dir` and
`--incoming-dir`, captures the debug log, prints the local identity, the bound
TCP port and the control path, and walks the tester through the inbound and
outbound steps.

The scripts do not replace the checklist. Use them to collect the evidence the
checklist asks for.

## Prerequisites

- A Linux host for `handfastd` and, optionally, the KDE Connect desktop client.
  The Windows build has the same link code but no desktop backend.
- A paired Android phone and a paired desktop client. The link checklist in
  `docs/INTEROP.md` must pass first.
- The binaries built in release mode:

  ```sh
  cargo build --release -p handfast-daemon --bin handfastd --bin hfctl
  ```

- UDP 1716 and TCP 1716-1764 open on the Handfast host. For `ufw`:

  ```sh
  sudo ufw allow 1716/udp
  sudo ufw allow 1716:1764/tcp
  ```

- `tcpdump` or Wireshark for failure capture.
- `bash` for the shell script. PowerShell 7 or Windows PowerShell 5.1 for the
  PowerShell script.

The scripts look in `target/release` first and `target/debug` second. Set
`HANDFASTD` or `HFCTL` to use another binary path. They never read or write the
real profile directory.

## Commands

```sh
scripts/interop-share.sh
```

```powershell
.\scripts\interop-share.ps1
```

Each run creates a temp directory and prints it at start and after stop:

| Path | Contents |
| --- | --- |
| `<run>/data` | `state.db` and the TLS identity. It holds the private key. |
| `<run>/incoming` | Received files, with `.handfast-tmp` for partials. |
| `<run>/handfastd.log` | Full `RUST_LOG=debug` stderr. |
| `<run>/handfastd.stdout.log` | Standard output. PowerShell only. |

The control path is `$XDG_RUNTIME_DIR/handfast/handfastd.sock` when
`XDG_RUNTIME_DIR` is set, else `<run>/data/handfastd.sock`, on Unix. Windows
uses the named pipe `\\.\pipe\handfastd-<user SID>`.

Press Enter in the script terminal to stop `handfastd`. The script prints the
log path, the incoming directory and its contents, and a `tcpdump` command for
the next failed step.

## Manual steps

Pair both peers before the run.

Inbound from Android, with the KDE Connect app on the phone:

1. Share a photo to Handfast. Confirm `share.request` with the filename, the
   declared size and the payload port, then `ShareCompleted`. The file appears
   in `<run>/incoming` with no `.part` file left.
2. Share text, then an `https` URL. Confirm `ShareText` and `ShareUrl`. The log
   shows a length, never the body.
3. Share an empty file. Confirm a zero-byte file and no payload connection.
4. Share a file larger than 100 MiB. Confirm progress and a complete file.
5. Cancel a transfer. Confirm the partial file is gone and `ShareFailed` is
   logged.
6. Share two files at once. Confirm both arrive and neither is partial.
7. Confirm the payload TLS version is TLS 1.2 for Android.

Inbound from the desktop client:

1. Share a file, then text and an `https` URL. Confirm the same log lines and
   events as the Android steps.
2. Queue a second file behind a large first file. Confirm both arrive in order
   and neither is partial.
3. Confirm the payload TLS version is TLS 1.3 when the desktop offers it.

Outbound from Handfast, with a paired peer connected:

```sh
target/release/hfctl share --device <peer-id> --data-dir <run>/data /path/to/file
```

`hfctl.exe` is the same command on Windows.

`hfctl` prints `sent <n> bytes to <peer-id>` and exits 0 on success. A remote or
IO error exits 1, a usage error exits 2. Repeat with a zero-byte file, then with
a directory. The empty file must arrive and bind no payload port. The directory
must exit non-zero and send no packet.

The reject, unpair and changed-certificate steps stay in `docs/INTEROP.md`.

## Expected events

| Action | Expected log | Expected event |
| --- | --- | --- |
| File share | `share.request` with the filename, size and payload port | `ShareProgress`, then `ShareCompleted` |
| Text share | The text length, never the body | `ShareText` |
| URL share | The URL length, never the body | `ShareUrl` |
| Unsupported URL scheme | The scheme and a drop reason | None |
| Empty file | `share.request` with no payload port | `ShareCompleted` with 0 bytes |
| Cancelled or truncated transfer | The payload error and reason | `ShareFailed`, no file left |
| Batch | The file count and total declared size | `ShareBatch` |

The full table and the exit criteria are in the share section of
`docs/INTEROP.md`.

## Attach a pcap

Start the capture before the step that fails:

```sh
sudo tcpdump -i any -s 0 -w share.pcap 'tcp portrange 1716-1764'
```

Attach all of these to the failure record:

- `share.pcap`.
- `<run>/handfastd.log`, untruncated from the first handshake line.
- The identity packet printed by the script, and the peer device ID and
  fingerprint.
- The peer log. Android: `adb logcat -d | grep -i kdeconnect > android.log`.
  Desktop: `journalctl --user -u kdeconnectd -b > kdeconnectd.log`.
- The Handfast commit SHA, the peer app version and the OS versions.

Do not attach `state.db`. It contains the private key. The identity packet, the
fingerprints and the capture are enough for a link diagnosis.

## Blocked steps

`--incoming-dir` is owned by the daemon composition unit. When the built
`handfastd` does not accept the flag, the scripts print the incoming directory
as blocked and skip the inbound file checks. A blocked step is not a pass.
`hfctl` missing fails the script before the daemon starts, because the outbound
checks need it.
