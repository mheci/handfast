# One `handfast` binary: daemon, control client, service, update and GUI

Status: design package, revision 1. No production code in this unit.

Goal: ship one executable, `handfast` on Unix and `handfast.exe` on Windows, that
starts the daemon, talks to it, installs its own autostart, updates itself and
opens a window. A user installs one file and never types a second program name.

Non-goals:

- macOS. No asset, no service backend, no CI lane.
- Changing the wire protocol, the store schema or the state paths.
- A tray icon, a notification area menu or an always-running background GUI.
- Replacing the distro package manager on Linux.
- Rewriting the historical design docs under `docs/design/`. They record past
  decisions and stay as written.

## Problem

The product currently ships two executables and three package names for one
thing. The daemon binary is `handfastd`. The control client is `hfctl`, a second
`[[bin]]` in the same package that exists only to send one request. The package
is `handfast-daemon`, the MSI is `handfast-<version>-x64.msi`, and the deb is
`handfastd`. A user who installs the product gets a daemon with no way to see
whether it runs, no way to start it at logon, and no way to update it without
downloading an installer.

The composition root sits in `main.rs`, so nothing else can start the daemon.
`run()` parses `Args` itself, installs the tracing subscriber itself, and waits
on `tokio::signal::ctrl_c()` itself. A service entry, a GUI and a test all need
the same startup order with a different shutdown source, and none of them can
reach it today.

## Current system

| Item | Current behavior | Constraint for this design |
| --- | --- | --- |
| `crates/handfast-daemon/Cargo.toml` | One `[[bin]] name = "handfastd" path = "src/main.rs"`. | Becomes one `[[bin]] name = "handfast"`. |
| `src/main.rs` | 900+ lines. `Args` (clap), `run()`, `Daemon`, event loop, shutdown, helper tests. `run()` calls `Args::parse()` and `tracing_subscriber::fmt().init()`. | Keeps the CLI. The composition root moves to the library. |
| `src/bin/hfctl.rs` | 82 lines. `hfctl share --device <id> [--data-dir <dir>] <path>`. | Deleted. `handfast share` replaces it. |
| `src/lib.rs` | `pub mod control`, `pub fn default_data_dir()`. | Gains `DaemonArgs`, `run()` and the private daemon modules. |
| `src/control.rs` | `ControlRequest::Share` only. `ControlResponse::{Ok{bytes}, Err{message}}`. 4-byte length frame, 64 KiB cap. Unix socket with a peer-credential check, Windows pipe with a current-user DACL. | Gains `Status` and `Shutdown`. The frame and the same-user check do not change. |
| `src/control/windows.rs` | `PIPE_PREFIX = r"\\.\pipe\handfastd-"` plus the user SID. | Renamed to `handfast-`. |
| `control_path()` | `$XDG_RUNTIME_DIR/handfast/handfastd.sock`, else `<data_dir>/handfastd.sock`. | Renamed to `handfast.sock`. |
| `Daemon` | Holds `sessions: HashMap<DeviceId, DeviceSession>`, `pending`, `prompts`, `store`, `desktop`, `limits`. `DeviceSession` exposes `handle()`, `into_task()`, `deliver()`. It does not expose the peer identity. | Gains an in-memory `HashMap<DeviceId, DeviceSummary>` filled on `LinkEvent::Connected`. |
| `DaemonEvent` | `PluginEvents { device_id, events }`, `PairingAccepted`, `PairingRejected`. | The status transfer list is derived from `PluginEvent::ShareProgress` and its terminal siblings. |
| `Store` | `trust_table() -> Vec<(String, [u8;32])>`, `device(&str) -> Option<DeviceRecord>`. No list-all method. | `Status` reads the trust table and one record per paired id. |
| `SHUTDOWN_GRACE` | 5 s. `main()` calls `runtime.shutdown_timeout(SHUTDOWN_GRACE)`. | Unchanged. |
| Packaging | deb `handfastd`, Arch `handfastd-bin`, MSI `perMachine` to Program Files, one exe each. No service unit, no shortcut, no checksums. | Renamed, per-user MSI, user unit, portable assets, `SHA256SUMS`. |
| CI | Nine required jobs. `clippy`, `test`, `msrv`, `docs` and the Windows build use `--all-features`; `testkit-self-test` and `fuzz` do not. `windows` runs `cargo test -p handfast-daemon --all-features --bin handfastd identity::tests`. | The bin name changes. A `gui` job is added. `--all-features` leaves the Linux workspace jobs that must not need GUI libraries. |
| Release | `cargo build -p handfast-daemon`, four packages, no bare binary, no checksums. | Adds portable bare binaries and `SHA256SUMS`, renames the packages. |

## Decision 1: one binary named `handfast`

The package keeps the name `handfast-daemon`. The binary is `handfast`, so
Windows ships `handfast.exe`. The release publishes one executable per platform.
The native packages add a unit file, a shortcut and documentation around that
same executable.

Cargo already separates the two. A package rename would touch every
`-p handfast-daemon` in CI, the release workflow, four docs and two scripts, and
it would buy nothing a user can see. The package name is an internal identifier.
The binary name is the product surface. Keep the cheap one and change the
expensive one.

### Subcommands

| Subcommand | Replaces | Arguments |
| --- | --- | --- |
| `handfast daemon` | `handfastd` | `--incoming-dir`, `--tcp-port` (default 1716), `--discovery-port` (default 1716), `--log-file`, `--print-identity` |
| `handfast share` | `hfctl share` | `--device <id>`, `<path>` |
| `handfast status` | new | `--json` |
| `handfast identity` | `handfastd --print-identity` | `--tcp-port` (default 1716) |
| `handfast version` | new | none |
| `handfast service` | new | `install`, `uninstall`, `start`, `stop`, `status` |
| `handfast update` | new | `check`, `apply` |
| `handfast gui` | new | feature-gated |

`--data-dir` becomes a global option on the top-level command with
`global = true`. Clap then accepts both `handfast --data-dir <dir> daemon` and
`handfast daemon --data-dir <dir>`. Scripts get one spelling that works for
every subcommand. The alternative, one `--data-dir` per subcommand, repeats the
same argument four times and lets the spellings drift.

