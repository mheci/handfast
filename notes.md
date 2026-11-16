# Handfast production program

Goal: make Handfast production ready, through P1 to P4 in `docs/ARCHITECTURE.md`.

Hardening queue, 2026-11-14:

- The shell runs on Windows PowerShell 5.1 after the OpenCode config was pointed at the absolute `powershell.exe` path. `pwsh` is not installed.
- Main is at `789a51f`. The P1 share implementation and the first two hardening waves are complete. Third wave complete. State and incoming path ownership checks merged.
- The manual P1 exit gate remains: a real Android phone and a real KDE Connect desktop client. Run `scripts/interop-share.sh` or `.ps1` and follow `docs/INTEROP.md`. CI cannot replace it.
- Merged in the first wave: `#219` store zeroization, `#220` clipboard, `#221` notifications D-Bus, `#222` link frames, `#223` interop harness, `#225` redaction, `#227` payload progress, `#228` testkit and fuzz hardening, `#218` unit 11, `#224` MPRIS, `#229` input plugins, `#230` manager hardening, `#226` system backends.
- Merged in the second wave: `#232` frame capacity, `#233` discovery throttle, `#235` payload final progress, `#234` store decode zeroization, `#231` session drop diagnostics, `#236` MPRIS no-track distinction, `#237` notification id serialization, `#240` screensaver refcount, `#241` interop harness fixes, `#238` TLS key zeroization, `#239` docs parity, `#242` daemon backend wiring.
- The daemon now installs Files plus the six CLI adapters when their tools exist, and identity capabilities follow the installed adapters.
- Read-only audits complete: P1 network and share security, desktop and plugin correctness. Their findings seeded the third and fourth waves.
- Desktop audit complete, 16 findings. Third wave launched, one owner per file set: notifier lifecycle `ses_e254d2c93ffdyyotZKPD9X4VL2`, notification namespace `ses_e254d2c92ffe3fJlvwgDmxS3NB`, peer string caps `ses_e254d2c8cffddcd5y8guebO1uX`, pause/resume on disconnect `ses_e254d2c87ffeLR6vyG0tjGGaeD`, files hardening `ses_e254d2c85ffeb9S3aJbHBhz1y2`, session stop preempt `ses_e254cc475ffdi4rBlOHVmyxMFP`, share dedupe `ses_e254cc473ffeWHEpMuxDfB6BbG`, clipboard and volume gates `ses_e254cc46fffeoJNQRkcpqFQKMu`, screensaver child lifetime `ses_e254cc46effeRtDQ6u7yDvZ7jn`.
- Network audit complete, 9 findings. Discovery flood `ses_e2548894effepI0tKr4e5bdcgD` (discovery.rs, main.rs, store.rs) and pending handshake admission `ses_e2548894effdiDiAfFH202Nbzh` (manager.rs, policy.rs) are in PRs.
- Third wave merged: `#243` peer caps, `#244` clipboard and volume gates, `#245` notification namespace, `#246` share dedupe, `#247` screensaver lifetime, `#248` files hardening, `#249` notifier lifecycle, `#250` pending handshakes, `#251` session stop, `#253` pause/resume.
- Third wave complete: all PRs merged, `#252` last at `bceaa91`.
- Fourth wave merged: `#254` incoming space reservation, `#255` interop harness nits, `#256` session stop design docs, `#257` share-during-pairing queue, `#258` state path safety, `#259` incoming owner check.
- P2 feature wave: `lockdevice` merged as `dc468ec`, lock-only with a `loginctl` backend.
- P1 gate finding, 2026-11-15: the phone discovers the daemon and connects. The plaintext hello fix made TLS 1.2 complete. The encrypted identity then failed with the same 2 GB frame length. The official Android `LanLink` reads and writes every post-TLS packet as a newline-delimited JSON line (`readLineBounded`, `NetworkPacket.serialize() + "\n"`), so the entire LAN link codec, not just the hello, must be lines. PR `#261` extends the identity exchange and steady-state link to lines; the local control channel keeps its 4-byte frame.
- P1 gate progress, 2026-11-15, real Android: discovery works, TLS 1.2 completes, secure identity and pairing work with `#261` at `fd1ecea`. Outbound `hfctl share` sent 46 bytes to the phone successfully. Inbound photo `IMG-20261112-WA0002.jpeg` (2,493,278 bytes) committed. Ping events arrive and are logged; Windows has no notification backend, so no toast. The phone redials on every 5 s announcement and Handfast replaces the session, so a fix pauses announcements while a link is active. Remaining: stable-link ping, restart reuse, empty/large/cancel/two-file, reject/unpair, changed certificate, desktop client, and the Windows ACL second-user check.
- The testkit mirrored the framed production codec, so CI could not catch the wire-format bug. The manual gate found it.
- The live daemon is running the full line codec from the `handfast-hello-line` worktree, paired with the Android phone. `hfctl` is built in that worktree.
- New requirement, 2026-11-15: replace `handfastd`, `hfctl` and any future GUI/updater with one monolithic `handfast` / `handfast.exe`. Design package `ses_e216fb658ffe4Qu3xl3y0xtf1C` on `docs/monolithic-binary`. Planned rollout: daemon library boundary, CLI subcommands (`daemon`, `share`, `status`, `identity`, `version`, `service`, `update`, `gui`), control status request, per-user autostart (Windows logon task, systemd user unit), feature-gated egui GUI, verified self-update, then packaging and script/docs migration to one exe.
- Main is at `43d1beb`. `#261` full line codec and `#262` announcement pause merged. The real Android gate passed discovery, TLS 1.2, secure identity, pairing, ping, outbound share and a 2.49 MB inbound file on the line codec.
- The phone has not reconnected since the announce-pause restart, likely asleep. The stable-link check waits for it.
- Monolith design merged as `14d2bd5`. The design fixes the release GUI feature, systemd user unit sandbox, Task Scheduler XML, aarch64 removal, identity `Send + Sync` errors, global `--data-dir`, and xtask `--all-features`.
- Monolithic CLI merged as `43d1beb`: one `handfast` binary, `handfastd`/`hfctl` deleted, library daemon root with a shutdown future, Unix SIGTERM with an end-to-end abort test, endpoint rename, global `--data-dir`, `identity --tcp-port`, scripts and tests migrated.
- Autostart service merged as `1600294`: Windows Task Scheduler XML, systemd user unit, install/uninstall/start/stop/status, checked stop with a 10 s deadline, absolute paths, unique unit temp, `%` escaping.
- GUI merged as `6190e1f`: `gui` feature with eframe 0.33, the window, child daemon, status polling, Start/Stop/Open/Refresh, no-subcommand rule, tenth `gui` CI job with clippy, xtask `--all-features` removal, corrected deny chains and `unused-ignored-advisory = "deny"`. The manual window check is a release gate; the release stays headless until packaging.
- Update merged as `7da6fc7`: verified check/apply, FFI module split, rollback/temp tests, Debian 12 portable asset, unsigned Windows asset guard, bounded wait. The real apply happy path still needs a signed 0.3.0 draft release.
- Packaging merged as `27e9e14`: one `handfast` per package, per-user MSI with deferred `service install`/`uninstall` actions, renamed deb/Arch packages, systemd user unit, third-party notices asset, runtime dependencies, version 0.3.0.
- The monolith is code-complete. `handfast.exe` builds with `--features gui` (10.75 MB) and the real smoke passed: `version` prints `0.3.0 (x86_64-pc-windows-gnu)`, help lists `daemon/share/status/service/update/gui/identity/version`, `status` reports pid/uptime/identity/fingerprint/port/incoming/connected/paired/transfers, `service stop` shuts the daemon down cleanly, `update check` reports up to date, `identity` prints the packet, `share` without a daemon exits 1, and the GUI process opens and stays alive.
- Remaining manual release gates: GUI window layout/buttons, update from a signed 0.3.0 draft release on Windows and Linux, MSI/deb/Arch installs and upgrades, P1 Android/desktop gate, then tag 0.3.0.
- Pairing merged as `5528440`: pending requests in `handfast status`, GUI Accept/Reject, `handfast pair --accept/--reject`, fingerprint-bound decisions, late decisions rejected, stdin EOF parks and stdout failure no longer rejects. Smoke: `status` shows `pairing requests: 0`, `pair` with no request exits 1.
- Worktree cleanup: 140 Handfast worktrees removed from `~/Projects`; only `C:\Users\mah\Projects\handfast` remains. Unrelated projects untouched.
- The daemon is stopped; the paired phone will reconnect when the new `handfast --data-dir <paired state> daemon` runs. The state schema is unchanged.
- The daemon must run in the user session, not as a SYSTEM service, because the desktop adapters need the user's clipboard, notifications, MPRIS, volume, shell and screensaver.
- The user is back; the pairing fix is the current priority. The manual release gate resumes after it.
- Queued low findings: Windows control pipe token, eager queued-file handshake (needs the desktop client evidence), pairing prompt stale-answer race, `p1-share-file.md:782` two-to-three JoinSets docs fix.

