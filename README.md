# Handfast v2

Clean-room, Rust-native KDE Connect implementation for Linux desktops.

> **Status: foundation branch.** This branch is a rewrite in progress and is not
> usable as a client yet. The v1 implementation is archived at tag
> `v1-archive`. The target is full feature parity with the 31 plugins shipped by
> `kdeconnect-kde`, while staying wire-compatible with the official KDE Connect
> Android and iOS apps.

## Why a rewrite

v1 proved the hard part: KDE Connect discovery, TLS, pairing and payload transfer
can be implemented in Rust. It also accumulated stubs, partial features and a CI
that mixed Python and shell tooling with the Rust build. v2 keeps the lessons,
not the code:

- One workspace, one runtime, one plugin model.
- Every plugin declares its packet contract and its desktop backend.
- Rust owns the protocol, the daemon, the CLI, the tests and the release tooling.
- CI is a small number of deterministic Rust jobs, not a collection of
  best-effort lanes.

## Documentation

- [Parity matrix](docs/PARITY.md) - the 31 plugins, their packets, Linux
  backends, phases and test status.
- [Architecture](docs/ARCHITECTURE.md) - crate layout, runtime, plugin model,
  desktop adapters, security and CI.

## Workspace

```
crates/
  handfast-proto/    wire packets, framing, identity, certificates
  handfast-net/      UDP discovery, TCP links, pairing state machine
  handfast-runtime/  plugin trait, registry, per-device host, events, supervision
  handfast-plugins/  31-plugin roster and plugin implementations
  handfast-desktop/  Linux desktop adapter traits and mock backend
  handfast-store/    redb state, settings, migrations
  handfast-testkit/  independent Rust wire peer and golden vectors
  handfast-daemon/   handfastd binary
xtask/               Rust developer and release tasks
```

Planned crates: `handfast-ipc`, `handfast-cli`, `handfast-tui`, `handfast-gui`.

## Build and test

```sh
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo run -p handfast-testkit -- --self-test
```

MSRV is Rust 1.90. CI enforces it separately from stable.

## Protocol target

- KDE Connect protocol version 8.
- Packet framing: 4-byte big-endian length, JSON body, 1 MiB hard limit.
- UDP discovery on port 1716, TCP control and payload links in the 1714-1764
  range.
- TLS 1.3 with certificate pinning and protocol-8 identity re-exchange.

## License

MIT. Handfast speaks the KDE Connect wire protocol but shares no code with the
KDE projects and is not affiliated with KDE e.V.