`handfast identity` prints the identity packet exactly as `--print-identity`
does today, including the `tcpPort` it is given. `docs/INTEROP.md` and both
interop scripts parse `body.deviceId` from that packet and read the bound port
from the daemon log, so the packet bytes stay and the log line stays. The human
summary, including the certificate fingerprint, is `handfast status`.

`handfast daemon --print-identity` stays as a deprecated spelling. It maps to
`DaemonArgs { print_identity: true }`, the same path `handfast identity` takes.
One implementation, two spellings. The documented form keeps working under
`handfast daemon`. The `handfastd` binary is deleted.

`handfast version` prints `handfast 0.3.0 (x86_64-pc-windows-msvc)`. The target
triple is what the updater uses to pick an asset, so a bug report that includes
the line answers the first question. `--version` keeps the clap default,
`handfast 0.3.0`.

### No subcommand

| Build | Behavior |
| --- | --- |
| `gui` feature on | Start the GUI. A double-clicked exe opens a window. |
| `gui` feature off | Print help to stderr, exit 2. |

The daemon is not the default. A typo in a shell should not start a process that
holds the store lock until the user finds it. A headless server build has no
window to open, so help is the only honest answer.

Windows builds keep the console subsystem in every configuration. A GUI-subsystem
exe cannot print `handfast status --json` to a pipe. When `handfast` runs with no
subcommand and the `gui` feature is on, the GUI calls
`GetConsoleProcessList`. If the process is the only one attached, it hides its
console window with `ShowWindow(GetConsoleWindow(), SW_HIDE)`. A launch from a
terminal keeps the console, so `handfast daemon` still logs to the terminal that
started it.

### Retiring `handfastd` and `hfctl`

One wave, per the migrate-callers-then-delete rule. The PR that adds `handfast`
deletes `src/bin/hfctl.rs`, renames the `[[bin]]`, updates both interop scripts,
and updates the docs that name either binary. No source-only alias binary, and
no symlink.

An alias `[[bin]]` compiles `main.rs` twice, doubles the build time for the same
code, and ships a second file the moment a packaging step copies
`target/release/*`. A symlink `/usr/bin/handfastd -> handfast` keeps old scripts
running against a CLI whose flags moved, so the failure arrives later and with
less context than `command not found`.

The control endpoint is renamed in the same PR. `handfastd.sock` becomes
`handfast.sock`, and `\\.\pipe\handfastd-<sid>` becomes
`\\.\pipe\handfast-<sid>`. Otherwise the retired name survives in the one place
a user sees it in a log. The rename is the break for a 0.2.0 client. Its
`hfctl` looks for the old path, so it cannot reach a 0.3.0 daemon.

Package migration replaces the old name rather than leaving it installed. The
deb control file carries `Conflicts: handfastd`, `Replaces: handfastd` and
`Provides: handfastd`. The Arch package carries `conflicts=('handfastd-bin')`
and `replaces=('handfastd-bin')`. The MSI keeps the 0.2.0 `UpgradeCode`, so later
per-user releases upgrade in place. The 0.2.0 per-machine install needs a
one-time manual uninstall first, for the reason in Decision 7.

The in-flight branch `feat/handfast-cli` keeps `handfastd` and `hfctl` as thin
wrappers. That branch reconciles to this decision before it lands. If the program
owner keeps the wrappers instead, this section changes with that call.

Rejected:

- A `handfast` crate that depends on `handfast-daemon`. One more package, one
  more `Cargo.toml`, one more version to keep in sync, for a `main` that is
  already in the same crate.
- Keeping `hfctl` as a second bin. It shares nothing with `handfast` except
  `send_request`, and that call is 20 lines in the CLI.
- Thin wrapper binaries named `handfastd` and `hfctl`. They keep the old commands
  working for one release, and they cost three binaries in every build, three
  files in `target/release`, and a `--help` that names a program the user did not
  run. A release that installs only `handfast` leaves the wrappers as dead code
  in the tree, and the release after that deletes them anyway.

## Decision 2: daemon library boundary

The library gains exactly two items:

```rust
pub struct DaemonArgs {
    #[arg(skip)]
    pub data_dir: Option<PathBuf>,
    pub incoming_dir: Option<PathBuf>,
    pub tcp_port: u16,
    pub discovery_port: u16,
    pub log_file: Option<PathBuf>,
    pub print_identity: bool,
}

pub async fn run(
    args: DaemonArgs,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn Error + Send + Sync>>;
```

`DaemonArgs` keeps its clap derive and lives in the library. A second struct in
the CLI plus a conversion would be two sources of truth for the same six flags,
and the first time someone adds a flag to one and not the other the service entry
silently ignores it.

`--data-dir` has one owner, the global option on the top-level `Cli`. The field
in `DaemonArgs` is `#[arg(skip)]`, and `main()` copies the global value into it
before it calls `run`. A `--data-dir` declared both globally and on the `daemon`
subcommand would give clap two options with the same long name, which panics at
startup.

`--log-file` is the one new flag. An autostarted daemon has no console, so its
stderr goes nowhere and a Windows install would have no diagnostics at all. When
the flag is set, `main()` adds a second tracing layer that writes the same lines
to that file. On start, a log file over 5 MiB rotates to `<name>.1`. The service
entry resolves the path, so the task XML carries no environment variable that
Windows may not expand.

The subscriber moves to `main()`. `run()` must not install a global subscriber,
because the GUI installs one that also writes a file, and tests would panic on a
second `init()`. `main()` initializes tracing before it builds the runtime, for
every subcommand that logs.

`shutdown` is a future, not an `Option`. A `None` would describe a daemon that
cannot be stopped, and every caller has a signal to give. The loop replaces its
`tokio::signal::ctrl_c()` arm with `shutdown`. Each caller supplies the source
that fits it:

| Caller | Future |
| --- | --- |
| CLI | `ctrl_c` plus `SIGTERM` on Unix |
| Linux service stop | the same CLI path, because `systemctl --user stop` sends `SIGTERM` |
| GUI | a `oneshot::Receiver` fed by the GUI's Stop button |
| Test | a `oneshot` the test fires |

