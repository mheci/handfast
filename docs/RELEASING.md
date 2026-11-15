# Releasing

Releases are cut from a tag. The tag must match the workspace version in
`Cargo.toml`, for example `v0.3.0`.

## Assets

| Platform | Asset | Build host |
| --- | --- | --- |
| Debian 12, amd64 | `handfast_<version>_debian12_amd64.deb` | `rust:1.90-bookworm` |
| Ubuntu 24.04, amd64 | `handfast_<version>_ubuntu24.04_amd64.deb` | `ubuntu-24.04` |
| Arch Linux, x86_64 | `handfast-bin-<pkgver>-1-x86_64.pkg.tar.zst` | `archlinux:latest` |
| Windows 11, x64 | `handfast-<msi-version>-x64.msi` | `windows-latest` |
| Linux x86_64, portable | `handfast-x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| Windows x64, portable | `handfast-x86_64-pc-windows-msvc.exe` | `windows-latest` |
| All platforms | `THIRD-PARTY-NOTICES.md` | `ubuntu-latest` |
| Every file above | `SHA256SUMS` | `ubuntu-latest` |

The Debian package version replaces `-` with `~`. The Arch package version
replaces `-` with `_`. The MSI version drops any pre-release suffix. For
`0.3.0` every asset uses `0.3.0`.

Every job builds the same binary with `cargo run -p xtask -- build`, which is
`cargo build --release --locked -p handfast-daemon --features gui`. The Linux
jobs install the X11 and Wayland development packages first. The portable assets
are the same binary without a package around it; `handfast update` downloads the
asset for its target triple and checks it against `SHA256SUMS`.

## Signing secrets

The Windows job signs both `handfast.exe` and the MSI when both secrets are
present, then runs `signtool verify /pa /all`. Without them it builds an
unsigned MSI and the release still includes it. The 0.2.0 release is unsigned.
The 0.3.0 Windows updater refuses an unsigned asset, so self-update needs a
signed release. Set both secrets before tagging 0.3.0.

| Secret | Value |
| --- | --- |
| `WINDOWS_CERT_PFX_BASE64` | Base64 of a code-signing PFX with the private key and certificate chain |
| `WINDOWS_CERT_PASSWORD` | Password for the PFX |

The certificate must chain to an authority that Windows trusts. A self-signed
certificate will fail verification in CI.

Create the base64 value on Windows:

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes("handfast.pfx"))
```

Or on Linux:

```sh
base64 -w0 handfast.pfx
```

Store the single-line result in `WINDOWS_CERT_PFX_BASE64`. The PFX is written
only to the runner's temporary directory and is removed by the `always()` step.

## Cut a release

1. Update `[workspace.package] version` in `Cargo.toml` and the `version` field
   of every path dependency in `crates/*/Cargo.toml`.
2. Refresh `Cargo.lock`, then run the normal local checks.
3. Add `docs/releases/<version>.md`. The publish job uses that file as the
   release body when it exists, and falls back to `--generate-notes`.
4. Commit and push. Wait for CI on `main` to pass.
5. Tag the commit and push the tag:

   ```sh
   git tag v0.3.0
   git push origin v0.3.0
   ```

6. The Release workflow checks the tag against `Cargo.toml`, builds the packages
   and portable assets, signs the Windows files when the secrets are present,
   writes `SHA256SUMS` and attaches every asset to the GitHub release.

A manual `workflow_dispatch` run builds and uploads run artifacts. Set
`sign_windows` to false to build an unsigned MSI without the signing secrets.
A tag push is also unsigned when the secrets are absent. The publish job only
runs on a tag push, so a manual run never publishes.

## Build the packages locally

```sh
cargo run -p xtask -- build      # target/release/handfast with the GUI
cargo run -p xtask -- portable   # dist/handfast-<target>(.exe)
cargo run -p xtask -- deb        # dist/handfast_<version>_debian12_amd64.deb
cargo run -p xtask -- msi        # dist/handfast-<version>-x64.msi
cargo run -p xtask -- package    # portable plus the host's native package
cargo run -p xtask -- checksums  # dist/SHA256SUMS
```

`xtask deb` calls `packaging/linux/make-deb.sh` and needs `dpkg-deb` and bash,
so it runs on Debian or Ubuntu. Pass `--glibc 2.39 --label ubuntu24.04` for the
Ubuntu package. `xtask msi` needs WiX 3.11 and resolves `candle.exe` and
`light.exe` from `WIX_BIN` when set, otherwise from `PATH`.

## Install

Debian 12:

```sh
sudo apt install ./handfast_0.3.0_debian12_amd64.deb
systemctl --user enable --now handfast
```

Ubuntu 24.04:

```sh
sudo apt install ./handfast_0.3.0_ubuntu24.04_amd64.deb
systemctl --user enable --now handfast
```

Arch Linux:

```sh
sudo pacman -U handfast-bin-0.3.0-1-x86_64.pkg.tar.zst
systemctl --user enable --now handfast
```

The Linux packages install `/usr/bin/handfast`, the systemd user unit at
`/usr/lib/systemd/user/handfast.service`, and the license and third-party
notices under `/usr/share/doc/handfast` or `/usr/share/licenses/handfast-bin`.
`systemctl --user enable --now handfast` starts the daemon at login.
`handfast service install` writes a unit for the current binary instead, which
is what a portable install under `~/.local/bin` needs.

Windows 11: open the MSI. It installs `handfast.exe` to
`%LOCALAPPDATA%\Handfast`, adds a Start Menu shortcut named `Handfast` that runs
`handfast gui`, and writes state to `%LOCALAPPDATA%\handfast`. Run
`handfast service install` once to register the logon task. It prints the
administrator command for the firewall rule. The MSI does not run that step: a
per-user install cannot add the firewall rule, and a custom action that failed
would fail the install.

```text
netsh advfirewall firewall add rule name=Handfast dir=in action=allow program="C:\Users\<you>\AppData\Local\Handfast\handfast.exe" enable=yes
```

A per-user install cannot add the rule, and a logon task cannot answer the
Windows firewall prompt. Until the rule exists the daemon runs but discovers no
peers.

## Upgrading from 0.2.0

Windows: uninstall the 0.2.0 MSI once, then install 0.3.0. Use Settings > Apps >
Installed apps > Handfast > Uninstall. Windows Installer cannot replace a
per-machine product with a per-user product without elevation, and both carry
the same `UpgradeCode`. 0.3.0 and later upgrade in place with no manual step.

Linux: `apt` and `pacman` replace `handfastd` with `handfast` in one step. The
deb carries `Conflicts`, `Replaces` and `Provides: handfastd`; the Arch package
carries `conflicts=('handfastd-bin')` and `replaces=('handfastd-bin')`. The data
directory and the store schema do not change, so the install keeps its device ID
and its pairings.

## Manual checks

The workflow cannot prove these, so run them on the release candidate:

- Install the MSI on Windows 11, open the Start Menu shortcut, confirm the
  window opens, run `handfast service install`, add the printed firewall rule as
  an administrator, confirm the logon task and the rule, then uninstall. Upgrade
  0.3.0 to the next build with the daemon running to exercise
  `CloseApplication`.
- `dpkg-deb -I` and `dpkg-deb -c` on both debs. Confirm `handfast`, the unit and
  the notices, and no `handfastd`.
- `makepkg` in an `archlinux` container. Confirm the same file list.
- `handfast update check` and `handfast update apply` from a draft release.
- The real Android and KDE Connect desktop run in
  [INTEROP.md](INTEROP.md).
