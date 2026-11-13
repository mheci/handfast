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
- The old README history was purged. README now has one commit.

Program rules:

- One owner per PR. The owner is the only writer of its branch and carries the PR from build to merge.
- One verifier per PR, never the author. The owner merges only after the verifier passes the current head.
- No direct pushes to `main`. No release tag until the P1 exit criteria pass.
- Every unit names its evidence before work starts.
- The coordinator owns this file and the program plan. Workers do not edit it.

P1 plan:

1. Discovery wired into `handfastd`. Done in `6f85e3f`.
2. TLS link, certificate persistence and identity re-exchange. Design done in `c5a288e`; implementation in flight.
3. Pairing prompt and persistent trust.
4. TCP link and ping end to end.
5. Battery and share/file payload transfer.
6. Testkit integration and security tests.

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
7. Framed TLS link, hello and identity re-exchange. In flight.
8. Link manager limits, dedupe, rate limits, keepalive and shutdown.
9. Daemon composition and startup ordering.
10. Docs, CI and real-client P1 interop.

Store follow-ups:

- `set_paired` is test-only but leaves a pin in place when called with `false`; the pairing unit must use `set_trusted` and the legacy method should be removed or marked deprecated.
- Corrupt-identity and device-ID-versus-settings startup tests belong with the daemon unit.
- Private-key zeroization and Windows state-file ACLs remain open.

Hardening follow-ups:

- Add fuzz targets for `IdentityHello` and `DiscoveryAnnouncement`; the current fuzz target only covers the strict `Identity`.
- Make the desktop fixtures use an epoch `id` instead of 0; no parser reads it.
- Remove or deprecate `set_paired`; the pairing unit must use `set_trusted`.

Active work:

- Branch: `feat/tls-link`
- Owner: link worker, to be spawned
- PR: pending
- Verifier: pending
- Evidence: the dialer sends the plaintext hello, both roles complete TLS, exchange encrypted identities, apply `PeerCertificate::check` and `check_identity` before any frame, and then exchange framed packets. Full-link tests use the independent testkit peer for TLS 1.2/1.3 and ECDSA/RSA. All 9 checks.

Testkit follow-ups:

- Add the minimal UDP announcement before the unit 10 discovery-driven dial test.
- Add the wrong-CN, pin, identity, protocol, silent-handshake and oversized-frame knobs in unit 8.
- Update the testkit CI comment, which still says codec only.

Trust hardening follow-ups:

- Enforce P-256 on `TlsIdentity::from_parts`, not only on generation.
- Add a trailing-key-bytes test if strict stored-key DER consumption matters.
- Zeroize the private key on drop before the link unit ships.
- `TrustTable` and `NetError::Tls` land with the link or manager caller.

Design risks carried into implementation:

- Real Android TLS 1.2 and desktop TLS 1.3 interop, including an RSA peer, is unverified until the P1 exit.
- The UDP broadcast test assumes the CI kernel delivers a broadcast to two reuse-addr sockets.
- Pairing clock skew, Windows state-file ACLs and private-key zeroization remain open.
- The unpaired idle timeout must not close a link inside the 30-second pairing window.
- The testkit TLS peer duplicates handshake logic on purpose.