The `SIGTERM` arm is a fix, not a refactor. Without it, `systemctl --user stop
handfast` waits for `TimeoutStopSec` and then sends `SIGKILL`, because the daemon
only selects on `SIGINT`. A user unit that ignores its stop signal is not
production ready.

The error type gains `Send + Sync`. `run()` today returns `Box<dyn Error>`, which
cannot cross a `tokio::spawn`. A caller that spawns the composition root, and any
test that boxes it, needs the bound. Two helpers in `identity.rs` also return
`Box<dyn Error>` and must change with it.
`identity::load_or_create_device_id` and
`identity::load_or_generate_tls_identity` return
`Result<_, Box<dyn Error + Send + Sync>>` after PR 1. That is a real signature
change in two files, not a claim that every site already satisfies the bound. If
another error type does not satisfy it, the PR reports the site instead of hiding
it with a wrapper.

### Startup and shutdown order

`run()` keeps the order that exists today, line for line:

1. Resolve `data_dir`, check ownership, create it, secure it.
2. Open the store, secure `state.db`, load or create the device ID.
3. Build the default desktop, the registry, the capabilities and the device name.
4. Return early when `print_identity` is set, before any TLS material exists.
5. Load or generate the TLS identity.
6. Bind the listener in 1716-1764 and log the actual port.
7. Build `LocalFiles`, sweep stale temp files, rebuild the desktop with `files`,
   recompute the capabilities.
8. Build `PayloadService` on the same `TlsIdentity`.
9. Build the local endpoint, load the trust table, start the link manager.
10. Bind discovery and start the announce task.
11. Start the pairing task.
12. Start the control server.
13. Run the event loop.
14. Drop the receivers, stop the control server, run `Daemon::shutdown`.

The move is a cut and paste plus the two parameters. A reorder would change which
port the identity advertises and whether `--print-identity` creates files, and
the existing tests pin both.

The `--print-identity` branch keeps its behavior. It opens the store, writes the
device ID when absent, and returns before the TLS identity, the incoming
directory and the control socket. `docs/INTEROP.md` documents that, and the TLS
design added a test that asserts no `identity_v1` record is written.

### Module layout after the move

| File | Contents |
| --- | --- |
| `src/lib.rs` | `pub mod control`, `pub use` of `DaemonArgs` and `run`, `pub fn default_data_dir`, private `mod identity`, `mod limits`, `mod pairing`, `mod redact`, `mod session` |
| `src/main.rs` | `clap` types, `main()`, subscriber setup, runtime setup, `gui` dispatch |
| `src/bin/hfctl.rs` | Deleted in the CLI PR |

The private modules stay private. The CLI reaches `identity` through
`DaemonArgs { print_identity: true }`, and the GUI reaches the daemon through
the control channel, so neither needs a module handle.

Rejected:

- A `Daemon` type with `start`, `stop`, `status` and `wait`. The GUI talks to the
  control channel and the service talks to the process, so nobody calls the extra
  methods. Two public entry points for one lifecycle is a second state machine.
- An in-process GUI. Covered in Decision 4.

## Decision 3: per-user autostart, not a system service

The daemon runs as the logged-in user, started at logon by a per-user mechanism.

A SYSTEM service or a root systemd unit is wrong for five reasons, and each one
is a user-visible failure:

1. **The plugins need the session.** Clipboard uses `wl-paste` and `xclip`, which
   need `WAYLAND_DISPLAY` or `DISPLAY`. Notifications use `notify-send` on the
   session bus. MPRIS uses `playerctl` on the same bus. Volume uses `wpctl` on
   the PipeWire socket. Shell uses `xdg-open`. Screensaver uses
   `systemd-inhibit`. A session-0 service has none of those variables, so every
   backend reports unavailable and the identity advertises fewer capabilities
   than the same install run by hand.
2. **The state path is per user.** `%LOCALAPPDATA%\handfast` and
   `$XDG_DATA_HOME/handfast` resolve to the service account's profile, not the
   user's. The service would pair devices into a database the user's
   `handfast status` never opens, and the user would see "pairing was lost".
3. **The private key belongs to the wrong principal.** A SYSTEM-owned state file
   is either unreadable by the user or has to grant the user read access to a key
   SYSTEM also holds. Both are worse than one user owning one key.
4. **The incoming folder is the user's Downloads.** A SYSTEM write creates files
   the user may not be able to delete without elevation.
5. **The prompts and toasts land in a session nobody sees.** A firewall prompt, a
   pairing prompt and a notification all need the interactive desktop.

`kdeconnectd` is a per-user daemon for the same reasons. Matching it is the
compatible behavior.

### Windows: a Task Scheduler logon task

| Property | Value |
| --- | --- |
| Name | `\Handfast` in the user's task folder |
| Trigger | logon of the installing user, interactive token |
| Action | the resolved `...\Handfast\handfast.exe daemon --log-file <resolved log>` |
| Run level | least privilege |
| Multiple instances | ignore new |
| Execution time limit | none (`PT0S`) |
| Restart on failure | 3 attempts, 1 minute apart |
| Battery settings | start and continue on battery |

`schtasks /Create` flags cannot set an execution time limit, the battery
settings or a restart policy. `service install` writes an XML definition to a
temp file and registers it with `schtasks /Create /XML <file> /TN Handfast /F`.
The `/F` makes a second install a no-op.

```xml
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>S-1-5-21-0000000000-0000000000-0000000000-1001</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-21-0000000000-0000000000-0000000000-1001</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>C:\Users\me\AppData\Local\Programs\Handfast\handfast.exe</Command>
      <Arguments>daemon --log-file "C:\Users\me\AppData\Local\handfast\handfast.log"</Arguments>
    </Exec>
  </Actions>
</Task>
```

`service install` substitutes the current user's SID for `UserId`, the resolved
exe path for `Command`, and the resolved log path for `Arguments`.
`InteractiveToken` runs the task only while that user is logged on, in that
session, and stores no password. `PT0S` means no execution time limit.

`schtasks /Delete /TN Handfast /F` removes it, and a missing task counts as
success.

A Task Scheduler task beats a Startup folder shortcut because it can restart on
failure, and because `schtasks /Run /TN Handfast` gives `handfast service start`
something to call. A shortcut cannot be started programmatically.

