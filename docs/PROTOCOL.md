# PROTOCOL.md — KDE Connect wire protocol notes

> **Status: Phase-1 draft.** Field layouts and packet semantics below are
> from the Phase-1 implementation study and **to be validated against
> upstream commit SHA in Phase 2** (kdeconnect-kde). Any divergence found
> during validation is recorded in the compatibility notes at the end.

## Transport overview

1. **Discovery (UDP).** Every few seconds each peer broadcasts a single JSON
   identity frame to UDP port **1716**. The frame is a length-prefixed
   `kdeconnect.identity` packet whose body includes the `tcpSourcePort`
   field telling listeners which TCP port to dial.
2. **Connection (TCP + TLS).** Peers connect to TCP port **1716** and
   immediately upgrade the raw socket to TLS. Both sides present self-signed
   device certificates. Trust is trust-on-first-use (TOFU): on first contact
   the SHA-256 fingerprint of the peer certificate is shown for verification
   and then **pinned per device**; later connections must match the stored
   fingerprint or the connection is rejected.

## Framing

Inside TLS, every packet is one frame:

```text
u32 BIG-endian byte count  ++  UTF-8 JSON packet object
```

The declared length is checked against a hard cap of **512 KiB**
(`MAX_PACKET_LEN`) before any payload memory is reserved. The JSON object
has exactly three fields:

```json
{"id": 42, "type": "kdeconnect.ping", "body": {}}
```

File transfers do not use this framing: they stream raw bytes over a
separate TLS connection negotiated via share packets.

## Identity packet fields

Body of `kdeconnect.identity`:

| Field                   | Type     | Meaning                                            |
|-------------------------|----------|----------------------------------------------------|
| `deviceId`              | string   | Stable unique identifier of the announcing device   |
| `name`                  | string   | Human-readable device name                          |
| `deviceType`            | string   | `phone`, `tablet`, `desktop`, ...                   |
| `ProtocolVersion`       | number   | Wire protocol version (`8` for this implementation) |
| `incomingCapabilities`  | string[] | Packet types the device can receive                 |
| `outgoingCapabilities`  | string[] | Packet types the device can send                    |
| `tcpSourcePort`         | number   | TCP port the announcer listens on for TLS sessions  |

## Packet-type catalogue

Every `TYPE_*` constant in `handfast-protocol` (`crates/protocol/src/lib.rs`),
with direction relative to *this* device:

| Constant                | Wire URI                                  | Direction    | Payload notes                                                        |
|-------------------------|-------------------------------------------|--------------|----------------------------------------------------------------------|
| `TYPE_IDENTITY`         | `kdeconnect.identity`                     | both         | Discovery broadcast; see field table above                           |
| `TYPE_PAIR`             | `kdeconnect.pair`                         | both         | `{pair: true}` requests/accepts, `{pair: false}` rejects/unpairs     |
| `TYPE_PAIR_REQUEST`     | `kdeconnect.pairingrequest`               | —            | Legacy pre-capability pairing alias; documentation only, never sent  |
| `TYPE_PING`             | `kdeconnect.ping`                         | both         | Empty body; liveness probe                                           |
| `TYPE_NOTIFICATION`     | `kdeconnect.notification`                 | both         | Mirrored notification; carries title/text/app, optional actions/reply |
| `TYPE_NOTIFICATION_REQUEST` | `kdeconnect.notification.request`     | out          | Cancel/dismiss or reply-to a remote notification                     |
| `TYPE_MOUSEPAD`         | `kdeconnect.mousepad.request`             | both         | Remote input: keypress, motion (dx/dy), click, scroll events         |
| `TYPE_MPRIS`            | `kdeconnect.mpris`                        | both         | Player state loop: properties (title/artist/pos/volume) + actions    |
| `TYPE_MPRIS_REQUEST`    | `kdeconnect.mpris.request`                | both         | Query players or command play/pause/seek/next                        |
| `TYPE_CLIPBOARD`        | `kdeconnect.clipboard`                    | both         | Clipboard text content updates                                       |
| `TYPE_CLIPBOARD_CONNECT`| `kdeconnect.clipboard.connect`            | both         | Capability exchange: announces clipboard sync participation          |
| `TYPE_SHARE`            | `kdeconnect.share.request`                | both         | Share `url`, `text`, or file-transfer metadata                       |
| `TYPE_RUNCOMMAND`       | `kdeconnect.runcommand`                   | both         | Advertised command list (id -> name/command)                         |
| `TYPE_RUNCOMMAND_REQUEST`| `kdeconnect.runcommand.request`          | out          | Execute one advertised command by key                                |
| `TYPE_BATTERY`          | `kdeconnect.battery`                      | both         | Report `level` (0-100) + `charging` flag                             |
| `TYPE_BATTERY_REQUEST`  | `kdeconnect.battery.request`              | out          | Ask peer for an immediate battery report                             |
| `TYPE_SFTP`             | `kdeconnect.sftp`                         | both         | startProcess / browse payloads for the remote filesystem view        |
| `TYPE_TELEPHONY`        | `kdeconnect.telephony`                    | in           | Ring/SMS/missed-call events from the phone                           |
| `TYPE_SYSTEMVOLUME`     | `kdeconnect.systemvolume`                 | both         | System volume reports and controls per sink/stream                   |
| `TYPE_FINDMYPHONE`      | `kdeconnect.findmyphone.request`          | both         | Ring a lost phone at full volume; repeat to cancel                   |

## Pairing state machine

```text
            identity seen (UDP)
   UNPAIRED ----------------------> DISCOVERED
       ^                                |
       | pair:{pair:false}      pair:{pair:true} sent
       |                                v
       +<-- reject --+             AWAITING_ACCEPT --- accept ---> PAIRED
                     |                  |                        |
                     +------------------+        pair:{pair:false} / timeout
                                                                  |
                                                                  v
                                                              UNPAIRED
```

- `DISCOVERED`: identity received but not paired. Pairing starts by sending
  `kdeconnect.pair {pair: true}`.
- `AWAITING_ACCEPT`: our request is pending, or an incoming request awaits a
  local user decision (surfaced over IPC). Requests time out.
- `PAIRED`: both sides accepted; the certificate fingerprint is pinned and
  persisted. Plugins may now exchange packets gated by capabilities.
- `pair {pair: false}` in any state tears down and unpairs symmetrically
  (the "untie the knot" path).
- Re-pairing after unpair requires a fresh user confirmation on both ends;
  the old pinned fingerprint is discarded first.

## Capabilities negotiation

Two devices may only exchange packets whose type appears in the intersection
of what each side advertises: receiver's `incomingCapabilities` against
sender's `outgoingCapabilities`. Handfast computes this once at pairing time,
stores it per device, and lets users override it per plugin locally
(`hfctl plugins <device>`); local overrides can only narrow, never widen,
the negotiated set.

## Compatibility notes

- **ECDSA certificates.** Upstream generates RSA-2048 device certificates;
  Handfast deliberately issues ECDSA P-256 instead (the `rcgen` default).
  Decision rationale: modern TLS stacks negotiate either side happily, the
  security property that matters comes from TOFU fingerprint verification
  rather than the key algorithm, and P-256 halves handshake cost on mobile
  radios. Recorded as an intentional, tested divergence.
- **Protocol version 8.** `ProtocolVersion: 8` is announced and required
  range-checked on incoming identities, matching current upstream peers.
- **Legacy alias.** `kdeconnect.pairingrequest` (`TYPE_PAIR_REQUEST`) is kept
  as a documentation-only alias so readers tracing upstream history are not
  confused; it is never emitted.
