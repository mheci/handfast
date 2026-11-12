# Handfast production program

Goal: make Handfast production ready, through P1 to P4 in `docs/ARCHITECTURE.md`.

Current state:

- `v0.2.0` is the latest stable release. It has native Debian 12, Ubuntu 24.04 and Arch Linux packages plus an unsigned Windows MSI.
- Discovery landed on `main` in `6f85e3f` with an independent verifier pass. `handfastd` announces, receives peers and persists devices without clearing pairing.
- The TLS link design landed on `main` in `c5a288e` after two adversarial reviews and three verifier rounds. The design is `docs/design/p1-tls-link.md`.
- The provider gate landed on `main` in `0dacce2`. rustls with ring and `tls12` builds on Windows with the static CRT, and real TLS 1.2 and 1.3 loopback tests pass on Linux.
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
1. Store `identity_v1`, optional peer pin, trust updates and migration tests. In flight.
2. `IdentityHello` and `DiscoveryAnnouncement` in proto with real Android and desktop packet fixtures.
3. Discovery compatibility for the desktop UDP packet.
4. Fingerprint and peer-trust policy, including CN and identity correlation.
5. Independent testkit TLS peer and RSA fixture.
6. TLS 1.2 and 1.3 configs and TLS-only tests.
7. Framed TLS link, hello and identity re-exchange.
8. Link manager limits, dedupe, rate limits, keepalive and shutdown.
9. Daemon composition and startup ordering.
10. Docs, CI and real-client P1 interop.

Active work:

- Branch: `feat/store-identity`
- Owner: store identity worker, to be spawned
- PR: pending
- Verifier: pending
- Evidence: store tests for `identity_v1` round-trip and persistence, a pre-audit `identity_v1` migration, a 0.2.0 state file opening cleanly, peer pin preservation through `record_discovery`, no `Debug` on the private-key record, and all 9 checks.

Provider follow-ups:

- The provider test comment should say Windows builds the dependencies, not the test binary.
- Remove the direct `rustls-pki-types` dependency if the identity module does not use it.

Design risks carried into implementation:

- Real Android TLS 1.2 and desktop TLS 1.3 interop, including an RSA peer, is unverified until the P1 exit.
- The UDP broadcast test assumes the CI kernel delivers a broadcast to two reuse-addr sockets.
- Pairing clock skew, Windows state-file ACLs and private-key zeroization remain open.
- The unpaired idle timeout must not close a link inside the 30-second pairing window.
- The testkit TLS peer duplicates handshake logic on purpose.