A Windows service running as the user is still a session-0 process. It is the
same wrong answer with more setup.

### Linux: a systemd user unit

```ini
[Unit]
Description=Handfast KDE Connect daemon
Documentation=https://github.com/mheci/handfast
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/bin/handfast daemon
Restart=on-failure
RestartSec=5
NoNewPrivileges=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6

[Install]
WantedBy=default.target
```

`WantedBy=default.target` starts the unit with the user manager at login.
`After=graphical-session.target` orders it behind the graphical session where
that target exists. The journal captures stderr, so the unit does not pass
`--log-file`.

The unit does not use `ProtectSystem=strict`, `PrivateTmp` or `ReadWritePaths`. A
user unit that needs the session bus, the user's home and `$XDG_RUNTIME_DIR`
cannot hide them. `ProtectSystem=strict` makes `$XDG_RUNTIME_DIR` read-only, so
the daemon cannot create the control socket under `$XDG_RUNTIME_DIR/handfast`,
and the home paths it writes would need a `ReadWritePaths` list that changes with
every custom `--data-dir` or `XDG_DOWNLOAD_DIR`. User-unit sandboxing is limited
here, and this design says so instead of carrying a setting that fails at
runtime.
The unit keeps the two restrictions it can enforce without breaking the product:
no new privileges and a restricted address-family set. The packaged binary and
the unit file are root-owned. A portable install under `~/.local/bin` is
user-owned, and its unit still runs as the user.

`RestrictAddressFamilies` matches the existing architecture promise. The daemon
needs `AF_UNIX` for the control socket, `AF_INET` and `AF_INET6` for discovery and
links.

### `handfast service`

| Command | Behavior | Exit |
| --- | --- | --- |
| `install` | Write the unit or task, enable it, start it. Second run is a no-op. | 0 |
| `uninstall` | Stop, disable, remove. Absent counts as success. | 0 |
| `start` | Start the unit or task. Already running is success. | 0 |
| `stop` | Send `Shutdown` over the control channel, wait up to 10 s, then `systemctl --user stop` or `schtasks /End` as the fallback. No daemon is success. | 0 |
| `status` | Print installed, running, pid and uptime. | 0 running, 1 stopped or not installed |

Every command reads the current state first and applies only the difference, so
a crash halfway through an install leaves a state the next run can finish. The
unit file is written to a temp path in the same directory and renamed into
place.

`install` also checks the Windows firewall rule and prints the exact
`netsh advfirewall firewall add rule name=Handfast dir=in action=allow
program="<resolved exe>" enable=yes` command when it is missing, with the
resolved exe path in place of the placeholder. A logon task cannot answer the
Windows firewall prompt, so an install with no rule would start a daemon that no
phone can reach and give the user nothing to click. The check reads the rule
without elevation. Adding it needs an administrator, and the design does not ask
for one.

## Decision 4: GUI

The GUI is `eframe` and `egui` behind a `gui` feature on the `handfast-daemon`
package, off by default. Release builds enable `gui`, and a plain `cargo build`
is the headless build. `xtask build` and the release workflow pass
`--features gui`, so the shipped executable has the window.

```toml
[features]
gui = ["dep:eframe"]

[dependencies]
eframe = { version = "0.33", default-features = false, features = ["accesskit", "default_fonts", "glow", "wayland", "x11"], optional = true }
```

`eframe` 0.33 is the newest release whose `rust-version` is below the workspace
MSRV of 1.90. Releases 0.34 and later require Rust 1.92, and 0.36 and later
require 1.95, so taking one of those would force an MSRV bump in the same PR as
the GUI. The `glow` backend avoids the `wgpu` dependency tree. `accesskit` keeps
the window reachable by a screen reader.

The module is `#[cfg(feature = "gui")] mod gui;` inside the same crate. A
separate `handfast-gui` package would exist to hold one module and would add a
workspace member, a `Cargo.toml`, a dependency edge and a version to keep in
sync, without adding a boundary the feature does not already give.

### The CI consequence

`--all-features` turns the GUI on, and the GUI needs X11 and Wayland libraries at
build time. Four workspace-wide CI jobs (`clippy`, `test`, `msrv`, `docs`) pass
`--all-features` on `ubuntu-latest`, and the Windows build job passes it too.
`testkit-self-test` and `fuzz` already run without it. The Linux workspace jobs
must stay free of GUI libraries, so the design changes CI in the GUI PR:

- `clippy`, `test`, `msrv` and `docs` drop `--all-features`.
  `handfast-net`'s only feature, `test-support`, is enabled by the daemon's
  dev-dependency where the tests need it.
- The Windows build job keeps `--all-features`, so the GUI is compiled on Windows
  in every PR. No extra system package is needed there.
- A `gui` job runs `cargo clippy -p handfast-daemon --features gui --all-targets
  -- -D warnings` and `cargo test -p handfast-daemon --features gui --lib` on
  `ubuntu-latest`, after installing the X11 and Wayland development packages
  that the build reports. The PR records the final package list in the workflow.
- `deny.toml` already sets `all-features = true`, so `cargo-deny` starts
  checking `eframe`, `egui` and their dependencies in that PR. Both are MIT or
  Apache-2.0, inside the current allow list.

If dropping `--all-features` surfaces a dead-code warning in a test-only path,
the job adds `--features handfast-net/test-support` back. That is settled by the
first CI run, not by a guess here.

### Child process, not in-process

The GUI starts `handfast daemon` as a child and talks to it over the control
channel.

```rust
Command::new(std::env::current_exe()?)
    .arg("daemon")
    .spawn()
```

| Reason | Consequence |
| --- | --- |
| A daemon started by the logon task is not the GUI's child, and the GUI must still show and stop it. | Status and stop go through the control channel, which works for any owner. |
| A daemon crash must not close the window. | The window shows `Stopped` and offers Start. |
| Start and stop already mean "process lifecycle" for the service and the updater. | One model for the GUI, the task, the unit and the updater. |
| The GUI does not need `run()` to be `Send`, and does not own the daemon's shutdown. | The library surface stays two items. |

