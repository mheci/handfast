# Contributing to Handfast

## Ground rules

1. **Conventional commits.** `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
   `chore:`, `perf:` — the release pipeline derives versions and changelogs
   from these. Breaking changes: `feat!:` with a `BREAKING CHANGE:` footer.
2. **No `unwrap()` / `expect()` / bare `panic!` in production code paths.**
   Tests may use them freely. Clippy denies warnings in CI.
3. **Wayland-first.** Never add X11/XWayland/XTEST dependencies for primary
   functionality. Use native Wayland protocols, then D-Bus, then XDG portals.
4. **Event-driven only.** No polling loops; use readiness-based I/O, watches,
   or subscriptions. Any unavoidable poll needs a written justification.
5. **Every network-facing parser gets a fuzz target** under `fuzz/`.
6. **Atomic persistence.** All config/state writes go through
   `handfast_core::store::atomic_write`.

## Workflow

```sh
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo xtask dist        # completions + man pages smoke check
```

CI runs exactly this plus `cargo audit`, `cargo deny`, a release build,
`makepkg` validation in an Arch container, fuzz smoke tests, and a headless
weston Wayland smoke test.

## Crate boundaries

- `crates/core` must not depend on any other workspace crate.
- `crates/protocol` depends only on `core`-free primitives (bytes/serde).
- Only `crates/daemon` wires crates together; GUI (`crates/gui`) and TUI
  (`crates/tui`) speak exclusively through `crates/ipc`.
- Linux-only code lives behind `#[cfg(target_os = "linux")]` with a compiling
  shim elsewhere so Windows/macOS builds still typecheck.
