# IPC.md — the Handfast local IPC contract (authoritative)

This document is the single source of truth for how local clients talk to
`handfastd`. The implementation lives in `crates/ipc`; if code and this
document disagree, fix one of them before shipping.

## Transport

| Property  | Value                                        |
|-----------|----------------------------------------------|
| Socket    | Unix domain socket at `$XDG_RUNTIME_DIR/handfast.sock` |
| Permissions | `0600`, owned by the daemon user            |
| Peer auth | `SO_PEERCRED`: same-uid enforcement on Linux; connections from any other uid are dropped before hello |
| Lifetime  | Created at daemon start, removed at shutdown |

## Framing

```text
u32 LITTLE-endian byte count ++ UTF-8 JSON message
```

Maximum frame: **16 MiB** (`MAX_FRAME_BYTES`). Frames declaring more are
rejected before buffering. Conversations are strictly request/response plus a
pushed event stream; pipelined bytes past the current frame are discarded.

## Hello / event envelope

On connect, the server immediately pushes:

```json
{"event": "hello", "data": {"version": 1, "app": "handfastd", "pid": 1234}}
```

All subsequent server-originated messages use the same envelope with an
`event` tag (see catalogue below). Client messages always carry a `method`
tag. The client must treat unknown events as ignorable, never fatal.

## Request methods

Serialized as `{"method": "<name>", "params": {...}}` with snake_case tags.
Params shown as object fields; "—" means none.

| Method                | Params                                  | Result                                                        |
|-----------------------|-----------------------------------------|---------------------------------------------------------------|
| `daemon_info`         | —                                       | `{version, app, pid, uptime_s}`                               |
| `device_list`         | —                                       | array of `{id, name, state, paired}`                          |
| `device_pair`         | `device_id`                             | `null` on success; pairing request dispatched                 |
| `device_unpair`       | `device_id`                             | `null` on success                                             |
| `plugin_list`         | `device_id`                             | array of `{plugin, title, enabled}`                           |
| `plugin_set_enabled`  | `device_id`, `plugin`, `enabled`        | `null` on success                                             |
| `send_file`           | `device_id`, `path`                     | `{transfer_id}`; progress arrives via `transfer_progress`     |
| `notification_list`   | —                                       | array of `{id, app, title, body}`                             |
| `notification_dismiss`| `notification_id`                       | `null` on success                                             |
| `clipboard_get`       | —                                       | `{"text": "..."}`                                             |
| `clipboard_set`       | `text`                                  | `null` on success                                             |
| `ping`                | —                                       | `null` (liveness probe)                                       |

## Response schema

Exactly one response per request:

```json
{"status": "ok",   "result": <json or null>}
{"status": "err",  "code": 2003, "message": "unknown device"}
```

Error-code ranges:

| Range    | Domain      | Notes                                    |
|----------|-------------|-------------------------------------------|
| 1000-1999| generic     | malformed frame, unknown method, internal error, io failure |
| 2000-2999| device      | unknown/unpaired device, pairing rejected, fingerprint mismatch |
| 3000-3999| plugin      | unknown plugin, invalid toggle state       |
| 4000-4999| transfer    | unreadable path, transfer refused/failed   |

Codes within a range are stable once released; new codes may be added.

## ServerEvent catalogue

Mirrors the core bus events one-to-one (see `handfast-core::bus`), plus the
hello frame:

| Event                 | Data                                                    |
|-----------------------|---------------------------------------------------------|
| `hello`               | `{version, app, pid}`                                   |
| `device_found`        | `{id, name}`                                            |
| `device_lost`         | `{id}`                                                  |
| `device_state_changed`| `{id, state}`                                           |
| `transfer_progress`   | `{id, bytes_done, bytes_total}`                         |
| `notification_received`| `{id, app, title, body}`                               |
| `clipboard_updated`   | `{text}`                                                |
| `log_record`          | `{level, msg}`                                          |
| `daemon_shutdown`     | `null` (final frame before close)                       |

## Versioning policy

- `hello.version` identifies the contract. **A bump is a breaking change**:
  it removes, renames, or reinterprets something above.
- Within v1, only additive changes are allowed: new optional fields in
  params/results/events, new method names, new event names. Clients must
  ignore both unknown fields and unknown variants.

## CLIENT RULES

1. **GUI (`handfast-gui`, Iced).** The GUI maps the `ServerEvent` stream to
   an Iced `Subscription` via `iced::stream::channel`, converting each frame
   into a GUI message. Requests are sent from update-loop side effects using
   the shared typed `Client`.
2. **Never touches the display server for connectivity.** The GUI performs
   no Wayland protocol work related to Handfast features — no input
   injection, no clipboard watching, no idle inhibition.
3. **TUI parity.** `hfctl tui` consumes the identical `handfast_ipc::Client`
   type. Any feature reachable from the GUI must be equally reachable from
   the CLI/TUI through the same request set.
4. **Wayland constraint (box).** The daemon is the **SOLE** Wayland client
   process in the Handfast suite. All compositor interaction — virtual input
   injection, clipboard watch/set, idle inhibit — happens inside `handfastd`
   behind `handfast-wayland`. Clients that need such capabilities must
   request them **through IPC only**; linking a Wayland library into a front
   end is a contract violation and will be rejected in review.