The GUI holds the `Child` handles it spawned and calls `try_wait()` on each
poll, so a Linux child does not become a zombie. It does not kill a child it did
not spawn. Stop is the `Shutdown` control request for every case.

### State

```rust
enum DaemonState {
    Unknown,
    Stopped,
    Running(Box<DaemonStatus>),
    Unreachable(String),
}
```

Poll `Status` every second while the window is visible, with
`ctx.request_repaint_after(Duration::from_secs(1))`. `Running` always carries a
status, so the window cannot render a running daemon with no data.

### Window content

| Area | Content |
| --- | --- |
| Header | `Running` or `Stopped`, version, uptime, Start and Stop buttons |
| Identity | Device name, device ID, fingerprint with a copy button |
| Incoming folder | The resolved path and an Open button |
| Devices | Paired devices with name, type and a connected marker |
| Transfers | One row per active transfer, bytes of total |
| Update | Check and Install buttons, the last result, and the reason for a refusal |

The Open button uses `Shell::open_url` with a `file://` URL on Linux. On Windows
it runs `explorer.exe` with the folder path, because `handfast-desktop` has no
Windows backend. That is the only platform branch in the window.

The update buttons call the same code as `handfast update`.

No file picker in v1. Sharing is not in the window's brief, and a native dialog
means a second GUI dependency (`rfd`) plus a per-platform dialog path. The
device row says to use `handfast share --device <id> <path>` and names the id.

`handfast gui` in a build without the feature exits 2 with "this build has no
GUI; build with --features gui".

Rejected:

- A separate `handfast-gui` crate. Named above.
- An in-process daemon. It would need `run()` to be `Send`, a shutdown channel,
  and a story for a daemon that panics inside the window process. The child
  process gives all three for one `Command`.
- A tray icon. It needs another platform integration and another dependency, and
  the window plus the autostart task already cover "start it and forget it".

## Decision 5: update

`handfast update check` reads the latest GitHub release.
`handfast update apply` replaces the running executable.

### Release assets

The release gains two bare binaries, one per supported target. Linux x86_64 and
Windows x64 are the supported platforms. There is no aarch64 asset, because the
release workflow has no aarch64 build job and an asset nobody builds is a broken
update path.

| Asset | Target |
| --- | --- |
| `handfast-x86_64-unknown-linux-gnu` | Linux x86_64 |
| `handfast-x86_64-pc-windows-msvc.exe` | Windows x64 |

A bare binary means the updater downloads exactly the bytes it installs. No
archive, no extraction dependency, and the SHA-256 covers the installed file
instead of an archive that contains it. The asset name carries no version,
because the release tag already carries it and two copies of a version can
disagree.

`handfast version` and the updater read the target triple from
`env!("HANDFAST_TARGET")`, which a four-line `build.rs` sets from Cargo's
`TARGET`. Hard-coding the triple per `cfg` pair would need a match with an arm
per platform and would be wrong the first time a musl target is added.

### Trust

| Step | Linux | Windows |
| --- | --- | --- |
| Transport | HTTPS only, final host in `github.com`, `api.github.com`, `objects.githubusercontent.com`, `*.githubusercontent.com` | same |
| Integrity | SHA-256 from `SHA256SUMS` in the same release | same |
| Authenticode | not applicable | `WinVerifyTrust` on the downloaded file must succeed |
| On any failure | refuse, keep the current exe, print the reason | same |

The Windows check refuses an unsigned asset. The 0.3.0 release must be signed
for the Windows updater to work at all. An unsigned 0.3.0 installs and runs, and
refuses to update. That is the correct trade: an updater that runs an unsigned
binary over TLS is a remote code execution path with a checksum for decoration.

Linux has no signature in the pipeline. The checksum is the only integrity
check, and an attacker who can replace the asset can replace `SHA256SUMS` in the
same release. TLS to `github.com` is the only barrier. The residual risk is named
here and listed as an open question. Adding minisign to the release is a
follow-up, not part of this design.

The HTTP client is `ureq` with its rustls backend, not `reqwest`. The check is
one blocking GET on a background thread, and `ureq` pulls a fraction of
`reqwest`'s dependency tree. `cargo-deny` sets `multiple-versions = "warn"`, so a
rustls version that does not unify with the workspace shows up in the deny job
rather than in the binary size.

### Version comparison

Tags are `vX.Y.Z`. Parse three `u64` and compare. A tag with a `-` suffix
compares by its numeric triple only, so `v0.3.0-alpha` is not newer than
`v0.3.0`. That is a 15-line function with no dependency. Pre-release ordering is
not worth a `semver` dependency until the project ships pre-releases.

### Atomic replace

1. Resolve `std::env::current_exe()` and its parent. Keep both in variables; the
   path stops existing halfway through.
2. Create `.handfast-update-<pid>` in that directory. A permission error is the
   refusal for a package-managed install, and the message names the package
   manager command.
3. Download the asset into the temp file, capped at 64 MiB and 60 s.
4. Download `SHA256SUMS`, find the line whose file name equals the asset name,
   and compare the hex. A missing line, a duplicate name or a mismatch refuses.
5. On Windows, verify Authenticode on the temp file.
6. On Unix, set the temp file to 0755.
7. Send `Shutdown` and wait up to 10 s for the control endpoint to stop
   answering.
8. Remove `handfast.old` when present. If the remove fails, refuse with "close
   the previous Handfast process and retry".
9. Rename `handfast` to `handfast.old`.
10. Rename the temp file to `handfast`.
11. Start the daemon again if it was running before the update.
12. Delete `handfast.old`. A failure is fine; the cleanup at the next start
    retries.

Every `handfast` start removes a leftover `handfast.old` when it can, so a crash
between 9 and 12 leaves a state the next run finishes.

### The Windows running-exe rename

The Windows loader opens a process image with `FILE_SHARE_DELETE`. A running
`handfast.exe` can be renamed within the same volume, because the loader holds a
section on the file, not an exclusive lock. It cannot be deleted or overwritten
while it runs.

The replace therefore renames the running file away and renames the new file
into the freed name. The running process keeps executing its mapped image from
`handfast.old`, and the `.old` file cannot be deleted until that process exits.
This is why step 12 is best effort and why the next start cleans up.

