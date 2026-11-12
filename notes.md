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
- Owner: background session `ses_e311577f8ffezU3Jp14F7T6jrT`
- PR: pending
- Verifier: pending
- Evidence: CI `test` job plus the new `crates/handfast-daemon/tests/discovery.rs` assertions on the store record.

Verifier checklist for the discovery unit:

- Read the PR diff, not only the owner summary.
- Confirm `handfastd` binds UDP discovery, announces, receives a peer identity and upserts a device record.
- Confirm the integration test starts the real binary, sends a real announcement, restarts the store and asserts the record. A type check is not enough.
- Confirm `--print-identity` now advertises `tcpPort` 1716 and the CLI test asserts it.
- Confirm no TLS, pairing, TCP listener, payload, version bump, release workflow or `notes.md` changes.
- Confirm the README and architecture text match the new behavior and that the 0.2.0 release is described as predating discovery.
- Check the CI run at the current head. Post a pass or a concrete failure. Do not edit or merge.

Next after this merge: TLS link design with the architect skill, then pairing, then the TCP link.
