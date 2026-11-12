# Handfast production program

Goal: make Handfast production ready, through P1 to P4 in `docs/ARCHITECTURE.md`.

Current state:

- `v0.2.0` is the latest stable release. It has native Debian 12, Ubuntu 24.04 and Arch Linux packages plus an unsigned Windows MSI.
- Discovery landed on `main` in `6f85e3f` with an independent verifier pass. `handfastd` announces, receives peers and persists devices without clearing pairing.
- The old README history was purged. README now has one commit.

Program rules:

- One owner per PR. The owner is the only writer of its branch and carries the PR from build to merge.
- One verifier per PR, never the author. The owner merges only after the verifier passes the current head.
- No direct pushes to `main`. No release tag until the P1 exit criteria pass.
- Every unit names its evidence before work starts.
- The coordinator owns this file and the program plan. Workers do not edit it.

P1 plan:

1. Discovery wired into `handfastd`. Done in `6f85e3f`.
2. TLS link, certificate persistence and identity re-exchange. Design in flight.
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

Active work:

- Branch: `design/tls-link`
- Owner: architect design worker, to be spawned
- PR: pending
- Verifier: pending
- Evidence: a design package with usage sketch, module map, type signatures, certificate and trust model, failure modes, test plan and sequencing, grounded with `how` and sketched with `architect`/`arena`. Independent verifier review before merge. Only one model is configured, so model diversity is not established.

Design scope for the TLS unit:

- TLS 1.3 with rustls, self-signed device certificates and pinned peer certificates.
- Certificate generation, persistence in the redb store, fingerprinting and identity re-add.
- TCP listener and connector, identity exchange over TLS, protocol-8 re-exchange validation, frame limits and link lifecycle.
- The pairing boundary is designed here but implemented in the next unit.
- Testkit and integration tests, including a wrong-certificate and identity-mismatch case.