Two consequences follow. `current_exe()` still reports the original path after
the rename, so step 10 must use the saved target path, not `current_exe()`. And
antivirus software can hold the file for a moment, so the rename retries three
times, 200 ms apart, before it refuses.

When the GUI runs the update, the GUI is also `handfast.exe`. It starts
`handfast update apply` as a child, and on success it closes itself. The next
launch runs the new code. A terminal update restarts only the daemon.

### Refusals

`update apply` refuses, and says why, when any of these is true:

- the current version is not older than the latest
- the asset for the target triple is missing
- the `SHA256SUMS` line is missing or mismatched
- the Windows signature is missing or invalid
- the exe directory is not writable, with the package manager command
- the daemon does not stop within 10 s
- `handfast.old` cannot be removed

`update check` prints `up to date (<version>)` or
`update available: <current> -> <latest> (<asset>)` and exits 0 in both cases.

Rejected:

- Downloading the MSI and running `msiexec`. It needs elevation on every update
  and it makes the updater a second installer. The bare binary keeps the update
  in the user's own directory.
- A tarball or zip asset. It adds an extraction dependency and puts a second
  layer between the checksum and the installed bytes.
- `reqwest`. Too much dependency tree for one GET.
- `semver`. Pre-release ordering is not in use.

## Decision 6: control status and shutdown

Two requests join `Share` on the existing channel.

```rust
pub enum ControlRequest {
    Share { device_id: DeviceId, path: PathBuf },
    Status,
    Shutdown,
}

pub enum ControlResponse {
    Ok { bytes: u64 },
    Status { status: Box<DaemonStatus> },
    ShuttingDown,
    Err { message: String },
}
```

`DaemonStatus` carries what the window and the CLI print:

```rust
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub identity: IdentitySummary,
    pub incoming_dir: PathBuf,
    pub connected: Vec<DeviceSummary>,
    pub paired: Vec<DeviceSummary>,
    pub transfers: Vec<TransferSummary>,
    pub truncated: bool,
}

pub struct IdentitySummary {
    pub device_id: DeviceId,
    pub device_name: String,
    pub fingerprint: String,
    pub tcp_port: u16,
}

pub struct DeviceSummary {
    pub device_id: DeviceId,
    pub device_name: String,
    pub device_type: String,
    pub connected: bool,
}

pub struct TransferSummary {
    pub device_id: DeviceId,
    pub transfer_id: u64,
    pub bytes: u64,
    pub total: Option<u64>,
}
```

The boxed `Status` variant keeps `ControlResponse` small, which is what
`clippy::large_enum_variant` asks for. The frame on the wire is the same JSON.

### Where each field comes from

| Field | Source |
| --- | --- |
| `version` | `env!("CARGO_PKG_VERSION")` |
| `pid` | `std::process::id()` |
| `uptime_secs` | an `Instant` captured at the top of `run()` |
| `identity` | the `LocalEndpoint` identity plus the TLS fingerprint already logged at startup |
| `incoming_dir` | the resolved path `run()` used for `LocalFiles` |
| `connected` | a new `HashMap<DeviceId, DeviceSummary>` filled on `LinkEvent::Connected` and cleared on `LinkEvent::Disconnected` |
| `paired` | `store.trust_table()` plus one `store.device(id)` per id |
| `transfers` | a reducer over `PluginEvent::ShareProgress`, `ShareCompleted` and `ShareFailed` |

`DeviceSession` does not expose its peer identity, and the daemon already sees
the full `Identity` in `LinkEvent::Connected`. Keeping the summary in the daemon
costs one map and avoids widening the session's public surface.

Transfers are derived from the event stream the daemon already receives. A
`ShareProgress` inserts or updates `(device_id, transfer_id)`. `ShareCompleted`
and `ShareFailed` remove it. No new plumbing in `session.rs`. The limit is that a
transfer which never emits a progress event, because it is smaller than the 4 MiB
progress step, never appears. The window shows bytes, not direction, because
`ShareProgress` does not carry one. Both limits are named and neither blocks the
window's job, which is to show that something is happening.

`paired` and `connected` are capped at 128 entries each and set `truncated`. A
frame body over 64 KiB fails at `write_frame`, which would break the window with
a clear but unhelpful error. The cap is three lines and turns that into a flag.

The status handler runs in the daemon event loop and reads the store through the
existing `run_blocking` path. A status request reads at most 129 small records,
and the loop answers one control request at a time.

`Shutdown` replies `ShuttingDown` and then completes the daemon's shutdown
future. The wiring is one internal `oneshot`. `run()` combines the caller's
shutdown future with that receiver in one `tokio::select!` arm, and
`DaemonCommand::Shutdown` fires the sender. It exists because the GUI and
`handfast service stop` must stop a daemon they did not start, and the
alternatives are `Child::kill` and `schtasks /End`, which skip the bounded
shutdown path the daemon already has.

`handfast status` prints the human form. One line per field, then one row per
device and one per transfer. `--json` prints `DaemonStatus` as JSON.

### Same door, same check

`Status` and `Shutdown` go through the existing `serve` and `dispatch`. They
inherit the Unix peer-credential check and the Windows pipe DACL. The design adds
no second listener, no TCP port, no token file and no GUI credential. Validation
stays at the boundary, and the request types carry already-typed values.

A same-user process can already kill the daemon, so `Shutdown` grants no new
capability. `Status` exposes device names and ids that the same user can read in
`state.db`. The fingerprint is public and already logged at startup. The private
key is never in a response.

Rejected:

- A local HTTP server for the GUI. It adds a second protocol, a port, an auth
  scheme and a firewall surface.
- The GUI opening `state.db` directly. Redb allows one writer, and a GUI that
  opens the store while the daemon holds it fails or forces the daemon out.

## Decision 7: packaging

### xtask

