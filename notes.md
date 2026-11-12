# Handfast production program

Goal: make Handfast production ready, through P1 to P4 in `docs/ARCHITECTURE.md`.

Current state:

- `v0.2.0` is the latest stable release. It has native Debian 12, Ubuntu 24.04 and Arch Linux packages plus an unsigned Windows MSI.
- `main` is the P0 foundation plus the first P1 work.
- The old README history was purged. README now has one commit.

Program rules:

- One owner per PR. The owner is the only writer of its branch and carries the PR from build to merge.
- One verifier per PR, never the author. The owner merges only after the verifier passes the current head.
- No direct pushes to `main`. No release tag until the P1 exit criteria pass.
- Every unit names its evidence before work starts.
- The coordinator owns this file and the program plan. Workers do not edit it.

P1 plan:

1. Discovery wired into `handfastd`. In flight.
2. TLS link, certificate persistence and identity re-exchange.
3. Pairing prompt and persistent trust.
4. TCP link and ping end to end.
5. Battery and share/file payload transfer.
6. Testkit integration and security tests.

P2 to P4 follow the parity matrix in `docs/PARITY.md`.

Active work:

- Branch: `feat/discovery-daemon`
- PR: pending
- Verifier: pending
