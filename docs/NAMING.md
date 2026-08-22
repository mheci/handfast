# NAMING.md — why "Handfast", and what is called what

This document is the canonical record of every user-visible name in the
project. The names below are a contract: they must appear exactly as written
in code, docs, packaging, and release automation. See also the naming
contract summarized in `CONTRIBUTING.md` and enforced by CI.

## Rationale

**Handfasting** is the old ritual of binding two hands together with a cord
or knot to mark a union. The metaphor maps one-to-one onto device pairing:

- *Binding two hands* = pairing your phone and your desktop.
- *A fast knot* = a connection that holds: reliable, supervised, quick to
  establish ("fast" is also an honest performance claim — see
  [ARCHITECTURE.md](ARCHITECTURE.md) for the budget we hold ourselves to).
- *Untying* = unpairing. The ritual knot can be undone; so can a pairing,
  cleanly and completely, with `hfctl unpair`.

The word is short, pronounceable, and verb-able ("handfast your phone"),
which makes it pleasant in CLI help text and completions.

## Uniqueness checks

Performed before adoption, current as of **2026-08**:

- **crates.io**: no crate named `handfast`, `handfastd`, or `hfctl`.
- **AUR**: no package named `handfast`.
- **KDE projects**: no product named Handfast (upstream is KDE Connect /
  kdeconnect-kde; no affiliation).
- **GNOME Circle / apps**: no conflicting app name.
- **GitHub**: no established project under the `handfast-rs` organization
  scope.

If any of these changes, this file must be updated before branding ships.

## Pronunciation

**HAND-fast** (two even syllables, stress on the first). It rhymes with
"stand fast".

## Naming table

| Artifact                | Exact name                          |
|-------------------------|-------------------------------------|
| Application / project   | `Handfast`                          |
| Daemon binary           | `handfastd`                         |
| CLI / TUI binary        | `hfctl`                             |
| GUI binary              | `handfast-gui`                      |
| systemd user unit       | `handfast.service`                  |
| Desktop entry id        | `dev.handfast.Gui.desktop`          |
| AUR / package name      | `handfast`                          |
| Control socket          | `$XDG_RUNTIME_DIR/handfast.sock`    |
| Config dir              | `~/.config/handfast`                |
| Data (state) dir        | `~/.local/state/handfast`           |
| Cache dir               | `~/.cache/handfast`                 |

## Branding vs wire identity

Handfast's branding is entirely local. On the network it is deliberately
boring: discovery uses the standard `_kdeconnect._udp` service and TCP/UDP
port **1716**, packet types are the upstream `kdeconnect.*` URIs, and
`ProtocolVersion` is 8 — see [PROTOCOL.md](PROTOCOL.md). Peers never see the
name "Handfast" as a protocol requirement; only the human-readable
`deviceName` field carries our branding. This keeps interop with KDE Connect
Android/desktop clients intact while letting the project keep its own name.