| Task | Behavior |
| --- | --- |
| `fmt` | Unchanged |
| `clippy` | Drop `--all-features`, so the GUI is not built on a host without GUI libraries |
| `test` | Drop `--all-features`, same reason |
| `doc` | Drop `--all-features`, same reason |
| `check` | `fmt`, `clippy`, `test`, `doc` |
| `build` | `cargo build --release --locked -p handfast-daemon --features gui` |
| `portable` | Copy the release binary to `dist/handfast-<target>(.exe)` |
| `deb` | Call `packaging/linux/make-deb.sh`, which builds the deb tree and calls `dpkg-deb --build` |
| `msi` | Call `candle` and `light` on `packaging/windows/handfast.wxs` |
| `checksums` | Write `dist/SHA256SUMS` over every file in `dist/` except `SHA256SUMS` itself |
| `package` | `portable` plus the host's native package; the deb needs `--glibc` and `--label` |

The `clippy`, `test` and `doc` tasks drop `--all-features` for the same reason the
CI jobs do. A developer without the X11 and Wayland packages can still run the
whole check suite. `xtask build` is the release build and enables `gui`, so the
portable asset and every package carry the window. The three Linux release jobs
install the same X11 and Wayland packages as the `gui` CI job before they run
`xtask build`. The Windows runner needs none.

`packaging/linux/make-deb.sh` stays and owns the deb tree. `xtask deb` calls it,
so one implementation builds the package. The Arch PKGBUILD stays, because
`makepkg` must run inside an Arch container and rewriting it in Rust buys
nothing.

### Packages

| Asset | Change |
| --- | --- |
| `handfast_<version>_debian12_amd64.deb` | Package `handfast`, `/usr/bin/handfast`, `/usr/lib/systemd/user/handfast.service`, `Conflicts`, `Replaces` and `Provides` on `handfastd` |
| `handfast_<version>_ubuntu24.04_amd64.deb` | Same |
| `handfast-bin-<pkgver>-1-x86_64.pkg.tar.zst` | `provides=('handfast')`, `conflicts=('handfastd-bin')`, `replaces=('handfastd-bin')`, the same unit |
| `handfast-<version>-x64.msi` | Per-user install to `%LOCALAPPDATA%\Handfast`, Start Menu shortcut, logon task, `CloseApplication` on the running exe |
| `handfast-<target>(.exe)` | Bare binary for the updater and for a portable install |
| `THIRD-PARTY-NOTICES.md` | The BSL-1.0, OFL-1.1 and Ubuntu-font-1.0 texts, also inside the deb, Arch package and MSI |
| `SHA256SUMS` | Every file above |

The MSI changes from `perMachine` to `perUser`. A per-machine install puts the exe
in Program Files, which the user cannot write, so `update apply` would need
elevation on every update. A per-user install makes the autostart task, the
state directory, the log file and the updater all live in one user's profile,
which is where the rest of the product already lives.

The cost is a one-time migration. Windows Installer does not support a per-user
product upgrading a per-machine product without elevation. The 0.3.0 release
notes and `docs/RELEASING.md` say to uninstall 0.2.0 once. The `UpgradeCode` stays
the same, so 0.3.0 and later upgrade in place.

The MSI runs `handfast.exe service install` as a custom action after the files
land, and `service uninstall` before removal. A per-user MSI runs both as the
user, so neither needs elevation. A failure in that action logs a warning and
does not fail the install, because a missing autostart is repairable and a failed
install is not. The MSI also installs the WiX `util` extension's
`CloseApplication` for `handfast.exe`, so an upgrade can replace a running binary.

### Docs and scripts

| File | Change |
| --- | --- |
| `README.md` | One binary, `handfast` subcommands, `service install`, the per-user paths |
| `docs/RELEASING.md` | New asset names, `SHA256SUMS`, the 0.2.0 uninstall note |
| `docs/INTEROP.md` | `handfast daemon`, `handfast identity`, `handfast share`, the new log and socket names |
| `docs/ARCHITECTURE.md` | Workspace table row for the daemon, the runtime model paragraph, the IPC section, the security unit settings, release packaging, migration |
| `scripts/README.md` | One binary, one `HANDFAST` override, `handfast.log` |
| `scripts/interop-share.sh` | `handfast daemon`, `handfast share`, one binary lookup, `handfast.sock` |
| `scripts/interop-share.ps1` | Same, and `handfast-<sid>` for the pipe |
| `notes.md` | Coordinator-owned. The design does not edit it. |
| `.github/workflows/ci.yml` | `--bin handfast`, the `gui` job, `--all-features` removal |
| `.github/workflows/release.yml` | `xtask build`, `xtask portable`, `xtask checksums`, the renamed packages |

### Version and state

The release that ships the one binary is `0.3.0`. The CLI, the package names and
the MSI scope all change, so a patch bump would misdescribe it.

The store schema does not change. The data directory does not change. A 0.3.0
daemon opens a 0.2.0 `state.db`, keeps the device ID and the certificate, and
reconnects to paired peers without a new prompt. That matters more than the
version number, because a regenerated identity would unpair every device.

## Decision 8: security

| Area | Rule |
| --- | --- |
| Update trust | HTTPS with a host allowlist, SHA-256 from the same release, Authenticode on Windows, refuse on any mismatch |
| Update privilege | Runs as the user, never elevated. A malicious update cannot gain administrator |
| Service account | The logged-in user. Task at `LIMITED`, systemd user unit. No stored password |
| State paths | Unchanged. `%LOCALAPPDATA%\handfast` and `$XDG_DATA_HOME/handfast`, 0700 and 0600 on Unix, protected current-user DACL on Windows |
| Control auth | The existing same-user socket and pipe. `Status` and `Shutdown` add no second door |
| GUI auth | The GUI uses the control channel. It never opens the store and never reads the key |
| Logs | The private key never leaves the store. `identity` prints the public packet. `status` prints the public fingerprint. The updater logs the asset name and checksum, never a token |
| Autostart binary | The task runs an exe in a user-writable directory. A same-user attacker can replace it, and that user can already run code. The elevation boundary does not move |
| Firewall | `service install` reports a missing rule and prints the administrator command. It does not elevate |

The update path is the only new trust boundary, and it is the only place the
design accepts a residual risk. On Linux, the checksum and TLS are the whole
check. That is stated in the open questions rather than hidden in a paragraph
about integrity.

## Decision 9: rollout

Seven PRs, in order. Each lands on `main` before the next starts, each keeps all
nine checks green, and each is verifiable on its own.