Current state:

- `v0.2.0` is the latest stable release. It has native Debian 12, Ubuntu 24.04 and Arch Linux packages plus an unsigned Windows MSI.
- Discovery landed on `main` in `6f85e3f` with an independent verifier pass. `handfastd` announces, receives peers and persists devices without clearing pairing.
- The TLS link design landed on `main` in `c5a288e` after two adversarial reviews and three verifier rounds. The design is `docs/design/p1-tls-link.md`.
- The provider gate landed on `main` in `0dacce2`. rustls with ring and `tls12` builds on Windows with the static CRT, and real TLS 1.2 and 1.3 loopback tests pass on Linux.
- Unit 1 landed on `main` in `cd47f52`. The store persists `identity_v1`, peer pins, atomic trust, authenticated identity updates and 0.2.0 plus pre-audit migrations.
- Unit 2 landed on `main` in `b7118ab`. Proto has the real Android and desktop identity hello, the desktop and Android discovery announcement, and the strict encrypted identity boundary.
- Unit 3 landed on `main` in `e892f7f`. Discovery returns `DiscoveryAnnouncement`, accepts the desktop four-field UDP packet and the Android full packet, and the daemon persists both.
- Unit 4 landed on `main` in `b354f43`. `handfast-net` has the certificate fingerprint, local endpoint, peer certificate, peer trust and the three-way identity correlation policy.
- Unit 5 landed on `main` in `4fa5c58`. The independent testkit TLS peer covers TLS 1.2 and 1.3 with ECDSA and RSA, and the CI testkit job runs `tls-self-test`.
- Unit 6 landed on `main` in `d26b8df`. TLS 1.2 and 1.3 configs, mutual TLS, the accept-any chain verifier with signature proof, `PeerCertificate::check` and the raw TLS 1.1 rejection test.
- Unit 7 landed on `main` in `69f17a4`. The framed TLS link handles both roles, plaintext hello, encrypted identity re-exchange, certificate and identity checks before frames, keepalive and explicit Unix port reuse.
- Unit 8 landed on `main` in `79d3bb9`. The link manager owns admission, caps, per-IP limits, peer TTL, last-wins dedupe, backoff, keepalive supervision, ordered events and shutdown.
- Unit 9 landed on `main` in `2cb871b`. The daemon composes the store, local identity, TCP listener, discovery, manager, device sessions, pairing gate and shutdown. The first runnable link daemon exists.
- Unit 10 landed on `main` in `8e40668`. The TLS design is closed: Windows and fuzz CI, zeroization, prompt queue, migration and on_disconnect tests, P1 docs and the manual interop checklist.
- Share unit 1 landed on `main` in `91bd6bb`. Proto has the share wire types, custom `-1` serde, validated payload offers, builders and five golden vectors.
- Share unit 2 landed on `main` in `8feb182`. The desktop Files adapter has the sanitizer, private temp directory, safe no-clobber commit, free-space math, sweep, mock files and platform defaults.
- Share unit 3 landed on `main` in `e290af1`. The testkit payload sender and receiver cover raw-bytes TLS, failure knobs, a 16-case self-test and the CI `payload-self-test`.
- The old README history was purged. README now has one commit.

