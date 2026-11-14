# Handfast production program

Goal: make Handfast production ready, through P1 to P4 in `docs/ARCHITECTURE.md`.

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

- Windows state-file ACLs remain the P1 release blocker; the exact API and test are in `docs/ARCHITECTURE.md`.
- Windows incoming-file ACLs for `~/Downloads/Handfast` and its temp directory.
- The store record's private-key copy is not zeroized; the long-lived `TlsIdentity` copy is.
- Add unpair and accept-rollback regression tests and a positive control in the self-announce test.
- Fix the small doc nits: 1714 versus 1716, the interop log format, and stale zeroization notes.

Active parallel wave:

- Unit 4 net payload: merged in `0606bef`.
- Unit 5 runtime transfer: merged in `578e156`.
- Unit 6 share plugin: merged in `4c4f0de`.
- Unit 7 manager share routing: merged in `4dc1aa7`.
- Unit 8 inbound session runner: branch `feat/inbound-runner`, owner `ses_e27624f69ffePp39O7upPyN1eB`, brief at `C:\Users\mah\AppData\Local\Temp\opencode\unit8-inbound-runner-brief.md`.
- Windows ACLs: merged in `4f4a7cf`. The P1 Windows state and incoming ACL blocker is cleared.
- P1 hardening: merged in `b4d41ba`.- Share docs and fuzz: merged in `c36802a`.
- Connectivity report plugin: merged in `b74f2d8`.
- Notifications backend: merged in `840e7a1`.
- System volume plugin: PR [#209](https://github.com/mheci/handfast/pull/209), head `e054893`, verifier PASS, owner merging.
- Pause music plugin: merged in `b228c1c`.
- MPRIS control plugin: merged in `54b85e9`.
- Notifications close fix: merged in `c92dae7`.
- Notifications plugin: merged in `c3d86ac`.
- Find this device plugin: merged in `53e3b0f`.
- Clipboard backend: merged in `e1f2a99`.
- Unit 8 prep: done, runner brief in the session record.
- Unit 9 prep: done. The outbound transfer must run in a task or JoinSet, not inline in the session loop, so a 30 s accept deadline cannot stall packets, pings or pairing.

Comment review follow-ups:

- The merged notifications backend comment says notify-send has no close handle. That is false. `notify-send` can print an id. The current close is a no-op. Fix the comment and either use D-Bus close or return Unsupported when notifications are wired.
- The INTEROP dependency note is a deferred-deletion marker. Replace it with a tracked issue when units 10 and 11 land.
- The MPRIS per-player failure path and the systemvolume volume-only mute behavior are untested or differ from upstream. Park for the backend units.
- The notifications plugin roster narrowing and the Windows ACL comment fixes are in flight.

Wave rules:

- Each PR still gets one owner and one independent verifier at the current head.
- The plugin and backend PRs will conflict on `lib.rs`, `roster.rs`, `backends/mod.rs`, `Cargo.toml` and `Cargo.lock`. Owners rebase and keep all implemented modules.
- The coordinator merges the critical path first: unit 4, then 5 and 7, then 6, then 8. Independent plugin and backend PRs merge in any order after a pass.
- Unit 6 starts as soon as unit 5 merges. Unit 8 starts as soon as units 4, 5, 6 and 7 merge.
- Verifiers fan out as PRs appear, which keeps 16 or more agents active without two owners on the same files.

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
