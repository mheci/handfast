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

Active work:

- Branch: `feat/desktop-files`
- Owner: desktop files worker `ses_e2a0d3c48ffeDWtw695mT5zqkH`
- PR: [#197](https://github.com/mheci/handfast/pull/197), head `163f72c`
- Verifier: `ses_e29da1fddffeQ7IbLkSm6Yz50E`, running
- Evidence: sanitizer table, temp and commit tests including concurrent duplicates and both commit strategies, sweep scope and user-file safety, free-space math, `.handfast-tmp` startup failure, outgoing file checks, Unix and Windows default resolution, and Unix permissions; all 9 checks.

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
2. Files adapter and safe save in desktop. In flight.

Unit 2 decision: the primary commit path is a safe `std::fs::hard_link` on every platform, with `create_new` plus copy as the fallback. The design's `renameat2`/`MoveFileExW` primary needs new unsafe FFI; the hard link is already atomic and no-clobber and is the design's own step 3. The owner must test both strategies and state the deviation in the PR.
3. Independent testkit payload peer with `payload-self-test`.
4. Payload service in net.
5. Transfer intent and `Files` mapping in runtime.
6. Share plugin and roster.
7. Manager remote address, `ShareEvent` channel and close.
8. Inbound session transfer runner and redaction.
9. Outbound share path.
10. Local control channel and `hfctl`.
11. Daemon composition, docs and CI.

Share unit 1 verifier checklist:

- Read the PR diff, not only the owner summary.
- Confirm `payloadSize` serializes as a JSON number with `-1` for `Unknown`, not an externally tagged enum.
- Confirm the file body uses `creationTime` and `lastModified` in milliseconds, `open` defaults false, and the batch fields appear only as a pair.
- Confirm an empty file parses as `Known(0)` with no offer, and a non-zero size without a port is rejected.
- Confirm ports 1739 and 1764 are accepted and 0, 1716 and 1765 are rejected by both `ShareRequest` and `Packet::payload_offer`.
- Confirm `payload_offer` returns `None` for metadata-only and zero-size packets and does not change `payload()`.
- Confirm the five golden vectors exist and the testkit vector command lists them, with the testkit normal build still free of `handfast-proto`.
- Confirm no transport, daemon, desktop, runtime, plugin, version, release or notes changes.
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