Program rules:

- One owner per PR. The owner is the only writer of its branch and carries the PR from build to merge.
- One verifier per PR, never the author. The owner merges only after the verifier passes the current head.
- No direct pushes to `main`. No release tag until the P1 exit criteria pass.
- Every unit names its evidence before work starts.
- The coordinator owns this file and the program plan. Workers do not edit it.

P1 plan:

1. Discovery wired into `handfastd`. Done in `6f85e3f`.
2. TLS link, certificate persistence and identity re-exchange. Done: design `c5a288e`, units 0-10 through `8e40668`.
3. Pairing prompt and persistent trust. Done in `2cb871b` and hardened in `8e40668`.
4. TCP link and ping end to end. Done in `69f17a4` and `2cb871b`.
5. Share/file payload transfer. Design merged in `fa9733c`; unit 1 in flight.
6. Real Android and desktop interop exit. Manual, checklist in `docs/INTEROP.md`.
7. Windows state-file and incoming-file ACLs. P1 release blocker, not implemented.
8. Battery and the remaining plugins.

P2 to P4 follow the parity matrix in `docs/PARITY.md`.

Discovery follow-ups:

- Validate `--tcp-port 0` when the link layer owns the port.
- Add a self-skip test for a peer that announces the daemon's own device ID.
- Throttle or dedupe the five-second `last_seen` writes if peer counts or disk writes grow.
- Announce on every interface broadcast, not only `255.255.255.255`, for multi-homed hosts.
- Supervise discovery after a socket read error instead of stopping until daemon restart.
- The identity now advertises `tcpPort`; the link unit must actually bind and test that port.