| PR | Scope | Automated evidence | Manual check |
| --- | --- | --- | --- |
| 1. Daemon library boundary | `run(args, shutdown)` in `lib.rs`, `DaemonArgs`, subscriber to `main`, `SIGTERM` on Unix, the two `identity.rs` helpers return `Box<dyn Error + Send + Sync>`, modules move. The binary is still `handfastd` | `cargo test -p handfast-daemon`. A test calls `run` with a oneshot shutdown and asserts it returns within 6 s. The `--print-identity` test asserts no `identity_v1` and no bound port | `handfastd`, then Ctrl+C. `handfastd`, then `SIGTERM` on Linux |
| 2. One binary, CLI | `[[bin]] handfast`, `daemon`, `share`, `identity`, `version`, global `--data-dir`, delete `hfctl.rs`, rename the endpoint, update scripts and docs | CLI parse tests, `handfast version`, `handfast identity` matches `daemon --print-identity` byte for byte, the control tests pass on the new endpoint name | `handfast daemon` plus `handfast share` against a phone |
| 3. Status and shutdown | `ControlRequest::{Status, Shutdown}`, `DaemonStatus`, the connected map, the transfer reducer, `handfast status [--json]` | A control round trip for `Status`, a status with zero devices, a status with a testkit peer, the transfer reducer with progress and completion, `Shutdown` returns and the daemon future completes | `handfast status` while running and while stopped |
| 4. Autostart | `handfast service`, the systemd unit, the task XML, the log file and rotation | The unit text renders exactly. The task XML contains `PT0S`, `InteractiveToken`, `LeastPrivilege` and `RestartOnFailure`. `install` twice is a no-op. A Windows CI test registers and deletes a uniquely named task from the XML | Log off and on with Windows. `systemctl --user enable --now handfast` on Linux. `service status` in both |
| 5. GUI | `gui` feature, `handfast gui`, no-subcommand rule, console hide, the `gui` CI job | The `DaemonState` reducer, the child command builder, `cargo test -p handfast-daemon --features gui --lib` | Open, Start, Stop, and a share from the CLI appears in the window |
| 6. Update | `handfast update check/apply`, asset selection, checksums, Authenticode, atomic replace, `.old` cleanup, `xtask portable`, `xtask checksums`, the portable assets in the release workflow | Version compare, asset select, sums parse, checksum mismatch refusal, replace and cleanup in a temp dir, `.old` cleanup on start | A portable install on Windows and Linux, updated from a draft release |
| 7. Packaging and version | `xtask build`, `xtask deb`, `xtask msi`, `xtask package`, deb, Arch, per-user MSI, unit install, docs, `0.3.0` | `xtask package`, `dpkg-deb -I` and `-c`, `makepkg` in the container, MSI install and uninstall | Install from each package, `service install`, one real update from 0.3.0 to the next build |

Rules for the sequence:

- PR 1 changes no user-visible behavior, so it can land while the manual interop
  gate is running.
- PR 2 is the only breaking change for scripts. It updates every caller in the
  same PR, which is why the scripts and docs are in its diff.
- PR 4 depends on PR 3 for `Shutdown` and on PR 1 for `SIGTERM`.
- PR 6 owns the portable assets and `SHA256SUMS`, so its manual check can use a
  draft release built by hand. PR 7 adds the native packages on top of the same
  binary.
- PR 7 is the only PR that bumps the version.

## Open questions and risks

1. Linux update integrity rests on TLS and a checksum published in the same
   release. A minisign signature in the release workflow is the fix. It needs a
   key in CI secrets and a public key in the binary, and it is out of scope here.
2. The Windows firewall rule needs an administrator. A logon task cannot answer
   the prompt, so a user who ignores the printed command gets a daemon that
   discovers nothing. The alternative is an elevated MSI, which the per-user
   install decision rejects.
3. The per-user MSI cannot upgrade the 0.2.0 per-machine MSI without elevation.
   The release notes carry a one-time uninstall instruction. If the 0.2.0 install
   base turns out to be large, the fallback is a final per-machine 0.3.0 MSI that
   installs only the binary, followed by the per-user 0.3.1.
4. Dropping `--all-features` from four workspace-wide CI jobs may surface a
   warning in a test-only path. The first CI run settles it. The fallback is an
   explicit `--features handfast-net/test-support`.
5. The exact `apt-get install` line for `eframe` on `ubuntu-latest` is not known
   here. The GUI PR records it from the first failing build.
6. `ureq`, `eframe` and `egui` must pass `cargo-deny`. All three are MIT or
   Apache-2.0. The deny job gates them like any other dependency.
7. The transfer list is derived from progress events, so a transfer smaller than
   the 4 MiB progress step never appears. A per-session counter would fix it and
   would add shared state to `session.rs`. Not worth it until a user reports it.
8. `handfast status` needs the daemon. A stopped daemon prints "handfast is not
   running" and exits 1. There is no offline mode that opens the store, because a
   second opener would fight the daemon for the redb lock.

## Sources consulted

- `crates/handfast-daemon/src/main.rs`, `lib.rs`, `control.rs`,
  `control/windows.rs`, `identity.rs`, `session.rs`.
- `crates/handfast-daemon/src/bin/hfctl.rs`.
- `crates/handfast-daemon/Cargo.toml`, `crates/handfast-runtime/src/event.rs`,
  `crates/handfast-store/src/lib.rs`, `crates/handfast-net/src/manager.rs`,
  `crates/handfast-desktop/src/lib.rs` and `files.rs`,
  `crates/handfast-proto/src/identity.rs`.
- `xtask/src/main.rs`, `packaging/linux/make-deb.sh`,
  `packaging/arch/PKGBUILD`, `packaging/windows/handfast.wxs`.
- `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `deny.toml`,
  `Cargo.toml`.
- `docs/ARCHITECTURE.md`, `docs/RELEASING.md`, `docs/INTEROP.md`,
  `docs/design/p1-tls-link.md`, `docs/design/p1-share-file.md`.
- `README.md`, `notes.md`, `scripts/README.md`, `scripts/interop-share.sh`,
  `scripts/interop-share.ps1`.
