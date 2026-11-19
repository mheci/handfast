# Production-readiness audit

This is a working triage, not a certification. Baseline: parity branch
`98e272aacc73bb9b0a39fcf9041335c56a44fc54` (PR #272). Its prior CI
reports green; that does not establish real-device or production readiness.

## Confirmed defects addressed in this audit

- Clipboard: an inbound write caused the next poll to be discarded even when
  the user copied different text before that poll. Compare the observed value
  with the last applied value instead; unchanged inbound text is still not
  echoed. Recopying a previous value after an empty selection is also handled.
- Incoming transfers: a fast transfer could finish before its queued progress
  was observed, causing a missing progress update and intermittent regression
  failure. A final best-effort byte count is now emitted before completion.
- Notifications: repeated updates and cancelled IDs accumulated indefinitely
  in per-device cleanup state. Track unique live IDs, remove cancelled IDs and
  bound the set to 256 per device. New IDs beyond the limit are refused with a
  warning until a slot is freed; existing IDs can still be updated.

## Priority gaps and verification gates

1. **Real interoperability:** `docs/INTEROP.md` identifies the unpassed real
   Android and desktop discovery, TLS, pairing, reconnect and transfer gate.
   Run it on physical peers with firewall, permissions, logs and packet capture.
   Fixtures cannot substitute for this.
2. **Native interfaces:** the current frontend is `eframe`/egui, not WinUI 3
   on Windows or GPUI/Wayland on Linux. Design and implement platform frontends
   against the existing local control protocol, then verify accessibility,
   DPI, localization, keyboard navigation and actual packaging on each OS.
3. **Missing and partial features:** use `docs/PARITY.md` as the starting
   inventory. Four roster entries are planned: `mmtelephony`,
   `sendnotifications`, `shareinputdevices` capture and `virtualmonitor`.
   Implemented entries can still lack capabilities (e.g. notification actions,
   MMS/attachments, album art, SFTP multi-path and Windows pen pressure).
   Confirm support against the actual Android and desktop clients rather than
   assuming all desktop plugins have Android counterparts.
4. **Security and reliability:** review trust establishment, unpairing, local
   control IPC authorization, file/command/input boundaries, untrusted packet
   limits, upgrade integrity and recovery under faults. Unix self-update relies
   on HTTPS and a checksum from the same release, not an independent signature.
   Notification disconnect cleanup is subject to the runtime's five-second
   per-plugin deadline and needs further fault/load testing.
5. **Release gates:** run the full locked test and lint matrix, Windows runtime
   tests, GUI rendering/accessibility tests, installer upgrade/uninstall tests,
   signed-update tests, long-running load and resource tests, fuzzing and an
   independent security review. Track supported OS/client versions and a
   reproducible support/release process. Certification requires explicit
   acceptance criteria and evidence; passing fixture tests alone is insufficient.

The audit is ongoing. This list is not a claim that the entire codebase has
been inspected or that these are all defects.

## Unmaintained time dependency

`cargo-deny` reported RUSTSEC-2026-0543 for `chrono`, a transitive dependency
of `russh-sftp` and `russh-util`. This is an unmaintained warning, not by itself
an exploitable vulnerability finding. Because neither upstream release has a
replacement yet, narrowly patched local copies remove the dependency without
ignoring the advisory. Provenance, licenses, changes and retirement criteria are
recorded in `vendor/README.md` and issue #274. These forks need active security
monitoring and should be replaced once maintained upstream releases are ready.