TLS implementation sequence:

0. Provider spike: done in `0dacce2`.
1. Store `identity_v1`, optional peer pin, trust updates and migration tests. Done in `cd47f52`.
2. `IdentityHello` and `DiscoveryAnnouncement` in proto with real Android and desktop packet fixtures. Done in `b7118ab`.
3. Discovery compatibility for the desktop UDP packet. Done in `e892f7f`.
4. Fingerprint and peer-trust policy, including CN and identity correlation. Done in `b354f43`.
5. Independent testkit TLS peer and RSA fixture. Done in `4fa5c58`.
6. TLS 1.2 and 1.3 configs and TLS-only tests. Done in `d26b8df`.
7. Framed TLS link, hello and identity re-exchange. Done in `69f17a4`.
8. Link manager limits, dedupe, rate limits, keepalive and shutdown. Done in `79d3bb9`.
9. Daemon composition and startup ordering. Done in `2cb871b`.
10. Docs, CI and real-client P1 interop. Done in `8e40668`; the real-client run remains manual.

P1 security follow-ups:

- `TlsIdentity::from_parts` drops its key argument unzeroized on validation error paths, and `tls.rs` makes transient key copies for the rustls configs. The store buffer is fixed; these remain.
- A serde decode error can drop a partial key Vec inside `serde_json`. A custom deserializer would be needed.
- The Windows pipe and state DACLs are shape-tested. The second-local-user rejection remains manual.
- `LocalFiles::new` resets an existing root's DACL. Documented, but visible to a user who shares that folder.
- The workspace `unsafe_code = deny` is still inert in crates that do not opt in. The daemon and desktop now have local policies.
- The real Android and desktop interop run is still manual.

Active parallel wave:

- Unit 4 net payload: merged in `0606bef`.
- Unit 5 runtime transfer: merged in `578e156`.
- Unit 6 share plugin: merged in `4c4f0de`.
- Unit 7 manager share routing: merged in `4dc1aa7`.
- Unit 8 inbound session runner: merged in `c429058`.
- Unit 9 outbound share: merged in `89eaf75`.
- Unit 10 control channel: merged in `a6c1234`.
- Unit 11 daemon composition: branch `feat/daemon-compose`, owner `ses_e26686125ffe9xqjvgyCurAW2s`, brief at `C:\Users\mah\AppData\Local\Temp\opencode\unit11-composition-brief.md`.
- Incident: commit `708fee3` accidentally swept the unit 10 working files into main and was reverted by `6241ec8`. Main's tree is restored to `488e851`; PR #217 remains the source of truth.
- Windows ACLs: merged in `4f4a7cf`. The P1 Windows state and incoming ACL blocker is cleared.
- P1 hardening: merged in `b4d41ba`.
- Share docs and fuzz: merged in `c36802a`.
- Connectivity report plugin: merged in `b74f2d8`.
- Notifications backend: merged in `840e7a1`.
- System volume plugin: merged in `88f8d9b`.
- Pause music plugin: merged in `b228c1c`.
- MPRIS control plugin: merged in `54b85e9`.
- Notifications close fix: merged in `c92dae7`.
- Notifications plugin: merged in `c3d86ac`.
- Find this device plugin: merged in `53e3b0f`.
- Clipboard backend: merged in `e1f2a99`.
- Unit 8 prep: done, runner brief in the session record.
- Unit 9 prep: done. The outbound transfer must run in a task or JoinSet, not inline in the session loop, so a 30 s accept deadline cannot stall packets, pings or pairing.
- Unit 10 prep: done. Control frame, Unix peer check, Windows pipe DACL, `hfctl`, error contract and tests are mapped.
- Unit 11 prep: done. Startup order, session wiring, limits, docs, test matrix and the manual gate are mapped.

Comment review follow-ups:

- The merged notifications backend comment says notify-send has no close handle. That is false. `notify-send` can print an id. The current close is a no-op. Fix the comment and either use D-Bus close or return Unsupported when notifications are wired.
- The INTEROP dependency note is a deferred-deletion marker. Replace it with a tracked issue when units 10 and 11 land.
- The MPRIS per-player failure path and the systemvolume volume-only mute behavior are untested or differ from upstream. Park for the backend units.
- The notifications plugin roster narrowing and the Windows ACL comment fixes are in flight.

Hardening wave:

- Manager close and drop diagnostics: `ses_e262a03d8ffdXraWAF7T9rQDp3`.
- Payload progress and chunk overhead: `ses_e262a03d6ffe2SJn27uyVSGBwf`.
- Link frame write buffers: `ses_e262a03d2ffd2oqU3bRmLfNO2u`.
- Store transient key zeroization: PR [#219](https://github.com/mheci/handfast/pull/219), head `8454326`, verifier `ses_e2618ebf3ffeECnbhtbf01GMZ3` running.
- Redaction single-pass: `ses_e262a03cbffe57J21XkW1RETO2`.
- Clipboard empty selection and builders: `ses_e262a03c9ffeNE4BLUw9pB7yXF`.
- Notifications D-Bus close: `ses_e26278ce9ffd7SnSgBuGIlvuc3`.
- System backends: `ses_e26278ce8ffeSU7pvCOorbBpmp`.
- MPRIS backend: `ses_e26278ce3ffeQzKfayH5KhTTZp`.
- Input plugin batch: `ses_e26278cdeffejYcgyAmJV3QruY`.
- Testkit and fuzz hardening: `ses_e26278cddffeoDwT06aje15277`.
- Interop harness: `ses_e26278cdcffe1aqsAUjONFO0LU`.

Wave rules:

- One owner per PR, one independent verifier at the current head.
- Unit 11 has priority. It touches `main.rs`, the daemon tests and the docs; the hardening owners avoid those files.
- Desktop backend PRs conflict on `backends/mod.rs`, `lib.rs` and `Cargo.toml`. Owners rebase and keep every module.
- The input plugin batch is the only plugin owner, so roster conflicts are limited.
- Merge order: unit 11, then net, store and daemon fixes, then desktop backends, then plugins, testkit and scripts.
- Every owner pushes early and lets CI find compile errors because the host cannot link.

Share design decisions:

- Payload is TLS, not plain TCP. The sender listens on 1739-1764 and is the TLS server; the receiver dials and is the TLS client. Pinned certificate and client certificate on both sides.
- Empty files carry no payload fields. Completion is EOF. The declared size is only a check.
- Incoming directory: `$XDG_DOWNLOAD_DIR/Handfast`, else `~/Downloads/Handfast`; Windows `%USERPROFILE%\Downloads\Handfast`; 0700; `--incoming-dir` override.
- Extra bytes: accept the declared size, cap extra at 64 MiB, fail beyond and delete the temp. The free-space check includes the extra cap and the reserve.
- Batches use per-file payload ports. The `open` flag is parsed but ignored. No auto-open for URL-like text.
- Outbound real-client check needs a minimal local control channel (Unix socket or Windows named pipe) or is blocked on `handfast-ipc`. The one-shot `handfastd share` flag is rejected: redb locks the store and a second process cannot reach the running session.
- Shape B is accepted: the plugin returns transfer intents, the session owns lifecycle, `handfast-net` moves bytes, the desktop saves.

Share design findings at `5c1a941`, all sent to the owner:

- Security: check-then-rename overwrite, sweep deleting user files, extra bytes consuming the reserve, 30 s outbound timeout exceeding Android's 10 s, filename suffix overflow, listener open before `start`, lossy share packet delivery, impossible one-shot outbound trigger.
- Architecture: mock Files `tempfile` dev-dependency, `DesktopRequirement::Files` mapping false, queued receive missing the accept deadline, outbound empty file binding a listener, firewall range excluding 1716, `--print-identity` creating the incoming directory, Windows incoming default using `HOME`, silent 65th-packet drop, clipboard text in daemon logs.
- Verified payload facts kept: sender TLS server, receiver TLS client, port 1739-1764, empty files without payload, EOF completion, Android TLS 1.2 and desktop TLS 1.3.

Share implementation sequence:

1. Share schema and golden vectors in proto. Done in `91bd6bb`.
2. Files adapter and safe save in desktop. Done in `8feb182`.
3. Independent testkit payload peer with `payload-self-test`. Done in `e290af1`.
4. Payload service in net. In flight.
5. Transfer intent and `Files` mapping in runtime.
6. Share plugin and roster.
7. Manager remote address, `ShareEvent` channel and close.
8. Inbound session transfer runner and redaction.
9. Outbound share path.
10. Local control channel and `hfctl`.
11. Daemon composition, docs and CI.

Share implementation notes:

- Unit 2 commit primary is a safe `std::fs::hard_link` on every platform, with `create_new` plus copy as the fallback. Windows raw errors 1 and 50 route to the fallback for FAT and exFAT.
- Parked for hardening: existing root permissions, free-space measured on the temp filesystem, short-NAME_MAX coverage, copy-fallback crash atomicity, Windows ACLs.

Share unit 4 verifier checklist:

- Read the PR diff, not only the owner summary.
- Confirm the sender binds 1739-1764 on all IPv4 interfaces, exposes the port before start, measures the 30 s accept deadline from bind, retries up to 3 handshakes, requires a client certificate and compares its DER exactly with `PayloadPeer.certificate`.
- Confirm the receiver dials under a 5 s timeout, holds the raw TCP connection for eager queued files, runs the TLS handshake later under 8 s, compares the server DER exactly, reads to EOF with an 8 s per-read deadline, and writes to the sink with optional progress.
- Confirm the size rules: short -> `SizeMismatch`, extra within 64 MiB -> accepted and reported, extra beyond -> `TooLarge`, unknown -> capped at `max_size`.
- Confirm the active and pending semaphores, the wait deadlines, and that dropping `PreparedSend`, `PendingPayload` or a receive future closes the socket and releases the permit.
- Confirm the shared bind helper keeps the link's SO_REUSEADDR, backlog and error behavior unchanged.
- Confirm the testkit hooks are minimal and the testkit normal build still has no `handfast-proto` or `handfast-net`.
- Confirm tests cover both directions, TLS 1.2/1.3, truncation, extra, unknown, wrong certificate, missing client certificate, raw TCP, accept timeout, backed-up control link, cancellation, concurrency, port bounds and port exhaustion.
- Confirm no daemon, desktop, runtime, plugin, proto, store, version, release or notes changes.
- Confirm all 9 checks pass at the head.
- Post PASS or FAIL. Do not edit or merge.

After the share implementation:

- P1 share/file implementation and the real Android and desktop interop exit.
- Desktop backends and the remaining 28 plugins.
- P2 to P4 parity.
- P1 security hardening and a new stable release.

Design risks carried into implementation:

- Real Android TLS 1.2 and desktop TLS 1.3 interop, including an RSA peer, is unverified until the P1 exit.
- The UDP broadcast test assumes the CI kernel delivers a broadcast to two reuse-addr sockets.
- Pairing clock skew and Windows state-file and incoming-file ACLs remain open.
- The unpaired idle timeout must not close a link inside the 30-second pairing window.
- The testkit TLS peer duplicates handshake logic on purpose.

Parity wave 1, 2026-11-16, branch `feat/parity`:

- Branding: two interlocking rings, teal `#128C7E` over copper `#E8833A`, from `scripts/branding.py`. Exports: 16 to 1024 PNG, a 7-size ICO, 128 px RGBA for the window, color, mono and wordmark SVG. Wired into the GUI window, the MSI product and shortcut icon, the deb and Arch hicolor trees, `handfast.desktop`, and `docs/branding.md`. The SVG rendered in Chromium with both ring colors; the ICO loads in .NET.
- Windows backends: clipboard (`clipboard-win`), notifications (toast through in-box PowerShell with tag close), media (SMTC through in-box PowerShell), volume (WASAPI through `volumecontrol-windows`), screensaver (`keepawake` on a dedicated thread), lock (`rundll32.exe`), shell (`explorer.exe`, http and https only), files (existing ACL path). Every tool is an absolute System32 path. No new unsafe. The daemon now installs these on Windows instead of only Files.
- Real Windows evidence: clipboard round trip, volume read and rewrite (44%, unmuted), keep-awake hold and release, live SMTC player list and now playing, and a toast show and close all passed with `cargo test -p handfast-desktop -- --ignored`.
- Outbound clipboard: `Plugin::on_tick` plus a 2 s session timer. The plugin sends `clipboard.connect` on connect and `clipboard` on a local change, caps the selection at 256 KiB, and never echoes an inbound write. Roster now advertises clipboard outgoing.
- Roster truth: fixed the inverted remotecommands, sftp and shareinputdevices entries, added remotekeyboard echo and keyboardstate incoming, removed sms.messages from SMS outgoing.
- Docs: `docs/ARCHITECTURE.md` now lists Linux and Windows adapters; `docs/PARITY.md` has the Windows table and the named gaps (no Windows input, no player volume read, 2 s clipboard poll, toast AUMID follow-up).
- Local checks: desktop 133 pass plus 5 ignored real tests, daemon 129 pass, plugins 132 pass, runtime 11 pass, fmt and clippy clean on the changed crates. The release `handfast.exe` builds at 10.99 MB. WiX is not installed locally, so the MSI build is unverified until CI or a release run.
- Pending: comment review, commit and PR. Then findmyphone, remotesystemvolume, telephony, MPRIS both ways, sendnotifications, runcommand, input, contacts, SMS, sftp, virtualmonitor.

Parity wave 2, 2026-11-16, same branch:

- `findmyphone` is implemented and registered: empty `kdeconnect.findmyphone.request` body, control request `Ring`, `handfast ring <device-id>`, and a Ring button on each connected device in the window.
- The roster test now accepts an implemented plugin with either an incoming or an outgoing contract; screensaver stays exempt because it is connection-driven.
- Evidence: plugins 133 pass, daemon lib 131 pass (including a ring-to-an-offline-device error test), bin 4 pass, fmt and clippy clean. The Unix ring CLI test compiles here and runs in CI.
- PR #272 CI first run: fmt, test, msrv, deny, docs, windows, testkit and fuzz passed; clippy and gui failed on an unused `FileLimits` import that was only used by a Windows test. Fixed by moving the import into the test module.

Parity wave 3, 2026-11-16, same branch:

- `mpriscontrol` now sends `kdeconnect.mpris` state to the phone: one packet per player on connect, on `requestPlayerList`, and on `requestNowPlaying`, with `player`, `playerList`, `title`, `artist`, `length`, `pos`, `isPlaying` and `volume`. The roster advertises `kdeconnect.mpris` outgoing.
- Evidence: plugins 137 pass, including connect, player-list and now-playing state tests; clippy clean.
- Still missing for full MPRIS parity: `Seek`, `SetPosition`, `requestVolume` and `albumArtUrl` handling, and the separate `mprisremote` plugin for phone media.

Parity wave 4, 2026-11-16, same branch:

- `telephony` is implemented and registered: ringing, missed and talking calls become desktop notifications with a stable per-number ID; `isCancel` closes the notification; the old `sms` event is left to the SMS plugin. The roster now requires `Notifications`, not the unimplemented `Telephony` capability.
- Evidence: plugins 147 pass, daemon 132 pass, clippy clean. The phone-side mute action is still missing and is named in `docs/PARITY.md`.
- MPRIS `requestVolume` is handled by sending the player state, which includes the volume.
- CI on `3754865` is fully green: all 10 checks pass. The MPRIS and telephony commits will trigger one more run.

Parity wave 5, 2026-11-16, same branch:

- `battery` is now bidirectional: it sends `battery.request` on connect, reports the host battery, and pushes a change every 60 s. Linux reads `/sys/class/power_supply`; Windows reads `Win32_Battery` through in-box PowerShell. A host with no battery reports `currentCharge: -1`.
- Evidence: plugins 153 pass, daemon 132 pass, clippy clean. The new `Power` trait has sysfs tests and a PowerShell fake-runner test.
- CI on `a23c338` failed `test` and `gui` because `kdeconnect.telephony` is now advertised by the telephony plugin with a notifier, not only by pausemusic with playerctl. Fixed by removing TELEPHONY from the playerctl probe case and adding `telephony_requires_a_notifier`.

Parity wave 6, 2026-11-16, same branch:

- `systemvolume` now pushes its sink list on connect instead of sending `requestSinks`, which is the remotesystemvolume direction. The roster outgoing drops `SYSTEMVOLUME_REQUEST`; the plugin still answers a peer's `requestSinks` and applies volume and mute requests.
- Evidence: plugins 153 pass, clippy clean. The old `on_connect_requests_the_sink_list` test was replaced by `on_connect_pushes_the_current_sink`.

Parity wave 7, 2026-11-16, same branch:

- Input is implemented on both platforms. Linux/Wayland uses `ydotool` for pointer movement, wheel and buttons and `wtype` for keys; Windows uses `enigo`. The `Input` trait now carries pointer buttons with click, double-click, press and release.
- `mousepad` is registered and enabled; `presenter` becomes active on hosts with an input adapter. The special-key table matches the KDE Connect desktop mapping 1-32.
- The daemon installs the Linux input adapter only when `WAYLAND_DISPLAY` is set and both `ydotool` and `wtype` are on `PATH`. Windows installs `enigo` unconditionally and creates the connection on first use.
- Evidence: desktop 154 pass (input argv tests and enigo key mapping), plugins 156 pass, daemon 132 pass, clippy clean. The real injection needs the manual phone touchpad/keyboard check.
- The real Windows enigo connection initialized in `the_real_input_backend_initializes` without injecting input. The first CI run on the input wave failed on a Linux-only test compile because the input probe used `registry` as a function; fixed by building the registry in the test.
- Still open for full input parity: absolute pointer and pressure for `digitizer`, input capture for `shareinputdevices`, and `singlehold`/`singlerelease` on Linux use the ydotool down/up masks.

Parity wave 8, 2026-11-16, same branch:

- `remotesystemvolume` is implemented and registered. The plugin keeps a per-device sink model from `kdeconnect.systemvolume` `sinkList` packets and deltas, capped at 64 sinks, and emits `PluginEvent::RemoteSinks` to the daemon. Invalid sink entries are skipped; out-of-range volumes and unknown sinks are warnings.
- The control channel has `RemoteVolume { device_id, sink, percent, muted }` with exactly one of percent and muted. The daemon scales percent to the sink's `maxVolume`, validates the sink against the model, and sends `{name, volume}` or `{name, muted}`. `handfast volume <device-id>` lists sinks; `--sink NAME --percent N`, `--mute` and `--unmute` change one. The window shows a slider and mute checkbox per reported sink.
- The `RemoteSink` model lives in `handfast-runtime`, carries `volume`/`maxVolume` raw units, and rounds `percent()` to the nearest point so 40% of 65536 reads back as 40.
- Evidence: plugins 166 pass, runtime 12 pass, daemon lib 134 pass (including the unknown-device and percent-scaling tests), bin 4 pass, clippy and fmt clean. `volume_cli.rs` type-checks and runs on Unix in CI.
- Android note: the official Android SystemVolume plugin is a controller, not a sink provider, so `remotesystemvolume` has no Android counterpart. It is exercised between two desktops.
